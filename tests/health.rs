//! In-process tests for the Этап 7 health-check scheduling and the "stuck →
//! restart" trigger: `run_due_health_check()` driven directly against
//! `SupervisorLoop`, advanced with `tick()` and a `FakeClock`. The probe
//! *execution* is real (fast `sh -c` exec probes); the *schedule* is logical,
//! controlled by advancing the fake clock.
//!
//! `cfg`/`sh`/`DEAF_SCRIPT`/`wait_for_pid`/`tick_until_reaped`/`tick_until_new_pid`
//! are deliberately duplicated from `tests/commands.rs`: every integration test
//! file is its own crate, and the existing files already made that trade — see
//! their preambles. `cfg` gains a health-check setter here.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use supervisor_rs::clock::FakeClock;
use supervisor_rs::config::DEFAULT_STOP_GRACE_SECS;
use supervisor_rs::config::{HealthCheckConfig, ProbeKind, ProcessConfig, RestartPolicy};
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
    }
}

fn sh(script: &str, restart: RestartPolicy) -> ProcessConfig {
    cfg("web", &["/usr/bin/env", "sh", "-c", script], restart)
}

/// A stub that outlives the test unless it is signalled.
fn long_running(restart: RestartPolicy) -> ProcessConfig {
    cfg("web", &["/usr/bin/env", "sleep", "100"], restart)
}

/// A stub deaf to SIGTERM that reports its pid — only the SIGKILL escalation
/// stops it. The trap is armed *before* the pid file is written, so a parseable
/// pid line is a true handshake for "the trap is live".
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

