//! In-process tests for the Этап 5 state snapshot: what `SupervisorLoop`
//! reports about its processes, and when it publishes it.
//!
//! Driven with `FakeClock` and without installing real signal handlers, for the
//! same reason as `tests/shutdown.rs`: `sigaction` is process-global and
//! `cargo test` runs every test as a thread of one process. The end-to-end
//! behaviour of the `status` command on the real binary lives in
//! `tests/status.rs`.
//!
//! The `cfg`/`sh` helpers below are copied from `tests/shutdown.rs`. Every
//! integration test file is its own crate, and the existing files already made
//! that duplication a deliberate choice — see their preambles.

use std::path::Path;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use supervisor_rs::clock::FakeClock;
use supervisor_rs::config::{ProcessConfig, RestartPolicy, DEFAULT_STOP_GRACE_SECS};
use supervisor_rs::state::{self, ProcState, StateSnapshot};
use supervisor_rs::supervise::{SupervisorLoop, STATE_WRITE_INTERVAL};

/// Deadline for the state file to show up with the content a test waits for.
const STATE_TIMEOUT: Duration = Duration::from_secs(5);

fn cfg(name: &str, command: &[&str], restart: RestartPolicy) -> ProcessConfig {
    ProcessConfig {
        name: name.to_string(),
        command: command.iter().map(|s| s.to_string()).collect(),
        workdir: None,
        env: None,
        restart,
        stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
        health_check: None,
        log: None,
    }
}

fn sh(name: &str, script: &str, restart: RestartPolicy) -> ProcessConfig {
    cfg(name, &["/usr/bin/env", "sh", "-c", script], restart)
}

/// A stub that outlives the test unless it is signalled.
fn long_running(name: &str, restart: RestartPolicy) -> ProcessConfig {
    cfg(name, &["/usr/bin/env", "sleep", "100"], restart)
}

