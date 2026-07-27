//! Supervision loop (Этап 2): watches spawned processes, applies the restart
//! policy on exit and respawns with exponential backoff.
//!
//! Этап 3 adds a shutdown mode: a SIGTERM/SIGINT caught by the process-wide
//! handler is forwarded to every child, and from that point on the restart
//! policy is suppressed — otherwise the forwarded signal would kill a child,
//! `tick()` would see the exit and an `always`/`on-failure` policy would
//! respawn it, and the supervisor would never terminate.
//!
//! Этап 4 makes the unit of supervision the process *group* rather than the
//! direct child, and turns the SIGTERM → grace → SIGKILL escalation into
//! per-process state advanced by the ordinary `tick()` (see [`StopPhase`]).
//! A blocking `terminate_tree(pgid, grace)` was rejected: it would serialise
//! the grace period across processes (N × grace to shut down N processes), it
//! is invisible from the outside, and its deadline could not be exercised on
//! the injected `FakeClock`.

use crate::clock::Clock;
use crate::config::{ExitOutcome, ProcessConfig};
use crate::process;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use std::time::{Duration, Instant};

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const BACKOFF_FACTOR: u32 = 2;
/// If a process stays up at least this long, its backoff resets to initial.
pub const STABLE_RESET: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(50);
/// How many consecutive failed status polls a process may accumulate before it
/// is abandoned. ~1 s at the 50 ms tick: any error here is already anomalous
/// (`SA_RESTART` rules EINTR out), a second of retries covers every conceivable
/// transient case, and keeping the process forever would mean an endless loop
/// flooding the log — the Этап 3 debt this closes.
const MAX_CONSECUTIVE_POLL_ERRORS: u32 = 20;
/// How long to wait before re-sending a SIGKILL that the kernel refused. Slow
/// on purpose: a kill that fails once will almost certainly fail again, so
/// retrying every 50 ms tick would only flood the log, while never retrying at
/// all would hang the shutdown forever.
const KILL_RETRY: Duration = Duration::from_secs(1);
/// How many times, and how long apart, the give-up path retries reaping a child
/// it has just SIGKILLed. See `handle_poll_error` for why a single attempt is
/// not enough.
const REAP_RETRIES: u32 = 10;
const REAP_RETRY_DELAY: Duration = Duration::from_millis(10);

pub struct Backoff {
    current: Duration,
    initial: Duration,
    max: Duration,
    factor: u32,
}

impl Backoff {
    pub fn new(initial: Duration, max: Duration, factor: u32) -> Self {
        Self {
            current: initial,
            initial,
            max,
            factor,
        }
    }

    /// Returns the delay to use now and advances to the next (capped) step.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = self.current.saturating_mul(self.factor).min(self.max);
        delay
    }

    pub fn reset(&mut self) {
        self.current = self.initial;
    }
}

fn classify(status: std::process::ExitStatus) -> ExitOutcome {
    if status.success() {
        ExitOutcome::Success
    } else {
        // Non-zero exit code or killed by a signal.
        ExitOutcome::Failure
    }
}

/// Returns true when the failure budget is exhausted and the process should be
/// abandoned. Split out as a pure function because a real `waitid` error on a
/// live direct child cannot be provoked cheaply, so only the decision is unit
/// tested.
fn record_poll_error(count: &mut u32) -> bool {
    *count += 1;
    *count >= MAX_CONSECUTIVE_POLL_ERRORS
}

/// Per-process shutdown escalation state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopPhase {
    /// Not being stopped (normal supervision).
    Idle,
    /// SIGTERM/SIGINT sent to the group; escalate to SIGKILL at `deadline`.
    Terminating { deadline: Instant },
    /// SIGKILL already sent to the group; waiting for the leader to be reaped.
    Killing,
}

/// What a single status poll of a live child concluded.
///
/// The poll is expressed as a value instead of acting in place because acting
/// needs the whole `Supervised`, while the poll itself holds a mutable borrow
/// of its `child` field.
enum PollOutcome {
    Running,
    Exited(std::process::ExitStatus),
    /// The status could not be determined; carries the message to log.
    Failed(String),
}

