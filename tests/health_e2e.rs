//! End-to-end tests for Этап 7 health checks on the real `supervisor-rs`
//! binary: a process whose exec probe starts failing is restarted after the
//! threshold; a daemon probing frequently still shuts down cleanly; an invalid
//! health-check section fails `run` with a config error.
//!
//! `write_config`, `start_supervisor`, `wait_for_state` and `wait_with_timeout`
//! are deliberately duplicated from `tests/control.rs`/`tests/status.rs`: every
//! integration test file is its own crate, and the existing files already made
//! that trade — see their preambles.
//!
//! Every daemon this file starts isolates **both** `--state-file` and
//! `--control-socket` in its own tempdir: the default paths are one per uid, and
//! tests run in parallel.
//!
//! Only exec probes are exercised end to end: the tcp/http transports are
//! covered exhaustively by `src/health.rs`'s unit tests on real sockets, and
//! what these tests prove is the *wiring* (config → schedule → restart → state
//! file), for which an exec probe suffices. Standing a TCP server up from an
//! `sh` stub is not portable (`nc` differs across systems).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use supervisor_rs::state::{self, ProcState, StateSnapshot};

/// Deadline for the daemon to come up and publish its first snapshot. Generous
/// for a loaded CI runner.
const READY_TIMEOUT: Duration = Duration::from_secs(20);
/// Deadline for the daemon to shut down after being signalled.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(20);

fn state_path(dir: &Path) -> PathBuf {
    dir.join("state.toml")
}

fn socket_path(dir: &Path) -> PathBuf {
    // Short file name: sun_path is limited to ~108 bytes.
    dir.join("c.sock")
}

/// Starts the daemon with its state file and control socket inside the test's
/// own temp dir. stdout/stderr captured to a file so the config-error test can
/// read the log.
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
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "state file {} did not reach the expected state within {timeout:?}; last seen: {last}",
        path.display()
    );
}

/// Reads the current snapshot, or `None` if the file is absent/unparseable.
fn read_snapshot(path: &Path) -> Option<StateSnapshot> {
    state::read(path).ok()
}

/// Waits for the daemon to exit, SIGKILLing it on timeout so no stray daemon is
/// left behind.
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

fn is_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None).is_ok()
}

fn wait_until_gone(pid: i32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("pid {pid} still alive after {timeout:?}");
}

/// The acceptance criterion of Этап 7, end to end: a process with an exec probe
/// is left alone while healthy, restarted after the threshold when the probe
/// starts failing, and left alone again once it recovers.
#[test]
fn unhealthy_process_is_restarted_after_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let flag = dir.path().join("healthy");
    // Start healthy.
    std::fs::write(&flag, b"").unwrap();

    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[[process]]
name = "web"
restart = "always"
command = ["/usr/bin/env", "sh", "-c", "i=0; while [ $i -lt 6000 ]; do sleep 0.1; i=$((i+1)); done"]