/// Attaches an exec health check to a process config. `interval`, `timeout`,
/// `threshold`, `start_period` in seconds; `command` is the probe argv.
fn with_exec_health(
    mut c: ProcessConfig,
    command: &[&str],
    interval_secs: u64,
    timeout_secs: u64,
    failure_threshold: u32,
    start_period_secs: u64,
) -> ProcessConfig {
    let toml_src = {
        let cmd = command
            .iter()
            .map(|s| format!("{s:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            r#"
type = "exec"
command = [{cmd}]
interval-secs = {interval_secs}
timeout-secs = {timeout_secs}
failure-threshold = {failure_threshold}
start-period-secs = {start_period_secs}
"#
        )
    };
    let hc: HealthCheckConfig = toml::from_str(&toml_src).expect("valid health-check toml");
    assert_eq!(hc.kind, ProbeKind::Exec);
    c.health_check = Some(hc);
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

fn state_of(loop_: &SupervisorLoop<FakeClock>, index: usize) -> supervisor_rs::state::ProcState {
    loop_.snapshot().process[index].state
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

/// Number of probe executions recorded in the counter file (one line per run).
fn probe_count(path: &Path) -> usize {
    match std::fs::read_to_string(path) {
        Ok(text) => text.lines().filter(|l| !l.trim().is_empty()).count(),
        Err(_) => 0,
    }
}

/// Ensures the process at `index` has a live pid, ticking once if needed. Used
/// right after construction so the very first exec is observable.
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

// ---- tests ----

/// A probe that always succeeds (exit 0) and records each run.
fn counting_ok_probe(counter: &Path) -> String {
    format!(r#"echo x >> "{}"; exit 0"#, counter.display())
}

/// A probe that always fails (exit 1) and records each run.
fn counting_fail_probe(counter: &Path) -> String {
    format!(r#"echo x >> "{}"; exit 1"#, counter.display())
}

/// A probe whose success is toggled by the presence of a flag file, recording
/// each run.
fn flag_probe(counter: &Path, flag: &Path) -> String {
    format!(
        r#"echo x >> "{}"; test -e "{}""#,
        counter.display(),
        flag.display()
    )
}

#[test]
fn no_probe_before_first_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("probes");
    // start-period 60 + interval 10 → first probe due at t=70.
    let base = long_running(RestartPolicy::Always);
    let config = with_exec_health(
        base,
        &["/usr/bin/env", "sh", "-c", &counting_ok_probe(&counter)],
        10,
        5,
        3,
        60,
    );
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    ensure_running(&mut loop_, 0);

    loop_.clock().advance(Duration::from_secs(69));
    for _ in 0..3 {
        loop_.run_due_health_check();
    }
    assert_eq!(
        probe_count(&counter),
        0,
        "probe fired before its first deadline"
    );

    loop_.clock().advance(Duration::from_secs(2)); // now t=71
    loop_.run_due_health_check();
    assert_eq!(
        probe_count(&counter),
        1,
        "exactly one probe after the deadline"
    );
}

#[test]
fn healthy_process_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("probes");
    let base = long_running(RestartPolicy::Always);
    let config = with_exec_health(
        base,
        &["/usr/bin/env", "sh", "-c", &counting_ok_probe(&counter)],
        10,
        5,
        3,
        0,
    );
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid = ensure_running(&mut loop_, 0);

    for _ in 0..10 {
        loop_.clock().advance(Duration::from_secs(10));
        loop_.run_due_health_check();
    }
    assert_eq!(get_pid(&loop_, 0), Some(pid), "healthy process pid changed");
    assert_eq!(loop_.restart_count(0), 0);
    assert_eq!(
        state_of(&loop_, 0),
        supervisor_rs::state::ProcState::Running
    );
    assert!(
        probe_count(&counter) >= 5,
        "probes did not run for a healthy process"
    );
}

#[test]
fn probe_respects_interval_between_runs() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("probes");
    let base = long_running(RestartPolicy::Always);
    let config = with_exec_health(
        base,
        &["/usr/bin/env", "sh", "-c", &counting_ok_probe(&counter)],
        10,
        5,
        3,
        0,
    );
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    ensure_running(&mut loop_, 0);

    loop_.clock().advance(Duration::from_secs(10)); // first probe due
    loop_.run_due_health_check();
    assert_eq!(probe_count(&counter), 1);

    loop_.clock().advance(Duration::from_secs(9)); // interval-1
    loop_.run_due_health_check();
    assert_eq!(
        probe_count(&counter),
        1,
        "second probe ran before the interval"
    );

    loop_.clock().advance(Duration::from_secs(2)); // past the interval
    loop_.run_due_health_check();
    assert_eq!(probe_count(&counter), 2);
}

#[test]
fn failures_below_threshold_do_not_restart() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("probes");
    let base = long_running(RestartPolicy::Always);
    // threshold 3, drive two failures.
    let config = with_exec_health(
        base,
        &["/usr/bin/env", "sh", "-c", &counting_fail_probe(&counter)],
        10,
        5,
        3,
        0,
    );
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid = ensure_running(&mut loop_, 0);

    for _ in 0..2 {
        loop_.clock().advance(Duration::from_secs(10));
        loop_.run_due_health_check();
    }
    assert_eq!(probe_count(&counter), 2);
    assert_eq!(get_pid(&loop_, 0), Some(pid), "restarted below threshold");
    assert_eq!(
        state_of(&loop_, 0),
        supervisor_rs::state::ProcState::Running
    );
}

/// Key test of the stage: the third consecutive failure forces a restart.
#[test]
fn reaching_threshold_forces_restart() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("probes");
    let base = long_running(RestartPolicy::Always);
    let config = with_exec_health(
        base,
        &["/usr/bin/env", "sh", "-c", &counting_fail_probe(&counter)],
        10,
        5,
        3,
        0,
    );
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid = ensure_running(&mut loop_, 0);

    // Two failures below the threshold: still running.
    for _ in 0..2 {
        loop_.clock().advance(Duration::from_secs(10));
        loop_.run_due_health_check();
    }
    assert_eq!(
        state_of(&loop_, 0),
        supervisor_rs::state::ProcState::Running
    );

    // Third failure → threshold → forced restart: StopPhase is armed.
    loop_.clock().advance(Duration::from_secs(10));
    loop_.run_due_health_check();
    assert_eq!(
        state_of(&loop_, 0),
        supervisor_rs::state::ProcState::Stopping,
        "threshold did not arm the stop"
    );

    tick_until_reaped(&mut loop_, 0);
    let new_pid = tick_until_new_pid(&mut loop_, 0, Some(pid));
    assert_ne!(new_pid, pid);
    assert_eq!(loop_.restart_count(0), 1);
    assert!(!loop_.is_done(0));
}

