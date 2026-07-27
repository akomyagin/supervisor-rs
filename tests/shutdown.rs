//! Integration tests for the Этап 3 shutdown mode, driven in-process.
//!
//! These tests call `begin_shutdown()` directly instead of installing real
//! signal handlers: `sigaction` is process-global state and `cargo test` runs
//! every test as a thread of one process, so a real handler installed here
//! would change signal handling for all the other tests. The signal *delivery*
//! path (handler → `run()`) is covered end-to-end on the real binary in
//! `tests/signals.rs`.
//!
//! As in `tests/restart.rs`, two kinds of time are mixed: the stubs are real
//! processes, so the tests do short *real* sleeps between `tick()`s, while
//! backoff delays are measured on the injected `FakeClock`.

use std::time::{Duration, Instant};

use nix::sys::signal::Signal;
use supervisor_rs::clock::FakeClock;
use supervisor_rs::config::{ProcessConfig, RestartPolicy};
use supervisor_rs::supervise::SupervisorLoop;

fn cfg(name: &str, command: &[&str], restart: RestartPolicy) -> ProcessConfig {
    ProcessConfig {
        name: name.to_string(),
        command: command.iter().map(|s| s.to_string()).collect(),
        workdir: None,
        env: None,
        restart,
    }
}

fn sh(script: &str, restart: RestartPolicy) -> ProcessConfig {
    cfg("stub", &["/usr/bin/env", "sh", "-c", script], restart)
}

/// A stub that outlives the test unless it is signalled. `env` execs `sleep`
/// directly, so the supervised pid is the sleeping process itself and the
/// default SIGTERM disposition ends it — no shell in between to complicate the
/// process tree (that case belongs to Этап 4).
fn long_running(restart: RestartPolicy) -> ProcessConfig {
    cfg("stub", &["/usr/bin/env", "sleep", "100"], restart)
}

/// Ticks with short real sleeps until the process at `index` is done.
fn tick_until_done(loop_: &mut SupervisorLoop<'_, FakeClock>, index: usize) {
    for _ in 0..500 {
        loop_.tick();
        if loop_.is_done(index) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for the process to finish shutting down");
}

/// Ticks until a restart is scheduled for the process at `index`.
fn tick_until_restart_scheduled(loop_: &mut SupervisorLoop<'_, FakeClock>, index: usize) {
    for _ in 0..500 {
        loop_.tick();
        if loop_.next_restart_delay(index).is_some() {
            return;
        }
        assert!(!loop_.is_done(index), "process done, expected a restart");
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for a scheduled restart");
}

#[test]
fn begin_shutdown_terminates_live_child_and_marks_done() {
    let configs = [long_running(RestartPolicy::Always)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));

    loop_.tick();
    assert!(!loop_.is_done(0), "stub should still be running");
    assert!(!loop_.is_shutting_down());

    loop_.begin_shutdown(Signal::SIGTERM);
    assert!(loop_.is_shutting_down());
    // The child is still alive here: it becomes `done` only once tick() reaps
    // it, otherwise the loop could exit leaving it orphaned.
    assert!(!loop_.is_done(0));

    tick_until_done(&mut loop_, 0);
    // `always` policy notwithstanding: shutdown suppresses the restart.
    assert_eq!(loop_.restart_count(0), 0);
}

#[test]
fn begin_shutdown_cancels_pending_restart() {
    let configs = [sh("exit 1", RestartPolicy::OnFailure)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));

    tick_until_restart_scheduled(&mut loop_, 0);
    assert!(loop_.next_restart_delay(0).is_some());

    loop_.begin_shutdown(Signal::SIGTERM);
    assert_eq!(loop_.next_restart_delay(0), None);
    // No live child to wait for, so the process is done immediately.
    assert!(loop_.is_done(0));

    // Even past the backoff deadline the process must not come back. Note this
    // asserts the `done` flag does the blocking: `begin_shutdown` marked a
    // child-less process done, and the respawn branch checks `!proc.done`
    // first. The `!shutting_down` guard next to it is defense in depth and is
    // currently unreachable by construction — no test can single it out.
    loop_.clock().advance(Duration::from_secs(60));
    loop_.tick();
    assert_eq!(loop_.restart_count(0), 0);
}

/// `begin_shutdown` iterates over *every* supervised process. With only
/// single-process configs a regression that signalled just `procs[0]` — or
/// broke out of the loop after the first — would stay green here while
/// orphaning the rest, which is exactly what the stage promises not to do.
#[test]
fn begin_shutdown_terminates_every_process() {
    let configs = [
        long_running(RestartPolicy::Always),
        long_running(RestartPolicy::Always),
        long_running(RestartPolicy::OnFailure),
    ];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));

    loop_.tick();
    loop_.begin_shutdown(Signal::SIGTERM);

    for index in 0..configs.len() {
        tick_until_done(&mut loop_, index);
        assert_eq!(loop_.restart_count(index), 0);
    }
}

#[test]
fn begin_shutdown_is_idempotent() {
    let configs = [long_running(RestartPolicy::Always)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));

    loop_.tick();
    loop_.begin_shutdown(Signal::SIGTERM);
    // A second signal is a no-op — the first one owns the shutdown.
    loop_.begin_shutdown(Signal::SIGINT);
    assert_eq!(loop_.shutdown_signal(), Some(Signal::SIGTERM));

    tick_until_done(&mut loop_, 0);
}
