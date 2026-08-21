//! End-to-end tests for the Этап 6 control socket on the real `supervisor-rs`
//! binary: `stop`/`start`/`restart` against a live daemon, the client's error
//! paths, and the socket-file lifecycle (removed on clean exit, recreated over
//! an orphaned file, refused when a live daemon already owns it).
//!
//! `write_config`, `start_supervisor`, `wait_for_state` and `wait_with_timeout`
//! are deliberately duplicated from `tests/status.rs`: every integration test
//! file is its own crate, and the existing files already made that trade — see
//! their preambles. `start_supervisor` is extended with `--control-socket`.
//!
//! Every daemon this file starts isolates **both** `--state-file` and
//! `--control-socket` in its own tempdir: the default paths are one per uid,
//! and tests run in parallel.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use supervisor_rs::state::{self, ProcState, StateSnapshot};

/// Deadline for the daemon to come up and publish its first snapshot.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// Deadline for the daemon to shut down after being signalled.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// A stub that outlives the test unless it is signalled or told to stop, and
/// restarts under policy `always` — so `stop` fighting the policy is
/// observable.
fn write_config(dir: &Path) -> PathBuf {
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[[process]]
name = "web"
restart = "always"
command = ["/usr/bin/env", "sleep", "100"]
"#,
    )
    .unwrap();
    config_path
}

/// A stub that ignores SIGTERM, so only the SIGKILL escalation stops it — used
/// by the shutdown-in-progress test to give a `stop-grace-secs` window wide
/// enough to observe `stopping` before the daemon actually exits.
fn write_deaf_config(dir: &Path, stop_grace_secs: u64) -> PathBuf {
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[[process]]
name = "web"
restart = "never"
stop-grace-secs = {stop_grace_secs}
command = ["/usr/bin/env", "sh", "-c", "trap '' TERM; i=0; while [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done"]
"#
        ),
    )
    .unwrap();
    config_path
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join("state.toml")
}

fn socket_path(dir: &Path) -> PathBuf {
    // Short file name: sun_path is limited to ~108 bytes, and a tempdir path
    // plus a long socket name can overflow it with a loud bind error.
    dir.join("c.sock")
}

/// Starts the daemon with its state file and control socket inside the test's
/// own temp dir.
fn start_supervisor(config_path: &Path, state_path: &Path, socket_path: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_supervisor-rs"))
        .arg("run")
        .arg(config_path)
        .arg("--state-file")
        .arg(state_path)
        .arg("--control-socket")
        .arg(socket_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn control(verb: &str, name: &str, socket_path: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_supervisor-rs"))
        .arg(verb)
        .arg(name)
        .arg("--control-socket")
        .arg(socket_path)
        .output()
        .unwrap()
}

/// Polls the state file until it parses *and* satisfies `pred`.
fn wait_for_state(
    path: &Path,
    pred: impl Fn(&StateSnapshot) -> bool,
    timeout: Duration,
) -> StateSnapshot {
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    while Instant::now() < deadline {
        match state::read(path) {
            Ok(snapshot) if pred(&snapshot) => return snapshot,
            Ok(snapshot) => last = format!("{snapshot:?}"),
            Err(err) => last = err.to_string(),
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "state file {} did not reach the expected state within {timeout:?}; last seen: {last}",
        path.display()
    );
}

/// Waits for the daemon to exit. On timeout it SIGKILLs it so the test run is
/// not left with a stray daemon, then fails the test.
fn wait_with_timeout(child: &mut Child, timeout: Duration) -> ExitStatus {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("supervisor did not exit within {timeout:?}");
}

fn web_running(snapshot: &StateSnapshot) -> bool {
    snapshot
        .process
        .first()
        .is_some_and(|proc| proc.state == ProcState::Running)
}

/// The acceptance criterion of Этап 6, end to end: on a daemon supervising a
/// process with policy `always`, `stop` stops it and keeps it stopped despite
/// the policy, `start` revives it with a new pid, and `restart` changes the pid
/// of a running process.
#[test]
fn stop_start_restart_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = write_config(dir.path());
    let state_path = state_path(dir.path());
    let socket_path = socket_path(dir.path());
    let mut supervisor = start_supervisor(&config_path, &state_path, &socket_path);

    let snapshot = wait_for_state(&state_path, web_running, READY_TIMEOUT);
    let pid1 = snapshot.process[0].pid.expect("running process has a pid");

    let out = control("stop", "web", &socket_path);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.starts_with("ok"), "{stdout}");

    // `always` policy does not resurrect it: reaching `stopped` with no pid at
    // all proves the policy was suppressed, not merely delayed.
    let snapshot = wait_for_state(
        &state_path,
        |s| s.process[0].state == ProcState::Stopped && s.process[0].pid.is_none(),
        READY_TIMEOUT,
    );
    assert_eq!(snapshot.process[0].restart_count, 0);

    let out = control("start", "web", &socket_path);
    assert_eq!(out.status.code(), Some(0));
    let snapshot = wait_for_state(
        &state_path,
        |s| {
            s.process[0].state == ProcState::Running
                && s.process[0].pid.is_some_and(|pid| pid != pid1)
        },
        READY_TIMEOUT,
    );
    let pid2 = snapshot.process[0].pid.unwrap();
    assert_eq!(snapshot.process[0].restart_count, 1);

    let out = control("restart", "web", &socket_path);
    assert_eq!(out.status.code(), Some(0));
    let snapshot = wait_for_state(
        &state_path,
        |s| {
            s.process[0].state == ProcState::Running
                && s.process[0].pid.is_some_and(|pid| pid != pid2)
        },
        READY_TIMEOUT,
    );
    assert_eq!(snapshot.process[0].restart_count, 2);

    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM).unwrap();
    let exit = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
    assert_eq!(exit.code(), Some(0));
}

