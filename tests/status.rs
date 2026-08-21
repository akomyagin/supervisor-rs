//! End-to-end tests for Этап 5 on the real `supervisor-rs` binary: what
//! `status` prints while a daemon is running, and how it reports the three ways
//! there is nothing to print.
//!
//! `write_config`, `start_supervisor` and `wait_with_timeout` are deliberately
//! duplicated from `tests/signals.rs`: every integration test file is its own
//! crate, and the existing files already made that trade — see their preambles.
//!
//! The handshake is the **state file** rather than a pid file written by the
//! stub. A fourth copy of `wait_for_pid` would have tripped the Этап 4 debt
//! trigger ("three copies with diverged semantics; revisit on the fourth"), and
//! waiting for the snapshot is the more honest signal anyway: it means "the
//! daemon is up *and* has published", which is exactly the precondition of
//! `status`.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use supervisor_rs::state::{self, ProcState, ProcessState, StateSnapshot, STATE_VERSION};

const STATEFILE: &str = "state.toml";

/// Deadline for the daemon to come up and publish its first snapshot.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// Deadline for the daemon to shut down after being signalled.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// A stub that outlives the test unless it is signalled. `env` execs `sleep`
/// directly, so the supervised pid is the sleeping process itself and the
/// default SIGTERM disposition ends it.
fn write_config(dir: &Path) -> PathBuf {
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[[process]]
name = "web"
restart = "never"
command = ["/usr/bin/env", "sleep", "100"]
"#,
    )
    .unwrap();
    config_path
}

fn state_path(dir: &Path) -> PathBuf {
    dir.join(STATEFILE)
}

/// Starts the daemon with its state file inside the test's own temp dir — see
/// the note in `tests/cli.rs` on why the isolation is mandatory.
fn start_supervisor(config_path: &Path, state_path: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_supervisor-rs"))
        .arg("run")
        .arg(config_path)
        .arg("--state-file")
        .arg(state_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn status_command(state_path: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_supervisor-rs"))
        .arg("status")
        .arg("--state-file")
        .arg(state_path)
        .output()
        .unwrap()
}

/// Polls the state file until it parses *and* satisfies `pred`.
///
/// Every error is retried: the daemon may not have published yet. The atomic
/// rename is what makes a persistent parse error meaningful — this loop would
/// otherwise be papering over torn reads.
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

fn snapshot_with(daemon_pid: u32, version: u32) -> StateSnapshot {
    StateSnapshot {
        version,
        daemon_pid,
        written_at_unix_secs: 0,
        process: vec![ProcessState {
            name: "web".to_string(),
            state: ProcState::Running,
            pid: Some(daemon_pid),
            restart_count: 0,
            uptime_secs: Some(1),
        }],
    }
}

/// The acceptance criterion of Этап 5, end to end: `status` on a live daemon
/// prints a line per process with its name, state and pid; once the daemon is
/// gone it says so and exits 1.
#[test]
fn status_shows_running_process_and_detects_daemon_exit() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = write_config(dir.path());
    let state_path = state_path(dir.path());
    let mut supervisor = start_supervisor(&config_path, &state_path);

    // The published snapshot doubles as the "daemon is ready" handshake.
    let snapshot = wait_for_state(
        &state_path,
        |snapshot| {
            snapshot
                .process
                .first()
                .is_some_and(|proc| proc.state == ProcState::Running)
        },
        READY_TIMEOUT,
    );
    let child_pid = snapshot.process[0]
        .pid
        .expect("a running process has a pid");
    assert_eq!(snapshot.daemon_pid, supervisor.id());

    let out = status_command(&state_path);
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("web"), "{stdout}");
    assert!(stdout.contains("running"), "{stdout}");
    assert!(stdout.contains(&child_pid.to_string()), "{stdout}");

    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM).unwrap();
    let exit = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
    assert_eq!(exit.code(), Some(0));

    // The removal happens inside run(), before the process exits, so after
    // `wait` this is a deterministic single assertion, not a poll.
    assert!(
        !state_path.exists(),
        "the state file outlived the daemon it belonged to"
    );

    let out = status_command(&state_path);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not running"), "{stderr}");
}

/// A file left behind by a daemon that died without cleaning up (SIGKILL,
/// panic) must not be reported as live state.
#[test]
fn status_reports_stale_state_file_of_dead_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = state_path(dir.path());

    // A direct child we reaped ourselves: the pid is certainly gone, with no
    // zombie window. (Pid reuse could in principle resurrect it, but that would
    // make the test fail rather than pass silently.)
    let mut dead = Command::new("/usr/bin/env").arg("true").spawn().unwrap();
    let dead_pid = dead.id();
    dead.wait().unwrap();

    state::write_atomic(&state_path, &snapshot_with(dead_pid, STATE_VERSION)).unwrap();

    let out = status_command(&state_path);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("stale"), "{stderr}");
}

/// A snapshot from a future schema version is refused outright rather than
/// interpreted through today's field names.
#[test]
fn status_errors_on_unsupported_version() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = state_path(dir.path());
    state::write_atomic(&state_path, &snapshot_with(std::process::id(), 99)).unwrap();

    let out = status_command(&state_path);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unsupported version"), "{stderr}");
}