/// Proves "consecutive", not "total": a success between failures resets the
/// counter, so it takes three *consecutive* failures to restart.
#[test]
fn success_resets_consecutive_failures() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("probes");
    let flag = dir.path().join("healthy");
    let base = long_running(RestartPolicy::Always);
    let config = with_exec_health(
        base,
        &["/usr/bin/env", "sh", "-c", &flag_probe(&counter, &flag)],
        10,
        5,
        3,
        0,
    );
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid = ensure_running(&mut loop_, 0);

    // Two failures (no flag).
    for _ in 0..2 {
        loop_.clock().advance(Duration::from_secs(10));
        loop_.run_due_health_check();
    }
    assert_eq!(
        state_of(&loop_, 0),
        supervisor_rs::state::ProcState::Running
    );

    // One success (flag present) resets the counter.
    std::fs::write(&flag, b"").unwrap();
    loop_.clock().advance(Duration::from_secs(10));
    loop_.run_due_health_check();
    std::fs::remove_file(&flag).unwrap();

    // Two more failures: still below threshold because the counter reset.
    for _ in 0..2 {
        loop_.clock().advance(Duration::from_secs(10));
        loop_.run_due_health_check();
    }
    assert_eq!(
        state_of(&loop_, 0),
        supervisor_rs::state::ProcState::Running,
        "counter did not reset on success"
    );
    assert_eq!(get_pid(&loop_, 0), Some(pid));

    // Third consecutive failure → restart.
    loop_.clock().advance(Duration::from_secs(10));
    loop_.run_due_health_check();
    assert_eq!(
        state_of(&loop_, 0),
        supervisor_rs::state::ProcState::Stopping,
        "third consecutive failure did not force a restart"
    );
}

/// After a health-triggered restart, the new instance gets a fresh start-period
/// and a zeroed failure count (generation keyed on `restart_count`).
#[test]
fn restarted_instance_gets_fresh_schedule() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("probes");
    let base = long_running(RestartPolicy::Always);
    // start-period 30, interval 10, threshold 1 → one failure restarts.
    let config = with_exec_health(
        base,
        &["/usr/bin/env", "sh", "-c", &counting_fail_probe(&counter)],
        10,
        5,
        1,
        30,
    );
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid = ensure_running(&mut loop_, 0);

    // First probe due at t = 30 + 10 = 40.
    loop_.clock().advance(Duration::from_secs(40));
    loop_.run_due_health_check();
    assert_eq!(probe_count(&counter), 1);
    assert_eq!(
        state_of(&loop_, 0),
        supervisor_rs::state::ProcState::Stopping
    );

    tick_until_reaped(&mut loop_, 0);
    let new_pid = tick_until_new_pid(&mut loop_, 0, Some(pid));
    assert_ne!(new_pid, pid);

    // New instance: advance less than start-period+interval of the new instance
    // → no new probe.
    let before = probe_count(&counter);
    loop_.clock().advance(Duration::from_secs(39));
    loop_.run_due_health_check();
    assert_eq!(
        probe_count(&counter),
        before,
        "new instance probed before its fresh start-period"
    );

    // Past the fresh deadline → a probe runs, and it takes a full threshold
    // (here 1) again — the counter began at zero.
    loop_.clock().advance(Duration::from_secs(2));
    loop_.run_due_health_check();
    assert_eq!(probe_count(&counter), before + 1);
}

