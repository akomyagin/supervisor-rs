//! In-process tests for the Этап 6 control-socket commands: `handle_command`
//! driven directly against `SupervisorLoop`, advanced with `tick()`. No sockets
//! involved — the socket plumbing itself is covered by `src/control.rs`'s own
//! unit tests and by the end-to-end tests in `tests/control.rs`.
//!
//! `cfg`/`sh` and the `DEAF_SCRIPT` stub are deliberately duplicated from
//! `tests/shutdown.rs`: every integration test file is its own crate, and the
//! existing files already made that trade — see their preambles.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use supervisor_rs::clock::FakeClock;
use supervisor_rs::config::{ProcessConfig, RestartPolicy, DEFAULT_STOP_GRACE_SECS};
use supervisor_rs::control::{Request, Response};
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

/// A stub that outlives the test unless it is signalled.
fn long_running(restart: RestartPolicy) -> ProcessConfig {
    cfg("web", &["/usr/bin/env", "sleep", "100"], restart)
}

/// A stub that ignores SIGTERM and reports its pid, so only the SIGKILL
/// escalation can stop it. As in `tests/shutdown.rs`'s `DEAF_SCRIPT`, the trap
/// is armed *before* the pid file is written, so "pid file has a parseable
/// line" is a true handshake for "the trap is live" — without it a SIGTERM
/// could reach a shell that has not yet run `trap`, kill it by the default
/// disposition, and leave the test green with the escalation path untested.
const DEAF_SCRIPT: &str = r#"trap '' TERM
echo $$ >> "$SUP_PIDFILE"
i=0
while [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done
"#;

fn deaf(restart: RestartPolicy, pidfile: &Path, stop_grace_secs: u64) -> ProcessConfig {
    let mut c = sh(DEAF_SCRIPT, restart);
    c.env = Some(BTreeMap::from([(
        "SUP_PIDFILE".to_string(),
        pidfile.display().to_string(),
    )]));
    c.stop_grace_secs = stop_grace_secs;
    c
}

/// Waits until the stub's pid file holds a complete, parseable pid, and
/// returns it. Polling for mere existence would not do: `echo $$ >> file` is an
/// `open` followed by a separate `write`, and between the two the file exists
/// and is empty.
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

/// A stub that always fails immediately — drives the BACKOFF class.
fn failing(restart: RestartPolicy) -> ProcessConfig {
    cfg("web", &["/usr/bin/env", "false"], restart)
}

fn is_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None) != Err(Errno::ESRCH)
}

fn get_pid(loop_: &SupervisorLoop<FakeClock>, index: usize) -> Option<i32> {
    loop_.snapshot().process[index].pid.map(|p| p as i32)
}

/// Ticks with short real sleeps until the process at `index` is fully reaped:
/// no live pid in the snapshot, and (unless `expect_done`) not because the
/// loop declared it `done`.
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

/// Ticks until the process at `index` is alive again with a *different* pid
/// than `old_pid`.
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

