//! In-process tests for Этап 9 log capture and rotation: `SupervisorLoop`
//! driven directly with `tick()` and a `FakeClock`, processes writing to real
//! captured pipes read by real OS reader threads.
//!
//! `cfg`/`sh`/`DEAF_SCRIPT`/`wait_for_pid`/`is_alive`/`get_pid`/
//! `tick_until_reaped`/`tick_until_new_pid` are duplicated from `tests/health.rs`
//! — every integration test file is its own crate and the existing files already
//! made that trade (see their preambles).
//!
//! **Two-mode observation of the log file** — the first departure from "the whole
//! supervisor is one thread on a `FakeClock`":
//! - **process alive** → data is written from another OS thread, asynchronously
//!   to the main loop; the only correct assertion is a **poll of the file with a
//!   real deadline** (`wait_for_log`), on *content*, not existence (the same
//!   discipline as `wait_for_pid`);
//! - **leader reaped** (after `tick_until_reaped` / an `Exited` handled) → the
//!   reader threads have been bounded-joined, the file is complete and rotation
//!   applied, so a **synchronous assertion without polling** is correct. That is
//!   the observable consequence of the "join at reap" decision.
//!
//! Stub output is only `echo`/`printf` of the shell: each is a single `write(2)`
//! with no stdio buffering (a C process writing to a pipe would buffer in blocks
//! and the output would appear late — flaky).

use std::collections::BTreeMap;
use std::num::{NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use supervisor_rs::clock::FakeClock;
use supervisor_rs::config::{LogConfig, ProcessConfig, RestartPolicy, DEFAULT_STOP_GRACE_SECS};
use supervisor_rs::control::Request;
use supervisor_rs::supervise::SupervisorLoop;

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
        rlimit: None,
        cgroup: None,
    }
}

fn sh(script: &str, restart: RestartPolicy) -> ProcessConfig {
    cfg("web", &["/usr/bin/env", "sh", "-c", script], restart)
}

/// Attaches a `[process.log]` section to a config (built through `toml::from_str`
/// like `with_exec_health`, then overriding the size/keep so tests can use tiny
/// limits without repeating the raw-parse dance).
fn with_log(
    mut c: ProcessConfig,
    stdout: Option<&Path>,
    stderr: Option<&Path>,
    max_size: u64,
    keep: u32,
) -> ProcessConfig {
    c.log = Some(LogConfig {
        stdout_path: stdout.map(PathBuf::from),
        stderr_path: stderr.map(PathBuf::from),
        max_size_bytes: NonZeroU64::new(max_size).expect("max_size non-zero"),
        keep: NonZeroU32::new(keep).expect("keep non-zero"),
    });
    c
}

/// A stub deaf to SIGTERM that echoes markers to stdout and reports its pid — only
/// the SIGKILL escalation stops it. The trap is armed *before* the pid file is
/// written, so a parseable pid line is a true handshake for "the trap is live".
const DEAF_SCRIPT: &str = r#"trap '' TERM
echo $$ >> "$SUP_PIDFILE"
i=0
while [ $i -lt 600 ]; do echo deaf-marker; sleep 0.1; i=$((i+1)); done
"#;

fn deaf(pidfile: &Path, stop_grace_secs: u64) -> ProcessConfig {
    let mut c = sh(DEAF_SCRIPT, RestartPolicy::Never);
    c.env = Some(BTreeMap::from([(
        "SUP_PIDFILE".to_string(),
        pidfile.display().to_string(),
    )]));
    c.stop_grace_secs = stop_grace_secs;
    c
}

/// Waits until the stub's pid file holds a complete, parseable pid, and returns
/// it. Polling for mere existence would race the `open`/`write` gap.
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

fn is_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None) != Err(Errno::ESRCH)
}

fn get_pid(loop_: &SupervisorLoop<FakeClock>, index: usize) -> Option<i32> {
    loop_.snapshot().process[index].pid.map(|p| p as i32)
}

