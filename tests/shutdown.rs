//! Integration tests for the shutdown mode, driven in-process: the Этап 3
//! semantics (forward, suppress restarts) and the Этап 4 escalation state
//! machine — grace deadline, immediate escalation, and the group sweep that
//! follows a leader's exit.
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

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{kill, Signal};
use nix::unistd::{getpgid, Pid};
use supervisor_rs::clock::FakeClock;
use supervisor_rs::config::{ProcessConfig, RestartPolicy, DEFAULT_STOP_GRACE_SECS};
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
    }
}

fn sh(script: &str, restart: RestartPolicy) -> ProcessConfig {
    cfg("stub", &["/usr/bin/env", "sh", "-c", script], restart)
}

/// A stub that outlives the test unless it is signalled. `env` execs `sleep`
/// directly, so the supervised pid is the sleeping process itself and the
/// default SIGTERM disposition ends it — no shell in between to complicate the
/// process tree (the tree cases are the Этап 4 tests below).
fn long_running(restart: RestartPolicy) -> ProcessConfig {
    cfg("stub", &["/usr/bin/env", "sleep", "100"], restart)
}

/// A stub that ignores SIGTERM and reports its pid, so only the SIGKILL
/// escalation can stop it. The trap is armed *before* the pid is written:
/// without that handshake `begin_shutdown` could reach a shell that is still
/// deaf-less, the default disposition would kill it, and the test would stay
/// green with the escalation completely broken.
const DEAF_SCRIPT: &str = r#"trap '' TERM
echo $$ >> "$SUP_PIDFILE"
i=0
while [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done
"#;

/// A stub that forks one long-running child and then fails, leaving the
/// grandchild behind for the sweep to collect.
const ORPHANING_SCRIPT: &str = r#"( i=0; while [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done ) &
echo $! >> "$SUP_PIDFILE"
exit 1
"#;

/// A stub that fails on its first run and stays alive on the second, telling
/// the two apart by the number of lines already in the pid file.
///
/// The asymmetry is what makes the restarted instance observable: a stub that
/// exited immediately on the respawn too would be reaped by the very next
/// `tick()`, and `getpgid` on the restarted pid would race that reaping.
const RESTART_SCRIPT: &str = r#"echo $$ >> "$SUP_PIDFILE"
if [ "$(wc -l < "$SUP_PIDFILE")" -ge 2 ]; then
  i=0
  while [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done
fi
exit 1
"#;

fn sh_with_pidfile(
    script: &str,
    restart: RestartPolicy,
    pidfile: &Path,
    stop_grace_secs: u64,
) -> ProcessConfig {
    let mut config = sh(script, restart);
    config.env = Some(BTreeMap::from([(
        "SUP_PIDFILE".to_string(),
        pidfile.display().to_string(),
    )]));
    config.stop_grace_secs = stop_grace_secs;
    config
}

/// Waits until line `n` (1-based) of the stub's pid file holds a complete,
/// parseable pid, and returns it.
///
/// Polling for mere existence would not do: `echo $$ >> file` is an `open`
/// followed by a separate `write`, so between the two the file exists and is
/// empty. Waiting for the content closes that window — and for line `n` the
/// same argument applies to the line itself, which is why the *n*-th line must
/// parse, not merely exist.
fn wait_for_pid_line(path: &Path, n: usize, timeout: Duration) -> i32 {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Some(pid) = text.lines().nth(n - 1).and_then(|l| l.trim().parse().ok()) {
                return pid;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("stub did not report pid line {n} within {timeout:?}");
}

/// The common case of [`wait_for_pid_line`]: the stub's first (and usually
/// only) report.
fn wait_for_pid(path: &Path, timeout: Duration) -> i32 {
    wait_for_pid_line(path, 1, timeout)
}

fn is_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None) != Err(Errno::ESRCH)
}

/// Polls until `pid` is gone from the process table.
///
/// A killed *grandchild* first becomes a zombie — where `kill(pid, 0)` still
/// succeeds — and only disappears once init reaps it, asynchronously. A single
/// assert would therefore be a flake; only a deadline-bounded poll is honest.
fn wait_until_gone(pid: i32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("process {pid} was still alive after {timeout:?}");
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

/// Both sides of the grace deadline, on the fake clock: no SIGKILL before it,
/// SIGKILL after it. This is what a blocking `terminate_tree(pgid, grace)`
/// could not have been tested for — its wait would have burned real seconds
/// inside a single call, invisible from here.
#[test]
fn escalation_waits_for_grace_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pids");
    // Deliberately not DEFAULT_STOP_GRACE_SECS: with the default here, the test
    // would stay green even if begin_shutdown ignored the config field and hard
    // -coded the default, i.e. the per-process grace could be entirely unwired
    // and nothing would notice. 7 s is checked from both sides below.
    let configs = [sh_with_pidfile(
        DEAF_SCRIPT,
        RestartPolicy::Never,
        &pidfile,
        7,
    )];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));

    let pid = wait_for_pid(&pidfile, Duration::from_secs(5));
    loop_.tick();
    loop_.begin_shutdown(Signal::SIGTERM);

    // Real time passes here, fake time does not — so the deadline stays in the
    // future however slow the runner is.
    for _ in 0..10 {
        loop_.tick();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !loop_.is_done(0),
        "the deaf stub cannot have exited on SIGTERM"
    );
    assert!(is_alive(pid), "SIGKILL was sent before the grace deadline");

    // Past the default, short of the configured 7 s: a supervisor using the
    // default would kill the stub here, and the next assertion would catch it.
    loop_.clock().advance(Duration::from_secs(6));
    loop_.tick();
    assert!(
        is_alive(pid),
        "SIGKILL came at the default grace, not the configured one"
    );

    loop_.clock().advance(Duration::from_secs(2));
    tick_until_done(&mut loop_, 0);
    // The stub is a direct child and was reaped by the loop, so it is not even
    // a zombie: ESRCH is immediate here, no polling needed.
    assert!(!is_alive(pid), "the stub survived the SIGKILL escalation");
}