/// A stub deaf to SIGTERM, so the `stopping` state can be observed without
/// racing the child's death. Loops over short sleeps because `sh` only runs a
/// trap between commands.
const DEAF_SCRIPT: &str = r#"trap '' TERM
i=0
while [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done
"#;

fn is_alive(pid: u32) -> bool {
    kill(Pid::from_raw(pid as i32), None) != Err(Errno::ESRCH)
}

/// Ticks with short real sleeps until the process at `index` is done.
fn tick_until_done(loop_: &mut SupervisorLoop<FakeClock>, index: usize) {
    for _ in 0..500 {
        loop_.tick();
        if loop_.is_done(index) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for the process to finish");
}

/// Ticks until a restart is scheduled for the process at `index`.
fn tick_until_restart_scheduled(loop_: &mut SupervisorLoop<FakeClock>, index: usize) {
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

/// Polls the state file until it parses. Any error is retried until the
/// deadline — which doubles as a smoke test of the atomic write: with a
/// non-atomic writer this would eventually observe a half-written file and the
/// retries would show up as flaky parse errors.
fn wait_for_state(path: &Path, timeout: Duration) -> StateSnapshot {
    let deadline = Instant::now() + timeout;
    loop {
        match state::read(path) {
            Ok(snapshot) => return snapshot,
            Err(err) => {
                assert!(
                    Instant::now() < deadline,
                    "no readable state file at {} within {timeout:?}: {err}",
                    path.display()
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// The first publication is immediate, not one interval late: `status` has to
/// work right after the daemon comes up.
#[test]
fn first_maybe_write_creates_state_file_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.toml");
    let configs = [long_running("web", RestartPolicy::Always)];
    let mut loop_ =
        SupervisorLoop::new(&configs, FakeClock::new(Instant::now())).with_state_file(path.clone());

    loop_.tick();
    loop_.maybe_write_state();

    let snapshot = wait_for_state(&path, STATE_TIMEOUT);
    assert_eq!(snapshot.daemon_pid, std::process::id());
    assert_eq!(snapshot.process.len(), 1);
    let proc = &snapshot.process[0];
    assert_eq!(proc.name, "web");
    assert_eq!(proc.state, ProcState::Running);
    assert_eq!(proc.restart_count, 0);
    assert!(
        proc.uptime_secs.is_some(),
        "a running process has an uptime"
    );
    let pid = proc.pid.expect("a running process has a pid");
    assert!(
        is_alive(pid),
        "the reported pid {pid} is not a live process"
    );

    loop_.begin_shutdown(Signal::SIGTERM);
    tick_until_done(&mut loop_, 0);
}

/// Both sides of the write interval, on the fake clock: nothing is republished
/// before it elapses, and the next call after it does republish.
#[test]
fn interval_throttles_writes_on_injected_clock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.toml");
    let configs = [long_running("web", RestartPolicy::Always)];
    let mut loop_ =
        SupervisorLoop::new(&configs, FakeClock::new(Instant::now())).with_state_file(path.clone());

    loop_.tick();
    loop_.maybe_write_state();
    wait_for_state(&path, STATE_TIMEOUT);

    // Removing the file makes the *absence* of a rewrite observable.
    std::fs::remove_file(&path).unwrap();
    loop_.maybe_write_state();
    assert!(
        !path.exists(),
        "the state file was rewritten before the interval elapsed"
    );

    loop_.clock().advance(STATE_WRITE_INTERVAL);
    loop_.maybe_write_state();
    assert!(
        path.exists(),
        "the state file was not rewritten after the interval elapsed"
    );

    loop_.begin_shutdown(Signal::SIGTERM);
    tick_until_done(&mut loop_, 0);
}

/// A process waiting out its backoff is `restarting`, with no pid and no
/// uptime — there is no instance to report either for.
#[test]
fn snapshot_reflects_restarting_after_crash() {
    let configs = [sh("crasher", "exit 1", RestartPolicy::OnFailure)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));

    tick_until_restart_scheduled(&mut loop_, 0);

    let snapshot = loop_.snapshot();
    let proc = &snapshot.process[0];
    assert_eq!(proc.name, "crasher");
    assert_eq!(proc.state, ProcState::Restarting);
    assert_eq!(proc.pid, None);
    assert_eq!(proc.uptime_secs, None);
    // Pins the current semantics: the counter is incremented by the respawn,
    // not by the decision to respawn.
    assert_eq!(proc.restart_count, 0);
}

/// Between the shutdown signal and the child's death the process is
/// `stopping`, still with a pid; once reaped it is `stopped` without one.
#[test]
fn snapshot_reflects_stopping_during_shutdown() {
    let configs = [sh("deaf", DEAF_SCRIPT, RestartPolicy::Never)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));

    loop_.tick();
    loop_.begin_shutdown(Signal::SIGTERM);

    let stopping = &loop_.snapshot().process[0];
    assert_eq!(stopping.state, ProcState::Stopping);
    assert!(stopping.pid.is_some(), "a stopping process still has a pid");

    loop_.escalate_to_kill();
    tick_until_done(&mut loop_, 0);

    let stopped = &loop_.snapshot().process[0];
    assert_eq!(stopped.state, ProcState::Stopped);
    assert_eq!(stopped.pid, None);
    assert_eq!(stopped.uptime_secs, None);
}

#[test]
fn snapshot_reflects_stopped_after_clean_exit() {
    let configs = [sh("oneshot", "exit 0", RestartPolicy::Never)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));

    tick_until_done(&mut loop_, 0);

    let proc = &loop_.snapshot().process[0];
    assert_eq!(proc.state, ProcState::Stopped);
    assert_eq!(proc.pid, None);
    assert_eq!(proc.uptime_secs, None);
}

/// A clean exit takes the state file with it — that is what makes "no file"
/// mean "not running".
///
/// Calling `run()` in-process is safe here: nobody signals this test process,
/// `FakeClock::sleep` does not block, and the single `never` process exits on
/// its own, so the loop terminates by itself.
#[test]
fn run_removes_state_file_on_exit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.toml");
    let configs = [cfg(
        "oneshot",
        &["/usr/bin/env", "true"],
        RestartPolicy::Never,
    )];
    let mut loop_ =
        SupervisorLoop::new(&configs, FakeClock::new(Instant::now())).with_state_file(path.clone());

    loop_.run();

    assert!(
        !path.exists(),
        "the state file outlived the daemon's clean exit"
    );
}
