//! End-to-end tests for Этап 9 log capture and rotation on the real
//! `supervisor-rs` binary: a chatty process with a `[process.log]` section
//! rotates its captured stdout end to end and the files survive shutdown; a
//! process without a section behaves as before; an invalid section fails `run`
//! with a config error.
//!
//! `write_config`/`start_supervisor`/`wait_for_state`/`wait_with_timeout`/
//! `wait_until_gone` are deliberately duplicated from `tests/health_e2e.rs`:
//! every integration test file is its own crate, and the existing files already
//! made that trade — see their preambles.
//!
//! Every daemon this file starts isolates **both** `--state-file` and
//! `--control-socket` in its own tempdir (defaults are one per uid, tests run in
//! parallel); log paths live in the same tempdir.
//!
//! Stubs write with the shell's `echo` only — each is a single unbuffered
//! `write(2)`; a C process writing to a pipe would buffer in blocks and the
//! output would appear late (flaky).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use supervisor_rs::state::{self, ProcState, StateSnapshot};

const READY_TIMEOUT: Duration = Duration::from_secs(20);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(20);

fn state_path(dir: &Path) -> PathBuf {
    dir.join("state.toml")
}

fn socket_path(dir: &Path) -> PathBuf {
    // Short file name: sun_path is limited to ~108 bytes.
    dir.join("c.sock")
}

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

/// Polls for a rotated `<path>.N` file to appear, with a real deadline.
fn wait_for_path(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("path {} did not appear within {timeout:?}", path.display());
}

/// Counts existing `<base>.N` rotated files in `dir` (N all digits).
fn rotated_count(dir: &Path, base: &str) -> usize {
    let prefix = format!("{base}.");
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.strip_prefix(&prefix)
                .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit()))
        })
        .count()
}

/// The acceptance criterion of Этап 9, end to end: a chatty process with a
/// `[process.log]` section (max-size 256, keep 2) rotates its captured stdout —
/// `.1` then `.2` appear, retention never exceeds keep — the daemon shuts down
/// cleanly on SIGTERM, and the log files survive on disk while the state file and
/// socket are removed.
#[test]
fn captured_output_rotates_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("web.stdout.log");
    let dot1 = dir.path().join("web.stdout.log.1");
    let dot2 = dir.path().join("web.stdout.log.2");

    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[[process]]
name = "web"
restart = "always"
command = ["/usr/bin/env", "sh", "-c", "while true; do echo rotate-me-marker; done"]

[process.log]
stdout-path = "{out}"
max-size-bytes = 256
keep = 2
"#,
            out = out.display()
        ),
    )
    .unwrap();

    let state_path = state_path(dir.path());
    let socket_path = socket_path(dir.path());
    let mut supervisor = start_supervisor(&config_path, &state_path, &socket_path);

    wait_for_state(&state_path, web_running, READY_TIMEOUT);

    // Rotation happens while alive: `.1`, then `.2` appear.
    wait_for_path(&dot1, READY_TIMEOUT);
    wait_for_path(&dot2, READY_TIMEOUT);
    assert!(
        rotated_count(dir.path(), "web.stdout.log") <= 2,
        "more rotated files than keep"
    );

    // SIGTERM → exit 0.
    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM).unwrap();
    let exit = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
    assert_eq!(exit.code(), Some(0));

    // Log files survive (decision §2.1); state file and socket are removed. The
    // rotated `.1` is durable — it existed before shutdown and rotation never
    // deletes what is still within `keep`. The *current* file is not asserted to
    // exist: if the very last action before EOF was a rotation, the current file
    // was just renamed to `.1` and the next (lazy) reopen never happened — a
    // legitimate state, not lost output. What matters is that captured bytes
    // remain on disk, which the rotated files carry.
    assert!(dot1.exists(), "rotated log .1 was removed on shutdown");
    let total: u64 = [out.as_path(), dot1.as_path(), dot2.as_path()]
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    assert!(total > 0, "concatenation of captured output is empty");
    assert!(
        !state_path.exists(),
        "state file was not removed on clean exit"
    );
    assert!(
        !socket_path.exists(),
        "socket was not removed on clean exit"
    );
}

/// A two-process config with a log section on only one process: the other runs
/// and never gets a log file. Guards against capturing every process by accident.
#[test]
fn process_without_log_section_behaves_as_before() {
    let dir = tempfile::tempdir().unwrap();
    let logged = dir.path().join("logged.stdout.log");
    let plain_log = dir.path().join("plain.stdout.log");

    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[[process]]
name = "logged"
restart = "always"
command = ["/usr/bin/env", "sh", "-c", "while true; do echo hi; sleep 0.1; done"]

[process.log]
stdout-path = "{logged}"

[[process]]
name = "plain"
restart = "always"
command = ["/usr/bin/env", "sh", "-c", "while true; do echo hi; sleep 0.1; done"]
"#,
            logged = logged.display()
        ),
    )
    .unwrap();

    let state_path = state_path(dir.path());
    let socket_path = socket_path(dir.path());
    let mut supervisor = start_supervisor(&config_path, &state_path, &socket_path);

    // Both processes running.
    wait_for_state(
        &state_path,
        |s| s.process.len() == 2 && s.process.iter().all(|p| p.state == ProcState::Running),
        READY_TIMEOUT,
    );
    // The logged process gets its file; the plain one never does.
    wait_for_path(&logged, READY_TIMEOUT);
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        !plain_log.exists(),
        "a log file appeared for the process without a log section"
    );

    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM).unwrap();
    let exit = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
    assert_eq!(exit.code(), Some(0));
}

/// An invalid log section (no path at all) fails `run` with a config error
/// (exit 1, the existing "config error" class), and no state file is created.
#[test]
fn invalid_log_section_fails_run_with_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[[process]]
name = "web"
command = ["/usr/bin/env", "true"]

[process.log]
max-size-bytes = 1024
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
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        combined.contains("stdout-path") || combined.contains("stderr-path"),
        "config error did not mention the missing log path: {combined}"
    );
    assert!(
        !state_path.exists(),
        "a state file was created despite the config error"
    );
}
