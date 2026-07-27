//! End-to-end tests for Этап 3: a signal sent to the real `supervisor-rs`
//! binary must be forwarded to the supervised children.
//!
//! The stub is run as `/usr/bin/env sh -c '<script>'` rather than a helper
//! executable written to disk: writing a file and `exec`-ing it moments later
//! races with `fork()` on the other threads of this test binary and fails
//! intermittently with ETXTBSY.
//!
//! Every test that waits for the supervisor uses `wait_with_timeout`. Этап 3
//! has no SIGKILL escalation (that is Этап 4), so a regression that leaves a
//! child alive would otherwise hang CI forever instead of failing red.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

/// The supervised stub.
///
/// The traps are armed *before* the pid file is written, so the appearance of
/// the pid file means "the trap is set" — that is the handshake the tests poll
/// on before signalling the supervisor. Without it a signal could be forwarded
/// to a shell that has not installed its traps yet and would die by the
/// default disposition, writing no marker.
///
/// The body is a loop of short `sleep`s rather than one long one because `sh`
/// only runs a trap between commands: with `sleep 10` the reaction would be
/// delayed by up to ten seconds.
///
/// `>>` on the pid file makes the number of lines the number of stub starts,
/// which is how the "shutdown suppresses restarts" test counts respawns.
///
/// The iteration bound exists only so that a stub leaked by a failing test does
/// not live forever; it is deliberately far longer than any test's deadlines
/// (~60 s against 5 s), because a stub that outlives its own test would look
/// exactly like a bug — a missing marker under `never`, or a second pid line
/// under `always`.
const STUB_SCRIPT: &str = r#"trap 'printf %s TERM > "$SUP_MARKER"; exit 0' TERM
trap 'printf %s INT > "$SUP_MARKER"; exit 0' INT
echo $$ >> "$SUP_PIDFILE"
i=0
while [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done
"#;

const MARKER: &str = "marker";
const PIDFILE: &str = "pids";

/// Deadline for the supervisor to shut down after being signalled.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Deadline for the stub to arm its traps and report its pid. Generous on
/// purpose: it covers a cold exec of the debug binary plus fork/exec of
/// `env`→`sh` on a runner busy with the other tests, and it only bounds a
/// failure — the happy path returns as soon as the pid file parses.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

fn write_config(dir: &Path, restart_policy: &str) -> PathBuf {
    let config_path = dir.join("config.toml");
    // The script goes in as a TOML multi-line literal string ('''...'''): it
    // contains both quote kinds and `$`, none of which need escaping there.
    let config = format!(
        r#"
[[process]]
name = "stub"
restart = "{restart_policy}"
command = ["/usr/bin/env", "sh", "-c", '''
{STUB_SCRIPT}''']
env = {{ SUP_MARKER = "{marker}", SUP_PIDFILE = "{pidfile}" }}
"#,
        marker = dir.join(MARKER).display(),
        pidfile = dir.join(PIDFILE).display(),
    );
    std::fs::write(&config_path, config).unwrap();
    config_path
}

fn start_supervisor(config_path: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_supervisor-rs"))
        .arg(config_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Waits until the stub's pid file holds a complete, parseable pid, and returns
/// it.
///
/// Polling for mere *existence* would not be enough: `echo $$ >> file` is an
/// `open(O_CREAT|O_APPEND)` followed by a separate `write()`, so between the
/// two syscalls the file exists and is empty. A poll landing in that window —
/// widened to milliseconds if the scheduler preempts `sh` in between — would
/// read nothing and fail to parse. Waiting for the content closes the window.
fn wait_for_pid(path: &Path, timeout: Duration) -> i32 {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Some(pid) = text.lines().next().and_then(|l| l.trim().parse().ok()) {
                return pid;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("stub did not report its pid within {timeout:?}");
}

/// Waits for the supervisor to exit. On timeout it SIGKILLs the supervisor so
/// the test run is not left with a stray daemon, then fails the test.
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

fn signal_supervisor(supervisor: &Child, sig: Signal) {
    kill(Pid::from_raw(supervisor.id() as i32), sig).unwrap();
}

/// Starts the supervisor and waits until the stub has armed its traps, then
/// returns the supervisor and the stub's pid.
fn start_and_wait_ready(dir: &Path, restart_policy: &str) -> (Child, i32) {
    let config_path = write_config(dir, restart_policy);
    let supervisor = start_supervisor(&config_path);
    let stub_pid = wait_for_pid(&dir.join(PIDFILE), READY_TIMEOUT);
    (supervisor, stub_pid)
}

#[test]
fn sigterm_is_forwarded_to_child() {
    let dir = tempfile::tempdir().unwrap();
    let (mut supervisor, _) = start_and_wait_ready(dir.path(), "never");

    signal_supervisor(&supervisor, Signal::SIGTERM);
    wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);

    let marker = std::fs::read_to_string(dir.path().join(MARKER)).unwrap();
    assert_eq!(marker, "TERM");
}

#[test]
fn sigint_is_forwarded_to_child() {
    let dir = tempfile::tempdir().unwrap();
    let (mut supervisor, _) = start_and_wait_ready(dir.path(), "never");

    signal_supervisor(&supervisor, Signal::SIGINT);
    wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);

    let marker = std::fs::read_to_string(dir.path().join(MARKER)).unwrap();
    assert_eq!(marker, "INT");
}

/// The acceptance criterion of Этап 3: the supervisor must not exit leaving a
/// live child behind.
#[test]
fn child_is_not_orphaned_after_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let (mut supervisor, pid) = start_and_wait_ready(dir.path(), "never");

    signal_supervisor(&supervisor, Signal::SIGTERM);
    wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);

    // Signal 0 only checks for the pid's existence. The child was reaped by
    // the supervisor, so it is not even a zombie; ESRCH is the only correct
    // answer here.
    assert_eq!(kill(Pid::from_raw(pid), None), Err(Errno::ESRCH));
}

#[test]
fn supervisor_exits_zero_after_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let (mut supervisor, _) = start_and_wait_ready(dir.path(), "never");

    signal_supervisor(&supervisor, Signal::SIGTERM);
    let status = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);

    // A shutdown by signal is a normal stop, not a failure — see the exit-code
    // rationale in main.rs.
    assert_eq!(status.code(), Some(0));
}

/// Without shutdown mode the forwarded SIGTERM would kill the stub, the loop
/// would see the exit, the `always` policy would respawn it and the supervisor
/// would never terminate.
#[test]
fn shutdown_suppresses_restart_with_always_policy() {
    let dir = tempfile::tempdir().unwrap();
    let (mut supervisor, _) = start_and_wait_ready(dir.path(), "always");

    signal_supervisor(&supervisor, Signal::SIGTERM);
    wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);

    let pids = std::fs::read_to_string(dir.path().join(PIDFILE)).unwrap();
    assert_eq!(
        pids.lines().count(),
        1,
        "the stub was restarted during shutdown: {pids:?}"
    );
}