/// The whole `StopPhase` machine is reused: a process deaf to SIGTERM is killed
/// via the grace → SIGKILL escalation, then respawned.
#[test]
fn stuck_process_is_killed_via_grace_escalation() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pids");
    let counter = dir.path().join("probes");
    // Deaf stub, grace 2s, threshold 1.
    let base = deaf(RestartPolicy::Always, &pidfile, 2);
    let config = with_exec_health(
        base,
        &["/usr/bin/env", "sh", "-c", &counting_fail_probe(&counter)],
        10,
        5,
        1,
        0,
    );
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid = wait_for_pid(&pidfile, Duration::from_secs(5));
    loop_.tick();

    // One failure → threshold → forced restart (TERM sent).
    loop_.clock().advance(Duration::from_secs(10));
    loop_.run_due_health_check();
    assert_eq!(
        state_of(&loop_, 0),
        supervisor_rs::state::ProcState::Stopping
    );

    // Deaf to TERM: only the SIGKILL after grace stops it. Advance past grace.
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

    // And it respawns.
    let new_pid = tick_until_new_pid(&mut loop_, 0, Some(pid));
    assert_ne!(new_pid, pid);

    // Teardown the respawned deaf instance.
    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, 0);
}

/// At most one probe per `run_due_health_check` call: two processes both due,
/// one call runs exactly one; the next call runs the other.
#[test]
fn at_most_one_probe_per_call() {
    let dir = tempfile::tempdir().unwrap();
    let counter_a = dir.path().join("probes_a");
    let counter_b = dir.path().join("probes_b");
    let a = with_exec_health(
        cfg(
            "a",
            &["/usr/bin/env", "sleep", "100"],
            RestartPolicy::Always,
        ),
        &["/usr/bin/env", "sh", "-c", &counting_ok_probe(&counter_a)],
        10,
        5,
        3,
        0,
    );
    let b = with_exec_health(
        cfg(
            "b",
            &["/usr/bin/env", "sleep", "100"],
            RestartPolicy::Always,
        ),
        &["/usr/bin/env", "sh", "-c", &counting_ok_probe(&counter_b)],
        10,
        5,
        3,
        0,
    );
    let configs = [a, b];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    ensure_running(&mut loop_, 0);
    ensure_running(&mut loop_, 1);

    loop_.clock().advance(Duration::from_secs(10)); // both due
    loop_.run_due_health_check();
    assert_eq!(
        probe_count(&counter_a) + probe_count(&counter_b),
        1,
        "more than one probe ran in a single call"
    );

    loop_.run_due_health_check();
    assert_eq!(
        probe_count(&counter_a) + probe_count(&counter_b),
        2,
        "the second due probe did not run on the next call"
    );
}

#[test]
fn user_stopped_process_is_not_probed() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("probes");
    let base = long_running(RestartPolicy::Always);
    let config = with_exec_health(
        base,
        &["/usr/bin/env", "sh", "-c", &counting_ok_probe(&counter)],
        10,
        5,
        3,
        0,
    );
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    ensure_running(&mut loop_, 0);

    loop_.handle_command(&Request::Stop("web".to_string()));
    tick_until_reaped(&mut loop_, 0);

    let before = probe_count(&counter);
    loop_.clock().advance(Duration::from_secs(100));
    for _ in 0..5 {
        loop_.run_due_health_check();
    }
    assert_eq!(
        probe_count(&counter),
        before,
        "a user-stopped process was probed"
    );

    // Start revives it and probing resumes on a fresh schedule.
    loop_.handle_command(&Request::Start("web".to_string()));
    tick_until_new_pid(&mut loop_, 0, None);
    loop_.clock().advance(Duration::from_secs(10));
    loop_.run_due_health_check();
    assert!(
        probe_count(&counter) > before,
        "probing did not resume after start"
    );
}

#[test]
fn no_probes_while_stop_is_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pids");
    let counter = dir.path().join("probes");
    // Deaf stub so the stop stays in flight (child still alive, stop != Idle).
    let base = deaf(RestartPolicy::Always, &pidfile, 10);
    let config = with_exec_health(
        base,
        &["/usr/bin/env", "sh", "-c", &counting_ok_probe(&counter)],
        10,
        5,
        3,
        0,
    );
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid = wait_for_pid(&pidfile, Duration::from_secs(5));
    loop_.tick();

    loop_.handle_command(&Request::Stop("web".to_string()));
    // Still alive, stop in flight.
    loop_.tick();
    let before = probe_count(&counter);
    loop_.clock().advance(Duration::from_secs(100));
    loop_.run_due_health_check();
    assert_eq!(
        probe_count(&counter),
        before,
        "a probe ran while a stop was in flight"
    );

    // Teardown.
    loop_.clock().advance(Duration::from_secs(20));
    tick_until_reaped(&mut loop_, 0);
    let _ = pid;
}

