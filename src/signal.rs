//! Async-signal-safe capture of SIGTERM/SIGINT/SIGHUP for the poll-loop
//! supervisor.
//!
//! Each handler only touches an atomic; the supervise loop polls it via
//! [`take_pending`] (stop) and [`take_reload_pending`] (SIGHUP → config reload,
//! Этап 8) on every iteration. This is a deliberate deviation from the classic
//! self-pipe pattern: a self-pipe exists to wake up a loop that blocks on a
//! file descriptor (`select`/`read`), but `run()` is a poll loop that never
//! blocks on one — there is nothing to wake. See `docs/TECHNICAL_PLAN.md`
//! (Этап 3) for the full rationale.

use nix::errno::Errno;
use nix::libc;
use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// Last signal delivered to the process; 0 means "no signal pending".
///
/// `Ordering::Relaxed` is sufficient: this is a standalone flag with no
/// associated data — nothing is published alongside the store, so no
/// acquire/release ordering with other memory accesses is needed.
static PENDING: AtomicI32 = AtomicI32::new(0);

/// Set when SIGHUP arrives; drained by [`take_reload_pending`] (Этап 8). A
/// separate flag, not a third value in `PENDING`: `PENDING` keeps only the
/// *last* signal, so a SIGHUP landing there could overwrite a concurrent
/// SIGTERM/SIGINT and lose the shutdown request. A reload must never mask a
/// stop.
static RELOAD_PENDING: AtomicBool = AtomicBool::new(false);

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
///
/// SIGHUP (Этап 8) branches to its own flag *before* the single atomic store:
/// the comparison `signo == SIGHUP` is a plain integer compare, itself
/// async-signal-safe, so the handler's budget does not grow. Routing SIGHUP to
/// `RELOAD_PENDING` keeps it out of `PENDING` by construction, so it can never
/// clobber a concurrent SIGTERM/SIGINT there.
extern "C" fn handle_signal(signo: libc::c_int) {
    if signo == libc::SIGHUP {
        RELOAD_PENDING.store(true, Ordering::Relaxed);
    } else {
        PENDING.store(signo, Ordering::Relaxed);
    }
}

/// Installs the SIGTERM/SIGINT/SIGHUP handlers for the supervisor process.
/// SIGTERM/SIGINT request shutdown; SIGHUP requests a config reload (Этап 8) —
/// before Этап 8 SIGHUP killed the daemon by its default disposition.
///
/// Do not call this in unit tests: `sigaction` changes process-global state,
/// and `cargo test` runs tests as threads of a single process, so installing
/// handlers in one test silently changes signal handling for every other.
pub fn install_handlers() -> Result<(), Errno> {
    // Mask all three signals while a handler runs so handlers do not nest.
    let mut mask = SigSet::empty();
    mask.add(Signal::SIGTERM);
    mask.add(Signal::SIGINT);
    mask.add(Signal::SIGHUP);

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
    // SAFETY: sigaction is unsafe because the handler runs asynchronously;
    // handle_signal only performs an atomic store, which is async-signal-safe.
    unsafe {
        sigaction(Signal::SIGHUP, &act)?;
    }
    Ok(())
}

/// Takes the pending signal, if any, clearing the flag.
pub fn take_pending() -> Option<Signal> {
    signal_from_raw(PENDING.swap(0, Ordering::Relaxed))
}

/// Takes the pending reload request (SIGHUP, Этап 8), clearing the flag.
pub fn take_reload_pending() -> bool {
    RELOAD_PENDING.swap(false, Ordering::Relaxed)
}

fn signal_from_raw(signo: i32) -> Option<Signal> {
    if signo == 0 {
        return None;
    }
    // An unrecognised number is dropped silently, which is unreachable: only
    // SIGTERM and SIGINT can ever reach `PENDING`. SIGHUP — the reload signal
    // this comment once anticipated — now lives in its own `RELOAD_PENDING`
    // flag and is branched off in `handle_signal` before the `PENDING` store,
    // so it cannot land here by construction.
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

    /// The only test touching `RELOAD_PENDING` (Этап 8): like `PENDING`, the
    /// flag is process-global, so a second reader would race. `PENDING` is
    /// deliberately left untouched here — that SIGHUP never lands in it is a
    /// property of the `handle_signal` branch, not something to assert against
    /// a shared flag owned by `pending_round_trips_and_clears`. `handle_signal`
    /// is called directly: it is a plain `extern "C"` function.
    #[test]
    fn reload_pending_round_trips_and_clears() {
        handle_signal(libc::SIGHUP);
        assert!(take_reload_pending());
        assert!(!take_reload_pending());
    }
}