/// The in-process half of `tests/tree.rs::second_signal_escalates_immediately`:
/// the grace is 30 s and the clock never advances, so the stub can only die
/// from the explicit escalation.
#[test]
fn escalate_to_kill_skips_grace() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pids");
    let configs = [sh_with_pidfile(
        DEAF_SCRIPT,
        RestartPolicy::Never,
        &pidfile,
        30,
    )];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));

    let pid = wait_for_pid(&pidfile, Duration::from_secs(5));
    loop_.tick();
    loop_.begin_shutdown(Signal::SIGTERM);
    loop_.escalate_to_kill();

    tick_until_done(&mut loop_, 0);
    assert!(!is_alive(pid), "the stub survived the immediate escalation");
}

/// Teardown on the restart path: a child that dies leaving its own children
/// behind must not have them still running when the next instance comes up.
///
/// Scope, precisely: this proves the sweep *happens*, not that it happens
/// before the restart is scheduled. Without it the grandchild would spin for
/// 60 s and `wait_until_gone` would time out; but a sweep deferred to a later
/// tick would pass just as well, because the grandchild's disappearance is
/// asynchronous (init reaps it) and has to be polled for. Pinning the ordering
/// would need an observable the kernel does not offer here.
#[test]
fn leader_exit_sweeps_leftover_grandchildren() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pids");
    let configs = [sh_with_pidfile(
        ORPHANING_SCRIPT,
        RestartPolicy::OnFailure,
        &pidfile,
        DEFAULT_STOP_GRACE_SECS,
    )];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));

    // The pid is written before the leader's `exit 1`, and the file outlives
    // it, so reading it cannot race with the leader's death.
    let grandchild = wait_for_pid(&pidfile, Duration::from_secs(5));
    tick_until_restart_scheduled(&mut loop_, 0);

    // Nobody reaps the grandchild but init, so its disappearance is
    // asynchronous — poll rather than assert once.
    wait_until_gone(grandchild, Duration::from_secs(5));
}