#[test]
fn no_probes_during_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("probes");
    let base = long_running(RestartPolicy::Always);
    let config = with_exec_health(
        base,
        &["/usr/bin/env", "sh", "-c", &counting_ok_probe(&counter)],
        10,
        5,
        3,
        0,
    );
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    ensure_running(&mut loop_, 0);

    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    let before = probe_count(&counter);
    loop_.clock().advance(Duration::from_secs(100));
    loop_.run_due_health_check();
    assert_eq!(probe_count(&counter), before, "a probe ran during shutdown");

    // Shutdown still finishes.
    tick_until_reaped(&mut loop_, 0);
    assert!(loop_.is_done(0));
}

/// An operator `stop` during a health-triggered restart-in-flight overrides the
/// intent: after reaping there is no respawn.
#[test]
fn operator_stop_overrides_health_restart_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pids");
    let counter = dir.path().join("probes");
    // Deaf stub, grace wide, threshold 1 → one failure arms RestartPending and
    // leaves the process STOPPING (still alive because deaf).
    let base = deaf(RestartPolicy::Always, &pidfile, 10);
    let config = with_exec_health(
        base,
        &["/usr/bin/env", "sh", "-c", &counting_fail_probe(&counter)],
        10,
        5,
        1,
        0,
    );
    let configs = [config];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    wait_for_pid(&pidfile, Duration::from_secs(5));
    loop_.tick();

    loop_.clock().advance(Duration::from_secs(10));
    loop_.run_due_health_check();
    assert_eq!(
        state_of(&loop_, 0),
        supervisor_rs::state::ProcState::Stopping
    );

    // Operator stop overrides the pending health restart.
    loop_.handle_command(&Request::Stop("web".to_string()));

    // Advance past grace so the SIGKILL fires and the process is reaped.
    loop_.clock().advance(Duration::from_secs(11));
    tick_until_reaped(&mut loop_, 0);
    for _ in 0..10 {
        loop_.tick();
    }
    assert!(
        get_pid(&loop_, 0).is_none(),
        "stop did not override the health restart"
    );
    assert_eq!(
        state_of(&loop_, 0),
        supervisor_rs::state::ProcState::Stopped,
        "process was respawned despite the operator stop"
    );
}

/// A neighbouring process without a health-check section is never probed
/// (guards against off-by-one index confusion in the runner).
#[test]
fn process_without_health_check_is_never_probed() {
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("probes");
    let with = with_exec_health(
        cfg(
            "web",
            &["/usr/bin/env", "sleep", "100"],
            RestartPolicy::Always,
        ),
        &["/usr/bin/env", "sh", "-c", &counting_ok_probe(&counter)],
        10,
        5,
        3,
        0,
    );
    let without = cfg(
        "plain",
        &["/usr/bin/env", "sleep", "100"],
        RestartPolicy::Always,
    );
    let configs = [without, with];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    ensure_running(&mut loop_, 0);
    ensure_running(&mut loop_, 1);

    for _ in 0..5 {
        loop_.clock().advance(Duration::from_secs(10));
        loop_.run_due_health_check();
    }
    // The probed process (index 1) ran; the plain one (index 0) has no counter
    // at all — which is exactly the file we look at.
    assert!(
        probe_count(&counter) >= 1,
        "the process with a check was not probed"
    );
    // The plain process at index 0 stays running and untouched.
    assert_eq!(
        state_of(&loop_, 0),
        supervisor_rs::state::ProcState::Running
    );
    assert_eq!(loop_.restart_count(0), 0);
}