struct Supervised<'a> {
    config: &'a ProcessConfig,
    child: Option<std::process::Child>,
    /// Process group of the current child; pgid == leader pid, set right after
    /// spawn (the child is made a session leader by `pre_exec` setsid). `Some`
    /// while the leader is alive or an unreaped zombie — exactly the window in
    /// which the kernel cannot recycle the pgid, so `killpg` on it is safe.
    /// Cleared only after the leader is reaped. Never derive it lazily from
    /// `child.id()`: once reaped, `Child` is consumed and the pid may already
    /// belong to somebody else.
    pgid: Option<Pid>,
    stop: StopPhase,
    /// Consecutive status-poll failures; the process is abandoned once
    /// `MAX_CONSECUTIVE_POLL_ERRORS` is reached.
    poll_errors: u32,
    restart_count: u32,
    started_at: Instant,
    next_restart_at: Option<Instant>,
    backoff: Backoff,
    done: bool,
}

/// Polls a live child and, if it has exited, sweeps its process group with
/// SIGKILL *before* reaping it.
///
/// The order peek → sweep → reap is load-bearing. Between the exit and the
/// reaping the leader is a zombie, and a zombie still occupies its pgid, so the
/// kernel cannot have handed that id to an unrelated group: `killpg` is
/// guaranteed to hit our tree. Reaping first would open a TOCTOU window in
/// which the sweep kills innocent processes.
///
/// Sweeping on *every* leader exit — during shutdown and on the restart path
/// alike — is deliberate. The grace period protects the shutdown of the
/// *leader*; once it is gone, the remaining group members are stragglers with
/// nobody left to shepherd them, and without cgroups there is no race-free way
/// to wait for a group to empty (a zombie leader makes even `killpg(pgid, 0)`
/// succeed forever). A leader that wants its own children stopped gracefully
/// must wait for them before exiting — the standard contract. On the restart
/// path the sweep is what keeps a new instance from coming up on top of the
/// old tree; for a childless process it hits a group holding one zombie and is
/// a harmless no-op.
fn poll_child(child: &mut std::process::Child, pgid: Option<Pid>, name: &str) -> PollOutcome {
    match process::peek_exited(child) {
        Ok(false) => PollOutcome::Running,
        Ok(true) => {
            if let Some(pgid) = pgid {
                if let Err(err) = process::signal_group(pgid, Signal::SIGKILL) {
                    tracing::error!(
                        name = %name,
                        error = %err,
                        "failed to sweep process group after leader exit"
                    );
                }
            }
            match child.try_wait() {
                Ok(Some(status)) => PollOutcome::Exited(status),
                // Unreachable after a positive peek — the status is there to be
                // collected. Reported as a poll failure rather than panicking.
                Ok(None) => {
                    PollOutcome::Failed("child reported exited but has no status".to_string())
                }
                Err(err) => PollOutcome::Failed(err.to_string()),
            }
        }
        Err(errno) => PollOutcome::Failed(errno.to_string()),
    }
}

/// Counts a failed status poll and, once the budget is spent, abandons the
/// process. Before Этап 4 the failure was only logged and the process stayed
/// `child = Some` / `done = false` forever, so `run()` spun at 20 iterations a
/// second flooding the log — the Этап 3 debt this closes. The supervisor's exit
/// code is deliberately untouched: giving up shows in the log only.
fn handle_poll_error(proc: &mut Supervised<'_>, err: &str) {
    let exhausted = record_poll_error(&mut proc.poll_errors);
    tracing::error!(
        name = %proc.config.name,
        error = %err,
        consecutive_errors = proc.poll_errors,
        "failed to poll process status"
    );
    if !exhausted {
        return;
    }
    // Best-effort teardown, in the usual order: the leader has not been reaped
    // (we never learned its status), so its pgid is still ours to kill.
    let killed = match proc.pgid {
        Some(pgid) => process::signal_group(pgid, Signal::SIGKILL).is_ok(),
        None => true,
    };
    // Reaping needs a short real-time retry, not a single attempt: SIGKILL was
    // sent microseconds ago and the kernel has almost certainly not moved the
    // process to zombie yet, so one try_wait() would return Ok(None) and we
    // would drop the Child — which neither reaps nor kills — leaking a zombie
    // for the lifetime of the daemon. The wait is real (thread::sleep, not
    // clock.sleep) because what we wait for is the kernel, not logical time,
    // so a FakeClock must not skip it. Blocking the loop for up to
    // REAP_RETRIES * REAP_RETRY_DELAY is acceptable here and nowhere else:
    // this path means something is already deeply wrong, and it runs at most
    // once per process.
    let mut reaped = false;
    if let Some(child) = proc.child.as_mut() {
        for _ in 0..REAP_RETRIES {
            match child.try_wait() {
                Ok(Some(_)) => {
                    reaped = true;
                    break;
                }
                Ok(None) => std::thread::sleep(REAP_RETRY_DELAY),
                // Still failing, exactly as the polls that got us here did.
                Err(_) => break,
            }
        }
    }
    proc.child = None;
    proc.pgid = None;
    proc.stop = StopPhase::Idle;
    proc.done = true;
    if killed && reaped {
        tracing::error!(
            name = %proc.config.name,
            consecutive_errors = proc.poll_errors,
            "giving up on process after repeated poll errors; killed its group and reaped it"
        );
    } else {
        // Say plainly what was left behind. This is the one path on which the
        // stage's acceptance criterion ("no descendant left alive, no zombies
        // accumulating") can fail, and a silent give-up would make the leak
        // undiagnosable from the log.
        tracing::error!(
            name = %proc.config.name,
            consecutive_errors = proc.poll_errors,
            group_killed = killed,
            reaped,
            "giving up on process after repeated poll errors; it may be left \
             behind as a zombie or, if the kill failed, as a live orphan"
        );
    }
}