/// Every process gets its *own* grace deadline, and the deadlines run
/// concurrently.
///
/// This is the argument against the rejected blocking `terminate_tree(pgid,
/// grace)` — it would have serialised the waits into 2 + 7 = 9 s — and until
/// now no test exercised it: every other shutdown test supervises a single
/// process, so a regression collapsing the per-process deadlines into one
/// shared deadline would have stayed green.
///
/// Both stubs are deaf to SIGTERM, so the only thing that can kill either of
/// them is its own expired deadline; the fake clock is advanced to 3 s, which
/// is past the first grace (2 s) and short of the second (7 s).
#[test]
fn shutdown_deadlines_are_per_process_not_shared() {
    let dir = tempfile::tempdir().unwrap();
    let quick_pidfile = dir.path().join("quick-pids");
    let slow_pidfile = dir.path().join("slow-pids");
    let configs = [
        sh_with_pidfile(DEAF_SCRIPT, RestartPolicy::Never, &quick_pidfile, 2),
        sh_with_pidfile(DEAF_SCRIPT, RestartPolicy::Never, &slow_pidfile, 7),
    ];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));

    let quick_pid = wait_for_pid(&quick_pidfile, Duration::from_secs(5));
    let slow_pid = wait_for_pid(&slow_pidfile, Duration::from_secs(5));
    loop_.tick();
    loop_.begin_shutdown(Signal::SIGTERM);

    // 3 s of logical time: past the first grace, well short of the second.
    loop_.clock().advance(Duration::from_secs(3));
    tick_until_done(&mut loop_, 0);
    // A direct child reaped by the loop leaves no zombie, so ESRCH is immediate.
    assert!(
        !is_alive(quick_pid),
        "the 2 s grace did not expire on its own schedule"
    );
    assert!(
        !loop_.is_done(1),
        "the second process finished on the first one's deadline"
    );
    assert!(
        is_alive(slow_pid),
        "the 7 s grace was cut short by the other process's deadline"
    );

    // Total 8 s: now the second deadline is past too.
    loop_.clock().advance(Duration::from_secs(5));
    tick_until_done(&mut loop_, 1);
    assert!(
        !is_alive(slow_pid),
        "the second process survived its own deadline"
    );
}

/// A restarted instance leads a *fresh* group of its own.
///
/// `leader_exit_sweeps_leftover_grandchildren` stops at "a restart is
/// scheduled"; nothing checked that the new instance actually comes up and gets
/// its own pgid. That matters because the whole teardown rests on
/// `pgid == leader pid`: a respawn that forgot `setsid` — or one whose pgid was
/// carried over from the reaped predecessor — would make the next `killpg` miss
/// its tree, or hit somebody else's.
#[test]
fn restarted_instance_leads_its_own_fresh_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pids");
    let configs = [sh_with_pidfile(
        RESTART_SCRIPT,
        RestartPolicy::OnFailure,
        &pidfile,
        DEFAULT_STOP_GRACE_SECS,
    )];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));

    let first_pid = wait_for_pid_line(&pidfile, 1, Duration::from_secs(5));
    tick_until_restart_scheduled(&mut loop_, 0);

    // Skip the backoff on the fake clock, then let one tick do the respawn.
    let delay = loop_.next_restart_delay(0).unwrap();
    loop_.clock().advance(delay);
    loop_.tick();
    assert_eq!(loop_.restart_count(0), 1, "the process was not restarted");

    let second_pid = wait_for_pid_line(&pidfile, 2, Duration::from_secs(5));
    assert_ne!(
        first_pid, second_pid,
        "the restarted instance reported the old pid"
    );

    // setsid runs in the child after fork, so the group membership is not
    // guaranteed to be visible the instant the pid shows up — poll with a
    // deadline instead of asserting once.
    let target = Pid::from_raw(second_pid);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut pgid = getpgid(Some(target)).unwrap();
    while pgid != target && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        pgid = getpgid(Some(target)).unwrap();
    }
    assert_eq!(
        pgid, target,
        "the restarted instance did not lead its own group"
    );

    loop_.begin_shutdown(Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_done(&mut loop_, 0);
}