/// Ensures the process at `index` has a live pid, ticking once if needed.
fn ensure_running(loop_: &mut SupervisorLoop<FakeClock>, index: usize) -> i32 {
    for _ in 0..100 {
        if let Some(pid) = get_pid(loop_, index) {
            return pid;
        }
        loop_.tick();
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("process never came up");
}

/// Ticks with short real sleeps until the process at `index` is fully reaped.
fn tick_until_reaped(loop_: &mut SupervisorLoop<FakeClock>, index: usize) {
    for _ in 0..500 {
        loop_.tick();
        if get_pid(loop_, index).is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for the process to be reaped");
}

/// Ticks until the process at `index` is alive again with a *different* pid.
fn tick_until_new_pid(
    loop_: &mut SupervisorLoop<FakeClock>,
    index: usize,
    old_pid: Option<i32>,
) -> i32 {
    for _ in 0..500 {
        loop_.tick();
        if let Some(pid) = get_pid(loop_, index) {
            if Some(pid) != old_pid {
                return pid;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for a new instance to come up");
}

/// Polls a log file's *content* with a real deadline until `pred` holds. The
/// only correct way to observe a live process's log (written from the reader
/// thread, asynchronously to the main loop). Returns the content that satisfied
/// `pred`.
fn wait_for_log<F: Fn(&[u8]) -> bool>(path: &Path, pred: F, timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    loop {
        let content = std::fs::read(path).unwrap_or_default();
        if pred(&content) {
            return content;
        }
        if Instant::now() >= deadline {
            panic!(
                "log {} did not satisfy the predicate within {timeout:?} (had {} bytes)",
                path.display(),
                content.len()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Counts existing `<path>.N` rotated files in `dir` for base name `base`.
fn rotated_count(dir: &Path, base: &str) -> usize {
    let prefix = format!("{base}.");
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            // `<base>.N` where N is all digits.
            name.strip_prefix(&prefix)
                .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|ch| ch.is_ascii_digit()))
        })
        .count()
}

// ---- tests ----

/// A chatty stdout stub that loops printing a marker (single `write`s each).
fn chatty_stdout(marker: &str) -> ProcessConfig {
    sh(
        &format!("while true; do echo {marker}; done"),
        RestartPolicy::Never,
    )
}

#[test]
fn stdout_is_captured_to_file() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("web.stdout.log");
    let err = dir.path().join("web.stderr.log");
    // Big limit: no rotation, just capture.
    let config = with_log(chatty_stdout("hello-marker"), Some(&out), None, 1 << 20, 3);
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    ensure_running(&mut loop_, 0);

    // Process is alive → poll the file content.
    let got = wait_for_log(
        &out,
        |c| c.windows(12).any(|w| w == b"hello-marker"),
        Duration::from_secs(5),
    );
    assert!(!got.is_empty());
    // stderr was not configured, so its file never appeared.
    assert!(
        !err.exists(),
        "stderr file appeared though it was not configured"
    );

    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, 0);
}

#[test]
fn stdout_and_stderr_go_to_separate_files() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("web.stdout.log");
    let err = dir.path().join("web.stderr.log");
    let stub = sh(
        "while true; do echo out-marker; echo err-marker 1>&2; done",
        RestartPolicy::Never,
    );
    let config = with_log(stub, Some(&out), Some(&err), 1 << 20, 3);
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    ensure_running(&mut loop_, 0);

    let out_content = wait_for_log(
        &out,
        |c| c.windows(10).any(|w| w == b"out-marker"),
        Duration::from_secs(5),
    );
    let err_content = wait_for_log(
        &err,
        |c| c.windows(10).any(|w| w == b"err-marker"),
        Duration::from_secs(5),
    );
    assert!(
        !out_content.windows(10).any(|w| w == b"err-marker"),
        "stderr leaked into the stdout file"
    );
    assert!(
        !err_content.windows(10).any(|w| w == b"out-marker"),
        "stdout leaked into the stderr file"
    );

    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, 0);
}

/// The key test of the stage: rotation happens while the process is alive.
#[test]
fn rotation_happens_while_process_is_alive() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("web.stdout.log");
    let dot1 = dir.path().join("web.stdout.log.1");
    let dot2 = dir.path().join("web.stdout.log.2");
    // Tiny limit, keep 2, chatty stub → rotation within seconds.
    let config = with_log(chatty_stdout("rotate-me"), Some(&out), None, 256, 2);
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    ensure_running(&mut loop_, 0);

    // Process alive → poll for the rotated files existing.
    wait_for_log(&dot1, |_| dot1.exists(), Duration::from_secs(10));
    wait_for_log(&dot2, |_| dot2.exists(), Duration::from_secs(10));

    // Retention holds: never more than keep rotated files.
    assert!(
        rotated_count(dir.path(), "web.stdout.log") <= 2,
        "more rotated files than keep"
    );
    // The current file reappears on the next write. It is not asserted to exist
    // *right now*: with a chatty no-sleep stub, a rotation may have just renamed
    // the current file to `.1` and the lazy reopen has not happened yet — a
    // legitimate transient, so poll for the current file rather than snapshotting
    // it (a plain `out.exists()` here is a race against the lazy reopen).
    wait_for_log(&out, |_| out.exists(), Duration::from_secs(5));

    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, 0);
}

