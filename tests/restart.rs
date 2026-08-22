//! Integration tests for the Этап 2 supervision loop: restart policy and
//! exponential backoff.
//!
//! Two kinds of time are mixed here deliberately. The supervised stubs are
//! real OS processes, so between `tick()` calls the tests do short *real*
//! `thread::sleep`s to let a stub actually exit. Backoff delays, however, are
//! measured on the injected `FakeClock`, so the tests never sleep through
//! real backoff seconds — they `advance()` the fake clock instead.

use std::time::{Duration, Instant};

use supervisor_rs::clock::FakeClock;
use supervisor_rs::config::{ProcessConfig, RestartPolicy, DEFAULT_STOP_GRACE_SECS};
use supervisor_rs::supervise::{SupervisorLoop, STABLE_RESET};

fn cfg(name: &str, command: &[&str], restart: RestartPolicy) -> ProcessConfig {
    ProcessConfig {
        name: name.to_string(),
        command: command.iter().map(|s| s.to_string()).collect(),
        workdir: None,
        env: None,
        restart,
        stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
        health_check: None,
    }
}

fn sh(script: &str, restart: RestartPolicy) -> ProcessConfig {
    cfg("stub", &["/usr/bin/env", "sh", "-c", script], restart)
}

/// Ticks (with short real sleeps) until the process at `index` has exited and
/// a restart is scheduled; returns the scheduled backoff delay.
fn wait_for_restart_scheduled(loop_: &mut SupervisorLoop<'_, FakeClock>, index: usize) -> Duration {
    for _ in 0..500 {
        loop_.tick();
        if let Some(delay) = loop_.next_restart_delay(index) {
            return delay;
        }
        assert!(!loop_.is_done(index), "process done, expected a restart");
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for process exit");
}

/// Advances the fake clock past the scheduled delay and ticks to respawn.
fn advance_and_respawn(loop_: &mut SupervisorLoop<'_, FakeClock>, index: usize, delay: Duration) {
    let before = loop_.restart_count(index);
    loop_.clock().advance(delay);
    loop_.tick();
    assert_eq!(loop_.restart_count(index), before + 1, "expected a respawn");
}

#[test]
fn never_does_not_restart() {
    let configs = [sh("exit 1", RestartPolicy::Never)];
    let clock = FakeClock::new(Instant::now());
    let mut loop_ = SupervisorLoop::new(&configs, clock);

    for _ in 0..500 {
        loop_.tick();
        if loop_.is_done(0) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(loop_.is_done(0));
    assert_eq!(loop_.restart_count(0), 0);
}

#[test]
fn always_restarts_after_success() {
    let configs = [sh("exit 0", RestartPolicy::Always)];
    let clock = FakeClock::new(Instant::now());
    let mut loop_ = SupervisorLoop::new(&configs, clock);

    for _ in 0..2 {
        let delay = wait_for_restart_scheduled(&mut loop_, 0);
        advance_and_respawn(&mut loop_, 0, delay);
    }
    assert!(loop_.restart_count(0) >= 2);
}

#[test]
fn on_failure_restarts_on_failure() {
    let configs = [sh("exit 1", RestartPolicy::OnFailure)];
    let clock = FakeClock::new(Instant::now());
    let mut loop_ = SupervisorLoop::new(&configs, clock);

    let delay = wait_for_restart_scheduled(&mut loop_, 0);
    advance_and_respawn(&mut loop_, 0, delay);
    assert_eq!(loop_.restart_count(0), 1);
}

#[test]
fn on_failure_does_not_restart_on_success() {
    let configs = [sh("exit 0", RestartPolicy::OnFailure)];
    let clock = FakeClock::new(Instant::now());
    let mut loop_ = SupervisorLoop::new(&configs, clock);

    for _ in 0..500 {
        loop_.tick();
        if loop_.is_done(0) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(loop_.is_done(0));
    assert_eq!(loop_.restart_count(0), 0);
}

#[test]
fn backoff_delay_grows() {
    let configs = [sh("exit 1", RestartPolicy::OnFailure)];
    let clock = FakeClock::new(Instant::now());
    let mut loop_ = SupervisorLoop::new(&configs, clock);

    // The fake clock does not move between exit detection and this read, so
    // the delays are exact.
    let first = wait_for_restart_scheduled(&mut loop_, 0);
    assert_eq!(first, Duration::from_secs(1));

    advance_and_respawn(&mut loop_, 0, first);
    let second = wait_for_restart_scheduled(&mut loop_, 0);
    assert_eq!(second, Duration::from_secs(2));
}

#[test]
fn backoff_resets_after_stable_run() {
    // The stub sleeps 50ms of *real* time before failing, which leaves room
    // to advance the *fake* clock past STABLE_RESET while it is still alive.
    let configs = [sh("sleep 0.05 && exit 1", RestartPolicy::OnFailure)];
    let clock = FakeClock::new(Instant::now());
    let mut loop_ = SupervisorLoop::new(&configs, clock);

    let first = wait_for_restart_scheduled(&mut loop_, 0);
    assert_eq!(first, Duration::from_secs(1));
    advance_and_respawn(&mut loop_, 0, first);

    // Model a long stable run: fake time jumps past the reset threshold
    // before the (real) process exits again.
    loop_.clock().advance(STABLE_RESET + Duration::from_secs(1));
    let after_stable = wait_for_restart_scheduled(&mut loop_, 0);
    assert_eq!(after_stable, Duration::from_secs(1));
}

/// A process that fails to spawn at startup must not abort supervision of the
/// others (regression test: `new` used to bail on the first spawn error and
/// drop already-spawned children without waiting or killing them).
#[test]
fn partial_start_failure_keeps_supervising_the_rest() {
    let configs = [
        cfg("missing", &["/no/such/binary-xyz"], RestartPolicy::Never),
        sh("exit 0", RestartPolicy::Never),
    ];
    let clock = FakeClock::new(Instant::now());
    let mut loop_ = SupervisorLoop::new(&configs, clock);
    assert!(loop_.had_start_errors());

    // Only the successfully spawned process is tracked, at index 0.
    for _ in 0..500 {
        loop_.tick();
        if loop_.is_done(0) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(loop_.is_done(0));
}

/// A restart-time spawn failure must be retried with backoff, not on every
/// tick (regression test for the pre-fix hot-retry loop).
///
/// The vanishing command is a symlink to `/usr/bin/env`, not a freshly
/// written executable: writing a new executable file and `exec`-ing it
/// moments later races with `fork()` on other threads in this (multi-
/// threaded) test binary and intermittently fails with ETXTBSY. A symlink
/// is never opened for writing, so removing it to fail the next spawn is
/// race-free.
#[test]
fn failed_restart_is_retried_with_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let link_path = dir.path().join("stub");
    std::os::unix::fs::symlink("/usr/bin/env", &link_path).unwrap();

    let configs = [cfg(
        "stub",
        &[link_path.to_str().unwrap(), "sh", "-c", "exit 1"],
        RestartPolicy::OnFailure,
    )];
    let clock = FakeClock::new(Instant::now());
    let mut loop_ = SupervisorLoop::new(&configs, clock);

    let first = wait_for_restart_scheduled(&mut loop_, 0);
    assert_eq!(first, Duration::from_secs(1));

    // Remove the symlink so the next respawn attempt fails.
    std::fs::remove_file(&link_path).unwrap();
    loop_.clock().advance(first);
    loop_.tick();

    // Without the backoff fix this stays at ~0 and every subsequent tick
    // retries immediately; with the fix it grows to the next backoff step.
    let delay_after_failed_respawn = loop_.next_restart_delay(0).unwrap();
    assert_eq!(delay_after_failed_respawn, Duration::from_secs(2));
}
