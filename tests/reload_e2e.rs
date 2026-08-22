//! End-to-end tests for the Этап 8 config reload on the real `supervisor-rs`
//! binary: SIGHUP re-reads the config file and applies the diff (keep / change
//! / remove / add) without a daemon restart; a broken config on SIGHUP changes
//! nothing and does not crash the daemon; a SIGHUP with no file change is a
//! no-op.
//!
//! `write_config`/`start_supervisor`/`wait_for_state`/`wait_with_timeout` are
//! deliberately duplicated from `tests/health_e2e.rs`/`tests/control.rs`: every
//! integration test file is its own crate, and the existing files already made
//! that trade — see their preambles.
//!
//! Every daemon isolates **both** `--state-file` and `--control-socket` in its
//! own tempdir (default paths are one per uid, tests run in parallel), and
//! socket file names are short (`sun_path` is ~108 bytes).
//!
//! **Handshake before `kill -HUP` is by the state file only.**
//! `install_handlers` runs in `main.rs` before the first spawn, but the first
//! snapshot is published later, from `run()` — so "wait_for_state saw the
//! processes" ⟹ "the SIGHUP handler is already installed". A HUP sent before
//! the handler is installed would kill the daemon by SIGHUP's default
//! disposition, an indistinguishable flake.

use std::io::Read;
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
    dir.join("c.sock")
}

/// Starts the daemon with its state file and control socket in the test's temp
/// dir; stdout+stderr are redirected to `log_path` so the tests can poll the
/// daemon's log (`tracing_subscriber::fmt` writes to stdout).
fn start_supervisor(
    config_path: &Path,
    state_path: &Path,
    socket_path: &Path,
    log_path: &Path,
) -> Child {
    let log = std::fs::File::create(log_path).unwrap();
    let log_err = log.try_clone().unwrap();
    Command::new(env!("CARGO_BIN_EXE_supervisor-rs"))
        .arg("run")
        .arg(config_path)
        .arg("--state-file")
        .arg(state_path)
        .arg("--control-socket")
        .arg(socket_path)
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
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

fn read_snapshot(path: &Path) -> Option<StateSnapshot> {
    state::read(path).ok()
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

/// Finds a process by name in a snapshot.
fn find<'a>(snapshot: &'a StateSnapshot, name: &str) -> Option<&'a state::ProcessState> {
    snapshot.process.iter().find(|p| p.name == name)
}

/// Whether every named process is in the `Running` state.
fn all_running<'a>(names: &'a [&'a str]) -> impl Fn(&StateSnapshot) -> bool + 'a {
    move |snapshot| {
        names
            .iter()
            .all(|n| find(snapshot, n).is_some_and(|p| p.state == ProcState::Running))
    }
}

/// Polls the daemon log until it contains `needle`.
fn wait_for_log(log_path: &Path, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    while Instant::now() < deadline {
        if let Ok(mut f) = std::fs::File::open(log_path) {
            let mut text = String::new();
            if f.read_to_string(&mut text).is_ok() && text.contains(needle) {
                return;
            }
            last = text;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "log {} never contained {needle:?}; last:\n{last}",
        log_path.display()
    );
}

/// A `[[process]]` block for a long-sleeping stub.
fn sleeper(name: &str) -> String {
    format!(
        r#"
[[process]]
name = "{name}"
restart = "always"
command = ["/usr/bin/env", "sh", "-c", "i=0; while [ $i -lt 6000 ]; do sleep 0.1; i=$((i+1)); done"]
"#
    )
}

/// A stub with a differing argv (extra echo) so it is a "changed" config.
fn changed_sleeper(name: &str) -> String {
    format!(
        r#"
[[process]]
name = "{name}"
restart = "always"
command = ["/usr/bin/env", "sh", "-c", "echo v2; i=0; while [ $i -lt 6000 ]; do sleep 0.1; i=$((i+1)); done"]
"#
    )
}

/// The whole acceptance criterion in one reload: keep unchanged, change one,
/// remove one, add one.
#[test]
fn sighup_applies_diff() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let log_path = dir.path().join("daemon.log");

    // Config A = {keep, change, gone}.
    std::fs::write(
        &config_path,
        format!(
            "{}{}{}",
            sleeper("keep"),
            sleeper("change"),
            sleeper("gone")
        ),
    )
    .unwrap();

    let state_path = state_path(dir.path());
    let socket_path = socket_path(dir.path());
    let mut supervisor = start_supervisor(&config_path, &state_path, &socket_path, &log_path);

    let snapshot = wait_for_state(
        &state_path,
        all_running(&["keep", "change", "gone"]),
        READY_TIMEOUT,
    );
    let keep_pid = find(&snapshot, "keep").unwrap().pid.unwrap();
    let change_pid = find(&snapshot, "change").unwrap().pid.unwrap();
    let gone_pid = find(&snapshot, "gone").unwrap().pid.unwrap();

    // Config B = {keep unchanged, change with a new command, new added}.
    std::fs::write(
        &config_path,
        format!(
            "{}{}{}",
            sleeper("keep"),
            changed_sleeper("change"),
            sleeper("new")
        ),
    )
    .unwrap();

    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGHUP).unwrap();

    // Wait until the diff has fully applied: change has a new pid, new is
    // running, gone is absent.
    let snapshot = wait_for_state(
        &state_path,
        |s| {
            find(s, "keep").is_some_and(|p| p.state == ProcState::Running)
                && find(s, "change")
                    .is_some_and(|p| p.state == ProcState::Running && p.pid != Some(change_pid))
                && find(s, "new").is_some_and(|p| p.state == ProcState::Running)
                && find(s, "gone").is_none()
        },
        READY_TIMEOUT,
    );

    // keep: same pid, restart-count 0.
    let keep = find(&snapshot, "keep").unwrap();
    assert_eq!(keep.pid, Some(keep_pid), "keep restarted");
    assert_eq!(keep.restart_count, 0);
    // change: new pid, restart-count >= 1.
    let change = find(&snapshot, "change").unwrap();
    assert_ne!(change.pid, Some(change_pid));
    assert!(change.restart_count >= 1);
    // gone: absent, tree dead.
    wait_until_gone(gone_pid as i32, SHUTDOWN_TIMEOUT);
    // new: running.
    assert_eq!(find(&snapshot, "new").unwrap().state, ProcState::Running);

    // SIGTERM → exit 0, all stubs dead, files removed.
    let keep_pid_now = find(&snapshot, "keep").unwrap().pid.unwrap();
    let change_pid_now = find(&snapshot, "change").unwrap().pid.unwrap();
    let new_pid_now = find(&snapshot, "new").unwrap().pid.unwrap();
    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM).unwrap();
    let exit = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
    assert_eq!(exit.code(), Some(0));
    for pid in [keep_pid_now, change_pid_now, new_pid_now] {
        wait_until_gone(pid as i32, SHUTDOWN_TIMEOUT);
    }
    assert!(!state_path.exists(), "state file not removed on clean exit");
    assert!(!socket_path.exists(), "socket not removed on clean exit");
}