pub struct SupervisorLoop<'a, C: Clock> {
    procs: Vec<Supervised<'a>>,
    clock: C,
    had_start_errors: bool,
    /// Set once a shutdown signal has been received; suppresses restarts.
    /// Deliberately per-instance state rather than a global: tests can drive
    /// `begin_shutdown()` directly without touching process-wide signal state.
    shutting_down: bool,
    shutdown_signal: Option<Signal>,
}

impl<'a, C: Clock> SupervisorLoop<'a, C> {
    /// Spawns every configured process, best-effort: a process that fails to
    /// spawn is logged and skipped rather than aborting the whole startup —
    /// aborting would drop already-spawned `Child`s without waiting or
    /// killing them, orphaning healthy processes over a single bad one.
    /// Skipped processes are not tracked, so indices into per-process
    /// accessors (`restart_count`, `is_done`, ...) refer to the subsequence
    /// of `configs` that spawned successfully, not `configs` itself.
    pub fn new(configs: &'a [ProcessConfig], clock: C) -> Self {
        let mut procs = Vec::with_capacity(configs.len());
        let mut had_start_errors = false;
        for config in configs {
            match process::spawn(config) {
                Ok(child) => {
                    tracing::info!(name = %config.name, pid = child.id(), "process spawned");
                    let pgid = Pid::from_raw(child.id() as i32);
                    procs.push(Supervised {
                        config,
                        child: Some(child),
                        pgid: Some(pgid),
                        stop: StopPhase::Idle,
                        poll_errors: 0,
                        restart_count: 0,
                        started_at: clock.now(),
                        next_restart_at: None,
                        backoff: Backoff::new(INITIAL_BACKOFF, MAX_BACKOFF, BACKOFF_FACTOR),
                        done: false,
                    });
                }
                Err(err) => {
                    tracing::error!(name = %config.name, error = %err, "failed to spawn process");
                    had_start_errors = true;
                }
            }
        }
        Self {
            procs,
            clock,
            had_start_errors,
            shutting_down: false,
            shutdown_signal: None,
        }
    }

    /// Whether any process failed to spawn during startup.
    pub fn had_start_errors(&self) -> bool {
        self.had_start_errors
    }

    /// Enters shutdown mode: cancels pending restarts and sends `sig` to the
    /// whole process group of every live child, then arms the per-process
    /// SIGKILL deadline. Idempotent — a repeated signal is a no-op, so the
    /// first signal keeps ownership of the shutdown.
    pub fn begin_shutdown(&mut self, sig: Signal) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        self.shutdown_signal = Some(sig);
        tracing::info!(signal = ?sig, "shutdown requested, forwarding to children");

