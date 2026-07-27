//! Supervision loop (Этап 2): watches spawned processes, applies the restart
//! policy on exit and respawns with exponential backoff.
//!
//! Этап 3 adds a shutdown mode: a SIGTERM/SIGINT caught by the process-wide
//! handler is forwarded to every child, and from that point on the restart
//! policy is suppressed — otherwise the forwarded signal would kill a child,
//! `tick()` would see the exit and an `always`/`on-failure` policy would
//! respawn it, and the supervisor would never terminate.

use crate::clock::Clock;
use crate::config::{ExitOutcome, ProcessConfig};
use crate::process;
use nix::sys::signal::Signal;
use std::time::{Duration, Instant};

const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
const BACKOFF_FACTOR: u32 = 2;
/// If a process stays up at least this long, its backoff resets to initial.
pub const STABLE_RESET: Duration = Duration::from_secs(10);
const TICK: Duration = Duration::from_millis(50);

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

struct Supervised<'a> {
    config: &'a ProcessConfig,
    child: Option<std::process::Child>,
    restart_count: u32,
    started_at: Instant,
    next_restart_at: Option<Instant>,
    backoff: Backoff,
    done: bool,
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
                    procs.push(Supervised {
                        config,
                        child: Some(child),
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

    /// Enters shutdown mode: cancels pending restarts and forwards `sig` to
    /// every live child. Idempotent — a repeated signal is a no-op, so the
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
            match proc.child.as_ref() {
                Some(child) => {
                    if let Err(err) = process::forward_signal(child, sig) {
                        // A failed forward must not abort the shutdown of the
                        // remaining processes; log and move on.
                        tracing::error!(
                            name = %proc.config.name,
                            error = %err,
                            "failed to forward signal to process"
                        );
                    }
                    // `done` stays false on purpose: the child is still alive
                    // and must be reaped by tick() before the loop may exit,
                    // otherwise we would leave it orphaned.
                }
                // Nothing left to wait for: no live child and no restart.
                None => proc.done = true,
            }
        }
    }

    pub fn tick(&mut self) {
        for proc in &mut self.procs {
            if let Some(child) = proc.child.as_mut() {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let outcome = classify(status);
                        tracing::info!(
                            name = %proc.config.name,
                            status = ?status,
                            outcome = ?outcome,
                            "process exited"
                        );
                        proc.child = None;
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
                    Ok(None) => {}
                    Err(err) => {
                        tracing::error!(
                            name = %proc.config.name,
                            error = %err,
                            "failed to poll process status"
                        );
                    }
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
                        tracing::info!(
                            name = %proc.config.name,
                            pid = child.id(),
                            restart_count = proc.restart_count,
                            "process restarted"
                        );
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
                    // Logged at info, not debug: until Этап 4 adds escalation a
                    // child ignoring SIGTERM hangs the shutdown forever, and an
                    // operator pressing Ctrl-C again deserves to be told that
                    // SIGKILL is currently the only way out.
                    tracing::info!(
                        signal = ?sig,
                        "already shutting down; send SIGKILL to force an exit"
                    );
                    // TODO(Этап 4): a second signal should escalate to SIGKILL.
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