/// A broken config on SIGHUP changes nothing and does not crash the daemon; a
/// subsequent valid change still applies (the daemon did not wedge).
#[test]
fn sighup_with_broken_config_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let log_path = dir.path().join("daemon.log");

    std::fs::write(&config_path, sleeper("web")).unwrap();

    let state_path = state_path(dir.path());
    let socket_path = socket_path(dir.path());
    let mut supervisor = start_supervisor(&config_path, &state_path, &socket_path, &log_path);

    let snapshot = wait_for_state(&state_path, all_running(&["web"]), READY_TIMEOUT);
    let web_pid = find(&snapshot, "web").unwrap().pid.unwrap();

    // Garbage config → SIGHUP → the reload is rejected, nothing changes.
    std::fs::write(&config_path, b"[[process\n").unwrap();
    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGHUP).unwrap();
    wait_for_log(&log_path, "config reload failed", READY_TIMEOUT);

    let snapshot = read_snapshot(&state_path).unwrap();
    assert_eq!(
        find(&snapshot, "web").unwrap().pid,
        Some(web_pid),
        "broken reload changed the pid"
    );
    assert!(is_alive(web_pid as i32), "the daemon killed the child");
    assert!(
        supervisor.try_wait().unwrap().is_none(),
        "the daemon died on a broken reload"
    );

    // A valid change now applies → proves the daemon did not wedge.
    std::fs::write(&config_path, changed_sleeper("web")).unwrap();
    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGHUP).unwrap();
    let snapshot = wait_for_state(
        &state_path,
        |s| find(s, "web").is_some_and(|p| p.state == ProcState::Running && p.pid != Some(web_pid)),
        READY_TIMEOUT,
    );
    let new_pid = find(&snapshot, "web").unwrap().pid.unwrap();
    assert_ne!(new_pid, web_pid);

    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM).unwrap();
    let exit = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
    assert_eq!(exit.code(), Some(0));
}

/// A SIGHUP with no file change is a no-op: pids and restart-counts are stable
/// across two snapshot reads, and the log says "no changes". Cheap, and it
/// catches a stray "restart everything on every HUP".
#[test]
fn sighup_before_any_change_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    let log_path = dir.path().join("daemon.log");

    std::fs::write(&config_path, format!("{}{}", sleeper("a"), sleeper("b"))).unwrap();

    let state_path = state_path(dir.path());
    let socket_path = socket_path(dir.path());
    let mut supervisor = start_supervisor(&config_path, &state_path, &socket_path, &log_path);

    let snapshot = wait_for_state(&state_path, all_running(&["a", "b"]), READY_TIMEOUT);
    let a_pid = find(&snapshot, "a").unwrap().pid.unwrap();
    let b_pid = find(&snapshot, "b").unwrap().pid.unwrap();

    // HUP without touching the file.
    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGHUP).unwrap();
    wait_for_log(&log_path, "config reload: no changes", READY_TIMEOUT);

    // Two reads with a gap: pids and restart-counts stable.
    let s1 = read_snapshot(&state_path).unwrap();
    std::thread::sleep(Duration::from_millis(500));
    let s2 = read_snapshot(&state_path).unwrap();
    for s in [&s1, &s2] {
        assert_eq!(
            find(s, "a").unwrap().pid,
            Some(a_pid),
            "a restarted on a no-op HUP"
        );
        assert_eq!(
            find(s, "b").unwrap().pid,
            Some(b_pid),
            "b restarted on a no-op HUP"
        );
        assert_eq!(find(s, "a").unwrap().restart_count, 0);
        assert_eq!(find(s, "b").unwrap().restart_count, 0);
    }

    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM).unwrap();
    let exit = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
    assert_eq!(exit.code(), Some(0));
}
