//! End-to-end tests for Этап 4 on the real `supervisor-rs` binary: a signal to
//! the supervisor must take down the whole *tree* of processes a child forked,
//! and a child that ignores the shutdown signal must be SIGKILLed.
//!
//! The helpers below (`write_config`, `start_supervisor`, `wait_with_timeout`,
//! `signal_supervisor`) are deliberately duplicated from `tests/signals.rs`:
//! every integration test file is its own crate, and wiring a shared module in
//! for ~40 lines would add indirection to tests that double as documentation.
//!
//! As in `tests/signals.rs`, the stubs run via `/usr/bin/env sh -c '<script>'`
//! rather than an executable written to disk (that races with `fork()` on the
//! other test threads and fails intermittently with ETXTBSY), the traps are
//! armed *before* the pid file is written so that the pid file is a handshake,
//! and long-lived stubs loop over short `sleep`s because `sh` only runs a trap
//! between commands.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

/// A supervised process that forks two children of its own.
///
/// The grandchildren ignore SIGTERM on purpose. With a plain `spin` they would
/// die from the same `killpg(SIGTERM)` that kills the leader, and the test
/// would pass even with the SIGKILL sweep removed entirely — it would only
/// prove group-wide forwarding. Deaf to SIGTERM, their sole path to death is
/// the sweep that follows the leader's exit, which is the mechanism this stage
/// exists for. The leader arms no trap and dies on the first SIGTERM, which is
/// what triggers the sweep.
///
/// The parent writes its own pid *last*: "the file holds 3 pids" therefore
/// means both grandchildren have already been forked.
///
/// Each grandchild reports readiness itself, *after* arming its trap, and the
/// test waits for both reports. The pid file alone cannot serve as that
/// handshake: `echo $!` is written by the *parent* right after `&`, with no
/// synchronisation with whether the subshell has reached its `trap` yet. A
/// grandchild descheduled between fork and trap would die from the forwarded
/// SIGTERM, and the test would stay green while quietly proving only group
/// forwarding — a silent loss of coverage, which is worse than a flake because
/// no run ever reports it. (`$$` inside `( )` is the *parent's* pid in POSIX
/// sh, so a grandchild cannot report its own pid; a separate marker file is the
/// way.)
const TREE_SCRIPT: &str = r#"spin() { i=0; while [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done; }
( trap '' TERM; echo ready >> "$SUP_READY"; spin ) &
echo $! >> "$SUP_PIDFILE"
( trap '' TERM; echo ready >> "$SUP_READY"; spin ) &
echo $! >> "$SUP_PIDFILE"
echo $$ >> "$SUP_PIDFILE"
spin
"#;