/// The synchronous guarantee of join-at-reap: once the leader is reaped the file
/// holds the whole output. Assert without polling.
#[test]
fn output_is_complete_after_reap() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("web.stdout.log");
    // Prints a unique marker and exits; restart = never.
    let stub = sh("echo complete-marker", RestartPolicy::Never);
    let config = with_log(stub, Some(&out), None, 1 << 20, 3);
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));

    tick_until_reaped(&mut loop_, 0);
    // Synchronous: the readers were bounded-joined during the reap.
    let content = std::fs::read(&out).unwrap();
    assert!(
        content.windows(15).any(|w| w == b"complete-marker"),
        "output not complete after reap: {:?}",
        String::from_utf8_lossy(&content)
    );
}

/// A grandchild that inherits the leader's stdout writes to the same file: the
/// pipe is captured for the whole tree.
#[test]
fn grandchild_output_is_captured() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("web.stdout.log");
    // Parent forks a grandchild (inherits the write end via fork) and stays alive.
    let stub = sh(
        "( echo from-grandchild ) & while true; do echo parent; sleep 0.1; done",
        RestartPolicy::Never,
    );
    let config = with_log(stub, Some(&out), None, 1 << 20, 3);
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    ensure_running(&mut loop_, 0);

    wait_for_log(
        &out,
        |c| c.windows(15).any(|w| w == b"from-grandchild"),
        Duration::from_secs(5),
    );

    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, 0);
}

/// A restart (operator) keeps writing to the same file (append), and the new
/// reader resumes the size count from metadata.
#[test]
fn restarted_instance_appends_to_same_file() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("web.stdout.log");
    // Each instance prints a pid-tagged marker so the two are distinguishable,
    // then loops staying alive.
    let stub = sh(
        "echo start-$$; while true; do sleep 0.1; done",
        RestartPolicy::Always,
    );
    let config = with_log(stub, Some(&out), None, 1 << 20, 3);
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid1 = ensure_running(&mut loop_, 0);

    wait_for_log(
        &out,
        |c| c.windows(6).any(|w| w == b"start-"),
        Duration::from_secs(5),
    );

    loop_.handle_command(&Request::Restart("web".to_string()));
    tick_until_reaped(&mut loop_, 0);
    let pid2 = tick_until_new_pid(&mut loop_, 0, Some(pid1));
    assert_ne!(pid1, pid2);

    let marker1 = format!("start-{pid1}");
    let marker2 = format!("start-{pid2}");
    let content = wait_for_log(
        &out,
        |c| {
            let s = String::from_utf8_lossy(c);
            s.contains(&marker1) && s.contains(&marker2)
        },
        Duration::from_secs(5),
    );
    let s = String::from_utf8_lossy(&content);
    assert!(
        s.contains(&marker1),
        "first instance marker missing (not appended)"
    );
    assert!(s.contains(&marker2), "second instance marker missing");

    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, 0);
}