[process.health-check]
type = "exec"
command = ["/usr/bin/env", "sh", "-c", "test -e {flag}"]
interval-secs = 1
timeout-secs = 5
failure-threshold = 2
start-period-secs = 0
"#,
            flag = flag.display()
        ),
    )
    .unwrap();

    let state_path = state_path(dir.path());
    let socket_path = socket_path(dir.path());
    let mut supervisor = start_supervisor(&config_path, &state_path, &socket_path);

    let snapshot = wait_for_state(&state_path, web_running, READY_TIMEOUT);
    let pid1 = snapshot.process[0].pid.expect("running process has a pid");

    // Healthy for at least 3 intervals: restart-count stays 0.
    std::thread::sleep(Duration::from_secs(3));
    let snapshot = read_snapshot(&state_path).expect("snapshot present");
    assert_eq!(
        snapshot.process[0].restart_count, 0,
        "a healthy process was restarted"
    );
    assert_eq!(snapshot.process[0].pid, Some(pid1), "healthy pid changed");

    // Make it unhealthy: the probe now fails.
    std::fs::remove_file(&flag).unwrap();

    // After threshold failures it restarts: new pid, restart-count grows.
    let snapshot = wait_for_state(
        &state_path,
        |s| {
            s.process[0].state == ProcState::Running
                && s.process[0].restart_count >= 1
                && s.process[0].pid.is_some_and(|pid| pid != pid1)
        },
        READY_TIMEOUT,
    );
    let pid2 = snapshot.process[0].pid.unwrap();
    assert_ne!(pid2, pid1);

    // Recovery: restore the flag, restarts stop. Read the count twice with a
    // gap of a few intervals and require it to stabilise.
    std::fs::write(&flag, b"").unwrap();
    // Let the new instance pass its start-period + a couple intervals.
    std::thread::sleep(Duration::from_secs(3));
    let count_a = read_snapshot(&state_path).unwrap().process[0].restart_count;
    std::thread::sleep(Duration::from_secs(3));
    let count_b = read_snapshot(&state_path).unwrap().process[0].restart_count;
    assert_eq!(
        count_a, count_b,
        "restart-count kept growing after recovery ({count_a} -> {count_b})"
    );

    // SIGTERM the daemon → exit 0, stubs dead.
    let daemon_pid = supervisor.id() as i32;
    let last = read_snapshot(&state_path).unwrap();
    let child_pid = last.process[0].pid;
    kill(Pid::from_raw(daemon_pid), Signal::SIGTERM).unwrap();
    let exit = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
    assert_eq!(exit.code(), Some(0));
    if let Some(cpid) = child_pid {
        wait_until_gone(cpid as i32, SHUTDOWN_TIMEOUT);
    }
}

/// A daemon probing frequently shuts down cleanly on SIGTERM — the probes do
/// not wedge the shutdown, and the state file and socket are removed.
#[test]
fn daemon_shuts_down_cleanly_while_probing() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[[process]]
name = "web"
restart = "always"
command = ["/usr/bin/env", "sh", "-c", "i=0; while [ $i -lt 6000 ]; do sleep 0.1; i=$((i+1)); done"]

[process.health-check]
type = "exec"
command = ["/usr/bin/env", "true"]
interval-secs = 1
timeout-secs = 5
failure-threshold = 3
start-period-secs = 0
"#,
    )
    .unwrap();

    let state_path = state_path(dir.path());
    let socket_path = socket_path(dir.path());
    let mut supervisor = start_supervisor(&config_path, &state_path, &socket_path);

    wait_for_state(&state_path, web_running, READY_TIMEOUT);
    // Let a few probe cycles happen.
    std::thread::sleep(Duration::from_secs(2));

    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM).unwrap();
    let exit = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
    assert_eq!(exit.code(), Some(0));
    assert!(
        !state_path.exists(),
        "state file was not removed on clean exit"
    );
    assert!(
        !socket_path.exists(),
        "socket was not removed on clean exit"
    );
}

/// An invalid health-check section (tcp without a port) fails `run` with a
/// config error (exit 1, the existing "config error" class), and no state file
/// is created — the daemon fell over before spawning anything.
#[test]
fn invalid_health_check_fails_run_with_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[[process]]
name = "web"
command = ["/usr/bin/env", "true"]

[process.health-check]
type = "tcp"
"#,
    )
    .unwrap();

    let state_path = state_path(dir.path());
    let socket_path = socket_path(dir.path());
    let out = Command::new(env!("CARGO_BIN_EXE_supervisor-rs"))
        .arg("run")
        .arg(&config_path)
        .arg("--state-file")
        .arg(&state_path)
        .arg("--control-socket")
        .arg(&socket_path)
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1), "expected a config-error exit 1");
    // The daemon logs the config error to stdout (tracing_subscriber::fmt).
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("health-check"),
        "config error did not mention health-check: {combined}"
    );
    assert!(
        !state_path.exists(),
        "a state file was created despite the config error"
    );
}