fn tick_until_done(loop_: &mut SupervisorLoop<FakeClock>, index: usize) {
    for _ in 0..500 {
        loop_.tick();
        if loop_.is_done(index) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for the process to be marked done");
}

fn assert_ok_contains(resp: &Response, needle: &str) {
    match resp {
        Response::Ok(Some(text)) => assert!(
            text.contains(needle),
            "{text:?} does not contain {needle:?}"
        ),
        other => panic!("expected Ok(Some(..)) containing {needle:?}, got {other:?}"),
    }
}

fn assert_error_contains(resp: &Response, needle: &str) {
    match resp {
        Response::Error(text) => assert!(
            text.contains(needle),
            "{text:?} does not contain {needle:?}"
        ),
        other => panic!("expected Error containing {needle:?}, got {other:?}"),
    }
}

/// Key test of the stage: `stop` kills the process and the `always` policy does
/// not resurrect it.
#[test]
fn stop_kills_process_and_policy_always_does_not_resurrect_it() {
    let configs = [long_running(RestartPolicy::Always)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    loop_.tick();
    let pid = get_pid(&loop_, 0).expect("stub should be running");

    let resp = loop_.handle_command(&Request::Stop("web".to_string()));
    assert_ok_contains(&resp, "stopping");

    tick_until_reaped(&mut loop_, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while is_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!is_alive(pid), "the stub survived the stop command");

    loop_.clock().advance(Duration::from_secs(60));
    for _ in 0..10 {
        loop_.tick();
    }
    assert!(
        !loop_.is_done(0),
        "the daemon must stay up after a user stop"
    );
    assert!(
        get_pid(&loop_, 0).is_none(),
        "policy always must not resurrect a user-stopped process"
    );
    let snapshot = loop_.snapshot();
    assert_eq!(
        snapshot.process[0].state,
        supervisor_rs::state::ProcState::Stopped
    );
}

#[test]
fn stop_escalates_to_sigkill_after_grace() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pids");
    let configs = [deaf(RestartPolicy::Never, &pidfile, 2)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid = wait_for_pid(&pidfile, Duration::from_secs(5));
    loop_.tick();

    let resp = loop_.handle_command(&Request::Stop("web".to_string()));
    assert_ok_contains(&resp, "stopping");

    // Real time passes but fake time does not: still alive short of the grace.
    for _ in 0..5 {
        loop_.tick();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(is_alive(pid), "SIGKILL fired before the grace deadline");

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
}

#[test]
fn start_revives_user_stopped_process() {
    let configs = [long_running(RestartPolicy::Always)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    loop_.tick();
    let first_pid = get_pid(&loop_, 0).expect("stub should be running");

    loop_.handle_command(&Request::Stop("web".to_string()));
    tick_until_reaped(&mut loop_, 0);

    let resp = loop_.handle_command(&Request::Start("web".to_string()));
    assert_ok_contains(&resp, "starting");

    let second_pid = tick_until_new_pid(&mut loop_, 0, Some(first_pid));
    assert_ne!(first_pid, second_pid);
    assert_eq!(loop_.restart_count(0), 1);
    let snapshot = loop_.snapshot();
    assert_eq!(
        snapshot.process[0].state,
        supervisor_rs::state::ProcState::Running
    );
}

#[test]
fn restart_respawns_regardless_of_policy_never() {
    let configs = [long_running(RestartPolicy::Never)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    loop_.tick();
    let first_pid = get_pid(&loop_, 0).expect("stub should be running");

    let resp = loop_.handle_command(&Request::Restart("web".to_string()));
    assert_ok_contains(&resp, "restarting");

    tick_until_reaped(&mut loop_, 0);
    let second_pid = tick_until_new_pid(&mut loop_, 0, Some(first_pid));
    assert_ne!(first_pid, second_pid);
    assert!(
        !loop_.is_done(0),
        "restart must respawn despite policy never"
    );

    // Teardown: the respawned instance is a real long-running child.
    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, 0);
}

#[test]
fn restart_during_backoff_is_immediate() {
    let configs = [failing(RestartPolicy::OnFailure)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    tick_until_restart_scheduled(&mut loop_, 0);
    assert!(loop_.next_restart_delay(0).unwrap() > Duration::ZERO);

    let resp = loop_.handle_command(&Request::Restart("web".to_string()));
    assert_ok_contains(&resp, "restarting");
    assert_eq!(loop_.next_restart_delay(0), Some(Duration::ZERO));

    // No clock advance needed: the delay was cut to now().
    loop_.tick();
    assert!(
        get_pid(&loop_, 0).is_some(),
        "restart during backoff did not respawn immediately"
    );
}

#[test]
fn stop_cancels_scheduled_restart() {
    let configs = [failing(RestartPolicy::OnFailure)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    tick_until_restart_scheduled(&mut loop_, 0);

    let resp = loop_.handle_command(&Request::Stop("web".to_string()));
    assert_ok_contains(&resp, "stopped");
    assert_eq!(loop_.next_restart_delay(0), None);

    loop_.clock().advance(Duration::from_secs(60));
    for _ in 0..10 {
        loop_.tick();
    }
    assert!(!loop_.is_done(0));
    assert!(
        get_pid(&loop_, 0).is_none(),
        "stop during backoff must cancel the scheduled restart"
    );
}

#[test]
fn stop_is_idempotent() {
    let configs = [long_running(RestartPolicy::Never)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    loop_.tick();
    loop_.handle_command(&Request::Stop("web".to_string()));
    tick_until_reaped(&mut loop_, 0);

    let resp = loop_.handle_command(&Request::Stop("web".to_string()));
    assert_ok_contains(&resp, "already stopped");
}

#[test]
fn start_on_running_is_idempotent_ok() {
    let configs = [long_running(RestartPolicy::Never)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    loop_.tick();

    let resp = loop_.handle_command(&Request::Start("web".to_string()));
    assert_ok_contains(&resp, "already running");
}

#[test]
fn restart_on_user_stopped_is_an_error() {
    let configs = [long_running(RestartPolicy::Never)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    loop_.tick();
    loop_.handle_command(&Request::Stop("web".to_string()));
    tick_until_reaped(&mut loop_, 0);

    let resp = loop_.handle_command(&Request::Restart("web".to_string()));
    assert_error_contains(&resp, "use 'start");
}

#[test]
fn commands_name_not_found() {
    let configs = [long_running(RestartPolicy::Never)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    loop_.tick();

    let resp = loop_.handle_command(&Request::Stop("ghost".to_string()));
    assert_error_contains(&resp, "no such process");
}

#[test]
fn commands_rejected_during_shutdown() {
    let configs = [long_running(RestartPolicy::Never)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    loop_.tick();
    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);

    for req in [
        Request::Stop("web".to_string()),
        Request::Start("web".to_string()),
        Request::Restart("web".to_string()),
    ] {
        let resp = loop_.handle_command(&req);
        assert_error_contains(&resp, "shutting down");
    }
}

#[test]
fn daemon_survives_stop_of_its_only_process() {
    let configs = [long_running(RestartPolicy::Always)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    loop_.tick();

    loop_.handle_command(&Request::Stop("web".to_string()));
    tick_until_reaped(&mut loop_, 0);
    for _ in 0..5 {
        loop_.tick();
    }
    assert!(
        !loop_.is_done(0),
        "the daemon must not exit when its only process is user-stopped"
    );
}

#[test]
fn shutdown_finishes_cleanly_with_user_stopped_process() {
    let configs = [long_running(RestartPolicy::Always)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    loop_.tick();

    loop_.handle_command(&Request::Stop("web".to_string()));
    tick_until_reaped(&mut loop_, 0);

    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    assert!(
        loop_.is_done(0),
        "shutdown must finish cleanly with a user-stopped process"
    );
}

/// A second `restart` before the first has been reaped must not queue a
/// double respawn — it reports the one already in flight.
#[test]
fn restart_twice_reports_already_in_progress() {
    let configs = [long_running(RestartPolicy::Never)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    loop_.tick();
    let first_pid = get_pid(&loop_, 0).expect("stub should be running");

    let resp = loop_.handle_command(&Request::Restart("web".to_string()));
    assert_ok_contains(&resp, "restarting");

    // Still STOPPING (running, TERM sent, not yet reaped): a second restart
    // must not re-arm a second respawn on top of the pending one.
    let resp = loop_.handle_command(&Request::Restart("web".to_string()));
    assert_ok_contains(&resp, "already in progress");

    tick_until_reaped(&mut loop_, 0);
    let second_pid = tick_until_new_pid(&mut loop_, 0, Some(first_pid));
    assert_ne!(first_pid, second_pid);

    // Teardown: the respawned instance is a real long-running child.
    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, 0);
}

/// A process that finished terminally (policy `never`, no operator intent) is
/// never revived: `start`/`restart` refuse it outright, and `stop` is a no-op
/// ack rather than an error — the daemon has nothing left to signal.
#[test]
fn commands_on_done_process_are_refused() {
    let configs = [failing(RestartPolicy::Never)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    tick_until_done(&mut loop_, 0);

    let resp = loop_.handle_command(&Request::Stop("web".to_string()));
    assert_ok_contains(&resp, "already stopped");

    let resp = loop_.handle_command(&Request::Start("web".to_string()));
    assert_error_contains(&resp, "cannot be started");

    let resp = loop_.handle_command(&Request::Restart("web".to_string()));
    assert_error_contains(&resp, "cannot be restarted");

    assert!(
        loop_.is_done(0),
        "commands on a finished process must not revive it"
    );
}

#[test]
fn stop_then_restart_override() {
    let configs = [long_running(RestartPolicy::Never)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    loop_.tick();
    let pid = get_pid(&loop_, 0).expect("stub should be running");

    let resp = loop_.handle_command(&Request::Restart("web".to_string()));
    assert_ok_contains(&resp, "restarting");
    let resp = loop_.handle_command(&Request::Stop("web".to_string()));
    assert_ok_contains(&resp, "stopping");

    tick_until_reaped(&mut loop_, 0);
    for _ in 0..10 {
        loop_.tick();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        get_pid(&loop_, 0).is_none(),
        "stop after restart must override the pending respawn"
    );
    let _ = pid;
}