#[test]
fn stop_of_unknown_name_reports_error() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = write_config(dir.path());
    let state_path = state_path(dir.path());
    let socket_path = socket_path(dir.path());
    let mut supervisor = start_supervisor(&config_path, &state_path, &socket_path);
    wait_for_state(&state_path, web_running, READY_TIMEOUT);

    let out = control("stop", "ghost", &socket_path);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no such process"), "{stderr}");

    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM).unwrap();
    wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
}

#[test]
fn client_reports_connect_error_without_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("absent.sock");

    let out = control("stop", "web", &socket_path);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot connect"), "{stderr}");
}

#[test]
fn restart_is_rejected_during_daemon_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = write_deaf_config(dir.path(), 30);
    let state_path = state_path(dir.path());
    let socket_path = socket_path(dir.path());
    let mut supervisor = start_supervisor(&config_path, &state_path, &socket_path);
    wait_for_state(&state_path, web_running, READY_TIMEOUT);

    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM).unwrap();
    // Handshake: `stopping` in the state file means shutdown has begun.
    wait_for_state(
        &state_path,
        |s| s.process[0].state == ProcState::Stopping,
        READY_TIMEOUT,
    );

    let out = control("restart", "web", &socket_path);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("shutting down"), "{stderr}");

    // Escalate so the deaf stub actually dies and the test does not wait out
    // the 30 s grace.
    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM).unwrap();
    wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
}

#[test]
fn daemon_removes_socket_on_clean_exit() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = write_config(dir.path());
    let state_path = state_path(dir.path());
    let socket_path = socket_path(dir.path());
    let mut supervisor = start_supervisor(&config_path, &state_path, &socket_path);
    wait_for_state(&state_path, web_running, READY_TIMEOUT);
    assert!(socket_path.exists(), "socket file was not created");

    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM).unwrap();
    let exit = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
    assert_eq!(exit.code(), Some(0));

    // The removal happens inside run(), before the process exits, so after
    // `wait` this is a deterministic single assertion, not a poll.
    assert!(
        !socket_path.exists(),
        "the control socket outlived the daemon it belonged to"
    );
}

#[test]
fn daemon_starts_over_orphaned_socket() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = write_config(dir.path());
    let state_path = state_path(dir.path());
    let socket_path = socket_path(dir.path());

    // Simulate a daemon that died without cleaning up: bind and drop.
    {
        let listener = std::os::unix::net::UnixListener::bind(&socket_path).unwrap();
        drop(listener);
    }
    assert!(socket_path.exists(), "precondition: orphaned file present");

    let mut supervisor = start_supervisor(&config_path, &state_path, &socket_path);
    wait_for_state(&state_path, web_running, READY_TIMEOUT);

    // The socket was rebound (not just left as a dead file): a real command
    // round-trips through it.
    let out = control("stop", "web", &socket_path);
    assert_eq!(out.status.code(), Some(0));

    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM).unwrap();
    wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
}

#[test]
fn second_daemon_refuses_busy_socket() {
    let dir1 = tempfile::tempdir().unwrap();
    let config1 = write_config(dir1.path());
    let state1 = state_path(dir1.path());
    let socket_path = socket_path(dir1.path());
    let mut supervisor1 = start_supervisor(&config1, &state1, &socket_path);
    wait_for_state(&state1, web_running, READY_TIMEOUT);

    // Daemon 2 shares the socket but has its own config and state file, so a
    // bind failure (not a state-file collision) is what's under test.
    let dir2 = tempfile::tempdir().unwrap();
    let config2 = write_config(dir2.path());
    let state2 = state_path(dir2.path());
    let mut supervisor2 = start_supervisor(&config2, &state2, &socket_path);

    let exit = wait_with_timeout(&mut supervisor2, SHUTDOWN_TIMEOUT);
    assert_eq!(exit.code(), Some(1));
    // Bind happens before the first spawn, so daemon 2 never got far enough to
    // publish a state file.
    assert!(
        !state2.exists(),
        "daemon 2 must not have started supervising before the bind check"
    );

    kill(Pid::from_raw(supervisor1.id() as i32), Signal::SIGTERM).unwrap();
    wait_with_timeout(&mut supervisor1, SHUTDOWN_TIMEOUT);
}
