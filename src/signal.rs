//! Async-signal-safe capture of SIGTERM/SIGINT for the poll-loop supervisor.
//!
//! The handler only stores the signal number into an atomic; the supervise
//! loop polls it via [`take_pending`] on every iteration. This is a deliberate
//! deviation from the classic self-pipe pattern: a self-pipe exists to wake up
//! a loop that blocks on a file descriptor (`select`/`read`), but `run()` is a
//! poll loop that never blocks on one — there is nothing to wake. See
//! `docs/TECHNICAL_PLAN.md` (Этап 3) for the full rationale.

use nix::errno::Errno;
use nix::libc;
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use std::sync::atomic::{AtomicI32, Ordering};

/// Last signal delivered to the process; 0 means "no signal pending".
///
/// `Ordering::Relaxed` is sufficient: this is a standalone flag with no
/// associated data — nothing is published alongside the store, so no
/// acquire/release ordering with other memory accesses is needed.
static PENDING: AtomicI32 = AtomicI32::new(0);

/// The signal handler. It runs asynchronously, interrupting arbitrary code in
/// the process, so its body may contain only async-signal-safe operations.
///
/// Forbidden here, and why:
/// - heap allocation (`format!`, `String`, ...) — the allocator's internal
///   lock may be held by the interrupted code → deadlock;
/// - `tracing` / `eprintln!` — they allocate and take locks internally;
/// - mutexes — the interrupted thread may already hold the lock → deadlock,
///   or UB for non-reentrant locks;
/// - `Instant::now()` — not on the POSIX async-signal-safe list.
///
/// A single atomic store is the entire budget of a safe handler — and it is
/// safe precisely because `AtomicI32` is lock-free on every target this project
/// supports, compiling down to a plain store instruction. An atomic that fell
/// back to a lock would take that lock inside the handler, which is the very
/// thing forbidden two lines above. This is the Rust equivalent of C's
/// `volatile sig_atomic_t`.
extern "C" fn handle_signal(signo: libc::c_int) {
    PENDING.store(signo, Ordering::Relaxed);
}

/// Installs the SIGTERM/SIGINT handlers for the supervisor process.
///
/// Do not call this in unit tests: `sigaction` changes process-global state,
/// and `cargo test` runs tests as threads of a single process, so installing
/// handlers in one test silently changes signal handling for every other.
pub fn install_handlers() -> Result<(), Errno> {
    // Mask both signals while a handler runs so handlers do not nest.
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGTERM);
    mask.add(Signal::SIGINT);

    // SA_RESTART is deliberate: the poll loop has nothing that wants EINTR,
    // and without it `Command::spawn` / `try_wait` could observe a spurious
    // EINTR in code paths that do not retry it.
    //
    // Never use `SigHandler::SigIgn` here: an "ignore" disposition is
    // inherited across exec(), which would leave every spawned child deaf to
    // SIGTERM. A real handler is reset to SIG_DFL on exec — exactly what the
    // children should get.
    let act = SigAction::new(
        SigHandler::Handler(handle_signal),
        SaFlags::SA_RESTART,
        mask,
    );

    // SAFETY: sigaction is unsafe because the handler runs asynchronously;
    // handle_signal only performs an atomic store, which is async-signal-safe.
    unsafe {
        sigaction(Signal::SIGTERM, &act)?;
    }
    // SAFETY: sigaction is unsafe because the handler runs asynchronously;
    // handle_signal only performs an atomic store, which is async-signal-safe.
    unsafe {
        sigaction(Signal::SIGINT, &act)?;
    }
    Ok(())
}

/// Takes the pending signal, if any, clearing the flag.
pub fn take_pending() -> Option<Signal> {
    signal_from_raw(PENDING.swap(0, Ordering::Relaxed))
}

fn signal_from_raw(signo: i32) -> Option<Signal> {
    if signo == 0 {
        return None;
    }
    // An unrecognised number is dropped silently, which is unreachable while
    // only SIGTERM and SIGINT are installed: nothing else can ever reach
    // `PENDING`. A future signal (SIGHUP for reload, say) has to be added to
    // `install_handlers` and handled by the caller together, or it would land
    // here and turn into a silent no-op.
    Signal::try_from(signo).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // `install_handlers()` is intentionally not exercised here — see its doc
    // comment about process-global state under the multi-threaded test runner.

    #[test]
    fn signal_from_raw_maps_sigterm_and_sigint() {
        assert_eq!(signal_from_raw(libc::SIGTERM), Some(Signal::SIGTERM));
        assert_eq!(signal_from_raw(libc::SIGINT), Some(Signal::SIGINT));
    }

    #[test]
    fn signal_from_raw_rejects_zero() {
        assert_eq!(signal_from_raw(0), None);
    }

    /// The only test touching `PENDING`: the flag is process-global and the
    /// test runner is multi-threaded, so a second such test would race.
    #[test]
    fn pending_round_trips_and_clears() {
        PENDING.store(libc::SIGTERM, Ordering::Relaxed);
        assert_eq!(take_pending(), Some(Signal::SIGTERM));
        assert_eq!(take_pending(), None);
    }
}