/// After an operator `stop` and reap: synchronous assert on content, then the
/// file does not grow (two reads with a real-time gap).
#[test]
fn stopped_process_file_is_complete_and_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("web.stdout.log");
    let stub = sh(
        "echo quiet-marker; while true; do sleep 0.1; done",
        RestartPolicy::Never,
    );
    let config = with_log(stub, Some(&out), None, 1 << 20, 3);
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    ensure_running(&mut loop_, 0);
    wait_for_log(
        &out,
        |c| c.windows(12).any(|w| w == b"quiet-marker"),
        Duration::from_secs(5),
    );

    loop_.handle_command(&Request::Stop("web".to_string()));
    tick_until_reaped(&mut loop_, 0);

    // Synchronous content assert.
    let content = std::fs::read(&out).unwrap();
    assert!(content.windows(12).any(|w| w == b"quiet-marker"));

    // File is quiet now (readers joined, no live writer).
    let size1 = std::fs::metadata(&out).unwrap().len();
    std::thread::sleep(Duration::from_millis(200));
    let size2 = std::fs::metadata(&out).unwrap().len();
    assert_eq!(
        size1, size2,
        "the file kept growing after the process stopped"
    );
}

/// §6.4: changing only `max-size-bytes` forces a restart through the existing
/// reload `changed` branch (derived `PartialEq`), with no reload-code changes.
#[test]
fn changed_log_config_forces_restart() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("web.stdout.log");
    let stub = sh(
        "echo cfg-marker; while true; do sleep 0.1; done",
        RestartPolicy::Always,
    );
    let config = with_log(stub.clone(), Some(&out), None, 1 << 20, 3);
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid1 = ensure_running(&mut loop_, 0);
    wait_for_log(
        &out,
        |c| c.windows(10).any(|w| w == b"cfg-marker"),
        Duration::from_secs(5),
    );

    // Reload with only max-size-bytes changed → forced restart.
    let changed = with_log(stub, Some(&out), None, 512, 3);
    loop_.apply_config(vec![changed]);
    // The process is being stopped (STOPPING) for the forced restart.
    assert_eq!(
        loop_.snapshot().process[0].state,
        supervisor_rs::state::ProcState::Stopping,
        "changed log config did not force a restart"
    );

    let pid2 = tick_until_new_pid(&mut loop_, 0, Some(pid1));
    assert_ne!(pid1, pid2);
    // Address by name (Этап 8 rule): find the entry named "web".
    let snap = loop_.snapshot();
    let web = snap
        .process
        .iter()
        .find(|p| p.name == "web")
        .expect("web present");
    assert_eq!(web.restart_count, 1, "restart_count did not advance");

    // The new instance's output appears in the file.
    wait_for_log(
        &out,
        |c| {
            // The marker appears at least once from the new instance too; simplest
            // observable: the file keeps getting the marker after restart.
            c.windows(10).filter(|w| *w == b"cfg-marker").count() >= 2
        },
        Duration::from_secs(5),
    );

    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, 0);
}

/// A tree deaf to SIGTERM is flushed after the SIGKILL escalation: whatever it
/// managed to print is in the file once reaped. Proves EOF converges through the
/// killpg-sweep, not through a polite exit.
#[test]
fn deaf_tree_is_flushed_after_sigkill_escalation() {
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("web.stdout.log");
    let pidfile = dir.path().join("pids");
    let mut base = deaf(&pidfile, 2);
    base = with_log(base, Some(&out), None, 1 << 20, 3);
    let configs = [base];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid = wait_for_pid(&pidfile, Duration::from_secs(5));
    loop_.tick();
    // Wait until the deaf stub has actually emitted at least one marker.
    wait_for_log(
        &out,
        |c| c.windows(11).any(|w| w == b"deaf-marker"),
        Duration::from_secs(5),
    );

    // Operator stop; deaf to TERM, so only the SIGKILL after grace stops it.
    loop_.handle_command(&Request::Stop("web".to_string()));
    loop_.clock().advance(Duration::from_secs(3));
    tick_until_reaped(&mut loop_, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while is_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !is_alive(pid),
        "the deaf stub survived the SIGKILL escalation"
    );

    // Synchronous: readers were bounded-joined at reap; all emitted output is in.
    let content = std::fs::read(&out).unwrap();
    assert!(
        content.windows(11).any(|w| w == b"deaf-marker"),
        "deaf tree output was not flushed to the file"
    );
}