/// A single process that ignores SIGTERM, so only the SIGKILL escalation can
/// stop it. The trap is armed before the pid is written, so the handshake
/// guarantees the shutdown signal lands on an already-deaf child.
const DEAF_SCRIPT: &str = r#"trap '' TERM
echo $$ >> "$SUP_PIDFILE"
i=0
while [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done
"#;

const PIDFILE: &str = "pids";
const READYFILE: &str = "ready";
const LOGFILE: &str = "supervisor.log";
const STATEFILE: &str = "state.toml";
/// Short name: `sun_path` is limited to ~108 bytes and a tempdir path plus a
/// long socket name can overflow it with a loud bind error.
const SOCKFILE: &str = "c.sock";

/// Deadline for the supervisor to shut down after being signalled.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// Deadline for the stubs to arm their traps and report their pids.
const READY_TIMEOUT: Duration = Duration::from_secs(5);
/// Deadline for killed processes to disappear from the process table.
const GONE_TIMEOUT: Duration = Duration::from_secs(5);

fn write_config(dir: &Path, script: &str, restart_policy: &str, stop_grace_secs: u64) -> PathBuf {
    let config_path = dir.join("config.toml");
    // The script goes in as a TOML multi-line literal string ('''...'''): it
    // contains both quote kinds and `$`, none of which need escaping there.
    let config = format!(
        r#"
[[process]]
name = "stub"
restart = "{restart_policy}"
stop-grace-secs = {stop_grace_secs}
command = ["/usr/bin/env", "sh", "-c", '''
{script}''']
env = {{ SUP_PIDFILE = "{pidfile}", SUP_READY = "{readyfile}" }}
"#,
        pidfile = dir.join(PIDFILE).display(),
        readyfile = dir.join(READYFILE).display(),
    );
    std::fs::write(&config_path, config).unwrap();
    config_path
}

/// Starts the daemon with its state file and control socket inside the test's
/// own temp dir.
///
/// The isolation is mandatory since Этап 5 for the state file, and since
/// Этап 6 for the control socket, both for the same reason: the default paths
/// are one per uid, so parallel tests would collide on them. For the socket
/// specifically, `ControlServer::bind` in `main.rs` runs *before* the first
/// spawn and refuses to start a second daemon on a socket a live one already
/// owns — without `--control-socket` here, every test in this file but the
/// first to bind would exit 1 immediately and never write its pid file.
fn start_supervisor(config_path: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_supervisor-rs"))
        .arg("run")
        .arg(config_path)
        .arg("--state-file")
        .arg(config_path.with_file_name(STATEFILE))
        .arg("--control-socket")
        .arg(config_path.with_file_name(SOCKFILE))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Same, but with the supervisor's output captured to `log_path` so a test can
/// wait for a specific log line instead of sleeping. Both streams are
/// redirected: `tracing_subscriber::fmt` writes to stdout, while a usage error
/// would go to stderr, and a test debugging a failure wants either.
fn start_supervisor_logging(config_path: &Path, log_path: &Path) -> Child {
    let log = std::fs::File::create(log_path).unwrap();
    let log_err = log.try_clone().unwrap();
    Command::new(env!("CARGO_BIN_EXE_supervisor-rs"))
        .arg("run")
        .arg(config_path)
        .arg("--state-file")
        .arg(config_path.with_file_name(STATEFILE))
        .arg("--control-socket")
        .arg(config_path.with_file_name(SOCKFILE))
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .spawn()
        .unwrap()
}

/// Waits until `path` holds exactly `n` fully parseable pid lines and returns
/// them.
///
/// Every line is parsed, not just the first: `echo $! >> file` is an `open`
/// followed by a separate `write`, so a poll can land on a file that already
/// has the line count but not yet the last number. Requiring all `n` lines to
/// parse closes that window.
fn wait_for_pids(path: &Path, n: usize, timeout: Duration) -> Vec<i32> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            let pids: Vec<i32> = text
                .lines()
                .filter_map(|line| line.trim().parse().ok())
                .collect();
            if pids.len() == n && text.lines().count() == n {
                return pids;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("stub did not report {n} pids within {timeout:?}");
}

/// Waits until `path` holds at least `n` lines.
///
/// Used for the grandchildren's readiness reports, which carry no payload —
/// only the fact that the trap is armed — so counting lines is enough.
fn wait_for_lines(path: &Path, n: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if text.lines().count() >= n {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("stub did not report {n} ready lines within {timeout:?}");
}

/// Polls until none of `pids` exists any more; returns the survivors on
/// timeout.
///
/// A single `assert` right after the supervisor exits would be a guaranteed
/// flake for grandchildren: the supervisor reaps only its *direct* child, while
/// a killed grandchild stays a zombie — and `kill(pid, 0)` on a zombie
/// succeeds — until init gets around to reaping it, asynchronously and possibly
/// after the supervisor is already gone.
fn wait_until_gone(pids: &[i32], timeout: Duration) -> Result<(), Vec<i32>> {
    let deadline = Instant::now() + timeout;
    loop {
        let alive: Vec<i32> = pids
            .iter()
            .copied()
            .filter(|pid| kill(Pid::from_raw(*pid), None) != Err(Errno::ESRCH))
            .collect();
        if alive.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(alive);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Waits until the supervisor's captured log contains `needle`.
fn wait_for_log(path: &Path, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if text.contains(needle) {
                return;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("supervisor log did not contain {needle:?} within {timeout:?}");
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

/// The acceptance criterion of Этап 4: shutting the supervisor down leaves no
/// live descendant behind — not the direct child, not the two grandchildren it
/// forked, which the supervisor never knew about and cannot even reap.
///
/// ESRCH is the assertion because it means "no such process at all": neither
/// alive nor a zombie. The grandchildren's zombies belong to init, so counting
/// zombies from here is impossible; their disappearance is the observable.
#[test]
fn shutdown_kills_whole_tree() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = write_config(dir.path(), TREE_SCRIPT, "never", 5);
    let mut supervisor = start_supervisor(&config_path);
    let pids = wait_for_pids(&dir.path().join(PIDFILE), 3, READY_TIMEOUT);
    // Both grandchildren must be deaf to SIGTERM *before* the signal goes out,
    // otherwise the sweep this test exists to prove is never exercised — see
    // TREE_SCRIPT.
    wait_for_lines(&dir.path().join(READYFILE), 2, READY_TIMEOUT);

    signal_supervisor(&supervisor, Signal::SIGTERM);
    let status = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
    assert_eq!(status.code(), Some(0));

    if let Err(alive) = wait_until_gone(&pids, GONE_TIMEOUT) {
        panic!("processes survived the shutdown: {alive:?} (tree was {pids:?})");
    }
}

/// The SIGKILL path on real time: the child ignores SIGTERM, so the supervisor
/// can only stop it by escalating once `stop-grace-secs` has passed. A grace of
/// 1 s against the 5 s deadline leaves 4 s of slack.
#[test]
fn sigkill_escalation_when_child_ignores_sigterm() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = write_config(dir.path(), DEAF_SCRIPT, "never", 1);
    let mut supervisor = start_supervisor(&config_path);
    let pids = wait_for_pids(&dir.path().join(PIDFILE), 1, READY_TIMEOUT);

    signal_supervisor(&supervisor, Signal::SIGTERM);
    let status = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
    assert_eq!(status.code(), Some(0));

    if let Err(alive) = wait_until_gone(&pids, GONE_TIMEOUT) {
        panic!("the deaf stub survived the escalation: {alive:?}");
    }
}

/// A second shutdown signal skips the remaining grace. The configured grace is
/// far longer than the test's own deadline, so the only way the supervisor can
/// exit in time is the escalation.
///
/// The second signal is sent only after the first one shows up in the log:
/// `PENDING` holds just the latest signal, so two signals delivered before the
/// first `take_pending()` would coalesce into one and the test would hang
/// flakily. If it hangs anyway, `wait_with_timeout` SIGKILLs the supervisor and
/// the stub dies on its own iteration bound — nothing leaks.
#[test]
fn second_signal_escalates_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = write_config(dir.path(), DEAF_SCRIPT, "never", 30);
    let log_path = dir.path().join(LOGFILE);
    let mut supervisor = start_supervisor_logging(&config_path, &log_path);
    let pids = wait_for_pids(&dir.path().join(PIDFILE), 1, READY_TIMEOUT);

    signal_supervisor(&supervisor, Signal::SIGTERM);
    wait_for_log(&log_path, "shutdown requested", READY_TIMEOUT);

    signal_supervisor(&supervisor, Signal::SIGTERM);
    let status = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
    assert_eq!(status.code(), Some(0));

    if let Err(alive) = wait_until_gone(&pids, GONE_TIMEOUT) {
        // Dump the log: a failure here is about escalation ordering, and the
        // supervisor's own view is the only way to tell which step was missed.
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        std::io::stderr().write_all(log.as_bytes()).unwrap();
        panic!("the deaf stub survived the second signal: {alive:?}");
    }
}