        for proc in &mut self.procs {
            // Drop any scheduled restart: a process waiting out its backoff
            // must not come back to life during shutdown.
            proc.next_restart_at = None;
            if proc.child.is_some() {
                // The signal goes to the group, not the pid: a child that
                // forked its own children must take the whole tree down.
                if let Some(pgid) = proc.pgid {
                    if let Err(err) = process::signal_group(pgid, sig) {
                        // A failed forward must not abort the shutdown of the
                        // remaining processes; log and move on.
                        tracing::error!(
                            name = %proc.config.name,
                            error = %err,
                            "failed to signal process group"
                        );
                    }
                }
                let deadline = self.clock.now() + proc.config.stop_grace();
                proc.stop = StopPhase::Terminating { deadline };
                // `done` stays false on purpose: the child is still alive
                // and must be reaped by tick() before the loop may exit,
                // otherwise we would leave it orphaned.
            } else {
                // Nothing left to wait for: no live child and no restart.
                proc.done = true;
            }
        }
    }

    /// Immediately SIGKILLs every live child's process group, skipping whatever
    /// grace is left. Used when a second shutdown signal arrives: an operator
    /// pressing Ctrl-C twice is asking for the hard stop. Reaping and `done`
    /// still happen in the ordinary `tick()`.
    ///
    /// Precondition: call only after [`begin_shutdown`](Self::begin_shutdown).
    /// This method does not enter shutdown mode itself, so calling it on a
    /// running supervisor would kill the groups and then let the restart policy
    /// bring them straight back — "killed and immediately respawned". `run()`
    /// guards the call; the method is `pub` for in-process tests.
    pub fn escalate_to_kill(&mut self) {
        debug_assert!(
            self.shutting_down,
            "escalate_to_kill without begin_shutdown would race the restart policy"
        );
        for proc in &mut self.procs {
            if proc.child.is_none() {
                continue;
            }
            let killed = match proc.pgid {
                Some(pgid) => match process::signal_group(pgid, Signal::SIGKILL) {
                    Ok(()) => true,
                    Err(err) => {
                        tracing::error!(
                            name = %proc.config.name,
                            error = %err,
                            "failed to SIGKILL process group; falling back to the deadline path"
                        );
                        false
                    }
                },
                None => true,
            };
            // On failure, hand the process to the deadline path with an expired
            // deadline instead of parking it in the terminal Killing phase: the
            // next tick retries and keeps retrying. Parking it there would hang
            // the shutdown forever — see the escalation branch in tick().
            proc.stop = if killed {
                StopPhase::Killing
            } else {
                StopPhase::Terminating {
                    deadline: self.clock.now(),
                }
            };
        }
    }

    pub fn tick(&mut self) {
        for proc in &mut self.procs {
            if let Some(child) = proc.child.as_mut() {
                let outcome = poll_child(child, proc.pgid, &proc.config.name);
                match outcome {
                    PollOutcome::Running => {
                        proc.poll_errors = 0;
                        if let StopPhase::Terminating { deadline } = proc.stop {
                            if self.clock.now() >= deadline {
                                // Grace expired: the leader ignored the
                                // shutdown signal. Its group is still pinned by
                                // the live leader, so killpg is safe here too.
                                tracing::warn!(
                                    name = %proc.config.name,
                                    "grace period expired; escalating to SIGKILL"
                                );
                                let killed = match proc.pgid {
                                    Some(pgid) => {
                                        match process::signal_group(pgid, Signal::SIGKILL) {
                                            Ok(()) => true,
                                            Err(err) => {
                                                tracing::error!(
                                                    name = %proc.config.name,
                                                    error = %err,
                                                    retry_in = ?KILL_RETRY,
                                                    "failed to SIGKILL process group; will retry"
                                                );
                                                false
                                            }
                                        }
                                    }
                                    None => true,
                                };
                                // Advance to Killing only if the signal actually
                                // went out. Killing is terminal — nothing
                                // re-examines it — so entering it after a failed
                                // kill would strand the process forever and hang
                                // the whole shutdown. Staying in Terminating with
                                // a fresh deadline retries instead, throttled to
                                // one attempt per KILL_RETRY rather than one per
                                // 50 ms tick: an unkillable process must not
                                // flood the log, which is the very Этап 3 defect
                                // the poll-error budget below exists to fix.
                                proc.stop = if killed {
                                    StopPhase::Killing
                                } else {
                                    StopPhase::Terminating {
                                        deadline: self.clock.now() + KILL_RETRY,
                                    }
                                };
                            }
                        }
                    }
                    PollOutcome::Exited(status) => {
                        proc.poll_errors = 0;
                        let outcome = classify(status);
                        tracing::info!(
                            name = %proc.config.name,
                            status = ?status,
                            outcome = ?outcome,
                            "process exited"
                        );
                        proc.child = None;
                        // The leader has been reaped: the kernel may recycle
                        // its pgid from now on, so it must never be used again.
                        proc.pgid = None;
                        proc.stop = StopPhase::Idle;
                        if self.shutting_down {
                            // During shutdown the exit is the expected result
                            // of our own forwarded signal — the restart policy
                            // is not consulted at all.
                            proc.done = true;
                            tracing::info!(
                                name = %proc.config.name,
                                "process exited during shutdown"
                            );
                        } else if proc.config.restart.should_restart(outcome) {
                            if self.clock.now().saturating_duration_since(proc.started_at)
                                >= STABLE_RESET
                            {
                                proc.backoff.reset();
                            }
                            let delay = proc.backoff.next_delay();
                            proc.next_restart_at = Some(self.clock.now() + delay);
                            tracing::info!(
                                name = %proc.config.name,
                                delay = ?delay,
                                "restart scheduled"
                            );
                        } else {
                            proc.done = true;
                        }
                    }
                    PollOutcome::Failed(err) => handle_poll_error(proc, &err),
                }
            } else if !proc.done
                // Defense in depth: begin_shutdown() already cleared every
                // next_restart_at, but a respawn here would resurrect a
                // process we are trying to stop.
                && !self.shutting_down
                && proc
                    .next_restart_at
                    .is_some_and(|at| self.clock.now() >= at)
            {
                match process::spawn(proc.config) {
                    Ok(child) => {
                        proc.restart_count += 1;
                        proc.started_at = self.clock.now();
                        proc.next_restart_at = None;
                        proc.poll_errors = 0;
                        tracing::info!(
                            name = %proc.config.name,
                            pid = child.id(),
                            restart_count = proc.restart_count,
                            "process restarted"
                        );
                        proc.pgid = Some(Pid::from_raw(child.id() as i32));
                        proc.stop = StopPhase::Idle;
                        proc.child = Some(child);
                    }
                    Err(err) => {
                        // Apply the same backoff as a crashing process would get,
                        // so a permanently broken command (e.g. missing binary)
                        // doesn't hot-retry every tick.
                        let delay = proc.backoff.next_delay();
                        proc.next_restart_at = Some(self.clock.now() + delay);
                        tracing::error!(
                            name = %proc.config.name,
                            error = %err,
                            delay = ?delay,
                            "failed to restart process; will retry with backoff"
                        );
                    }
                }
            }
        }
    }

    fn any_active(&self) -> bool {
        self.procs.iter().any(|proc| !proc.done)
    }

    pub fn run(&mut self) {
        loop {
            // Polled before the liveness check so that a signal delivered
            // while `new()` was still spawning is acted on in the very first
            // iteration, instead of after a full tick.
            if let Some(sig) = crate::signal::take_pending() {
                if !self.shutting_down {
                    self.begin_shutdown(sig);
                } else {
                    // A second signal is read as "stop waiting": skip whatever
                    // grace is left and SIGKILL every group right away.
                    tracing::warn!(
                        signal = ?sig,
                        "second signal during shutdown; escalating to SIGKILL"
                    );
                    self.escalate_to_kill();
                }
            }
            if !self.any_active() {
                break;
            }
            self.tick();
            self.clock.sleep(TICK);
        }
    }

    // Test-visibility accessors: integration tests drive tick() manually and
    // need to observe per-process state.

    pub fn clock(&self) -> &C {
        &self.clock
    }

    pub fn restart_count(&self, index: usize) -> u32 {
        self.procs[index].restart_count
    }

    pub fn is_done(&self, index: usize) -> bool {
        self.procs[index].done
    }

    /// Remaining delay until the scheduled restart, measured from `clock.now()`.
    pub fn next_restart_delay(&self, index: usize) -> Option<Duration> {
        self.procs[index]
            .next_restart_at
            .map(|at| at.saturating_duration_since(self.clock.now()))
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down
    }

    /// The signal that started the shutdown, if any.
    pub fn shutdown_signal(&self) -> Option<Signal> {
        self.shutdown_signal
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_and_caps() {
        let mut backoff = Backoff::new(INITIAL_BACKOFF, MAX_BACKOFF, BACKOFF_FACTOR);
        let expected = [1u64, 2, 4, 8, 16, 30, 30];
        for secs in expected {
            assert_eq!(backoff.next_delay(), Duration::from_secs(secs));
        }
    }

    #[test]
    fn backoff_resets() {
        let mut backoff = Backoff::new(INITIAL_BACKOFF, MAX_BACKOFF, BACKOFF_FACTOR);
        backoff.next_delay();
        backoff.next_delay();
        assert_eq!(backoff.next_delay(), Duration::from_secs(4));
        backoff.reset();
        assert_eq!(backoff.next_delay(), INITIAL_BACKOFF);
    }

    /// Only the budget decision is unit tested: provoking a real `waitid`
    /// error on a live direct child is not worth mocking an errno for. The
    /// wiring of the give-up branch in `tick()` is covered by review.
    #[test]
    fn poll_error_budget_exhausts_after_max() {
        let mut count = 0;
        for _ in 1..MAX_CONSECUTIVE_POLL_ERRORS {
            assert!(!record_poll_error(&mut count));
        }
        assert_eq!(count, MAX_CONSECUTIVE_POLL_ERRORS - 1);
        assert!(record_poll_error(&mut count));

        // A successful poll resets the counter, so only *consecutive* failures
        // add up.
        count = 0;
        assert!(!record_poll_error(&mut count));
    }

    /// `handle_poll_error` takes `&mut Supervised` directly rather than being
    /// woven only into `tick()`'s match arms, so the give-up branch can be
    /// exercised without provoking a real `waitid` errno: build a `Supervised`
    /// around a genuinely long-lived child, call the poll-error handler
    /// `MAX_CONSECUTIVE_POLL_ERRORS` times as if every status poll had failed,
    /// and check both the resulting state *and* that the child's group was
    /// actually killed, not just that the struct fields were reset.
    #[test]
    fn poll_error_gives_up_and_kills_group_after_max() {
        let config = ProcessConfig {
            name: "stuck".to_string(),
            command: vec![
                "/usr/bin/env".to_string(),
                "sleep".to_string(),
                "60".to_string(),
            ],
            workdir: None,
            env: None,
            restart: crate::config::RestartPolicy::Never,
            stop_grace_secs: crate::config::DEFAULT_STOP_GRACE_SECS,
        };
        let child = process::spawn(&config).unwrap();
        let pid = Pid::from_raw(child.id() as i32);
        let pgid = Pid::from_raw(child.id() as i32);

        let mut proc = Supervised {
            config: &config,
            child: Some(child),
            pgid: Some(pgid),
            stop: StopPhase::Idle,
            poll_errors: 0,
            restart_count: 0,
            started_at: std::time::Instant::now(),
            next_restart_at: None,
            backoff: Backoff::new(INITIAL_BACKOFF, MAX_BACKOFF, BACKOFF_FACTOR),
            done: false,
        };

        for i in 1..MAX_CONSECUTIVE_POLL_ERRORS {
            handle_poll_error(&mut proc, "synthetic poll error");
            assert!(!proc.done, "abandoned too early at iteration {i}");
            assert_eq!(proc.poll_errors, i);
        }
        handle_poll_error(&mut proc, "synthetic poll error");

        assert!(proc.done, "process was not abandoned after the budget");
        assert!(proc.child.is_none(), "child handle was not dropped");
        assert!(proc.pgid.is_none(), "pgid was not cleared");
        assert_eq!(proc.stop, StopPhase::Idle);

        // The teardown must be more than a state reset: the SIGKILL must have
        // actually reached the leader. It becomes a zombie the instant the
        // kernel processes the signal, which can be a moment after `killpg`
        // returns, so a single `try_wait` inside `handle_poll_error` can miss
        // the exit (`Ok(None)`) and drop the `Child` handle while the process
        // is still a zombie -- with `child` gone, nobody ever reaps it, so
        // `ps` shows it as `Z <defunct>` forever. That is why the reachable
        // signal here is "the leader received SIGKILL", checked via `ps`
        // STAT, not "the pid is fully gone" -- the latter is a real, separate
        // gap (see the coverage report).
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut stat = String::new();
        while std::time::Instant::now() < deadline {
            let out = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid.to_string()])
                .output()
                .unwrap();
            stat = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if stat.is_empty() || stat.starts_with('Z') {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            stat.is_empty() || stat.starts_with('Z'),
            "the stuck child's group was not killed (ps STAT={stat:?})"
        );
    }

    #[test]
    fn classify_success_and_failure() {
        let ok = std::process::Command::new("/usr/bin/env")
            .arg("true")
            .status()
            .unwrap();
        assert_eq!(classify(ok), ExitOutcome::Success);

        let fail = std::process::Command::new("/usr/bin/env")
            .arg("false")
            .status()
            .unwrap();
        assert_eq!(classify(fail), ExitOutcome::Failure);
    }
}
