//! In-process tests for the Этап 8 config reload: `apply_config()` (the pure
//! diff+apply) and `reload_config()` (the file-reading path) driven directly
//! against `SupervisorLoop`, advanced with `tick()` and a `FakeClock`. No
//! sockets and no config files except the two `reload_config` tests.
//!
//! `cfg`/`sh`/`DEAF_SCRIPT`/`deaf`/`wait_for_pid`/`is_alive`/`get_pid`/
//! `tick_until_reaped`/`tick_until_new_pid`/`ensure_running`/`with_exec_health`
//! are deliberately duplicated from `tests/health.rs`: every integration test
//! file is its own crate, and the existing files already made that trade — see
//! their preambles.
//!
//! **Addressing after a reload is by name, not by index.** Before Этап 8 no
//! entry was ever removed from `procs`, so index accessors were stable; a
//! reload that removes a process shifts indices. Every assertion here reads the
//! snapshot and finds the process by `name`, never `procs[index]`.

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
use supervisor_rs::state::ProcState;
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

/// A stub that writes a marker line (its own argv tag) to a file, then sleeps.
/// The marker lets a test tell "the new command is running" from "the old one".
fn marker_stub(name: &str, marker_file: &Path, tag: &str, restart: RestartPolicy) -> ProcessConfig {
    let script = format!(
        r#"echo {tag} >> "{}"
i=0
while [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done
"#,
        marker_file.display()
    );
    cfg(name, &["/usr/bin/env", "sh", "-c", &script], restart)
}

const DEAF_SCRIPT: &str = r#"trap '' TERM
echo $$ >> "$SUP_PIDFILE"
i=0
while [ $i -lt 600 ]; do sleep 0.1; i=$((i+1)); done
"#;

fn deaf(name: &str, restart: RestartPolicy, pidfile: &Path, stop_grace_secs: u64) -> ProcessConfig {
    let mut c = cfg(name, &["/usr/bin/env", "sh", "-c", DEAF_SCRIPT], restart);
    c.env = Some(BTreeMap::from([(
        "SUP_PIDFILE".to_string(),
        pidfile.display().to_string(),
    )]));
    c.stop_grace_secs = stop_grace_secs;
    c
}

fn with_exec_health(
    mut c: ProcessConfig,
    command: &[&str],
    interval_secs: u64,
    timeout_secs: u64,
    failure_threshold: u32,
    start_period_secs: u64,
) -> ProcessConfig {
    let cmd = command
        .iter()
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    let toml_src = format!(
        r#"
type = "exec"
command = [{cmd}]
interval-secs = {interval_secs}
timeout-secs = {timeout_secs}
failure-threshold = {failure_threshold}
start-period-secs = {start_period_secs}
"#
    );
    let hc: HealthCheckConfig = toml::from_str(&toml_src).expect("valid health-check toml");
    assert_eq!(hc.kind, ProbeKind::Exec);
    c.health_check = Some(hc);
    c
}

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

// ---- name-based snapshot accessors ----

fn find(
    loop_: &SupervisorLoop<FakeClock>,
    name: &str,
) -> Option<supervisor_rs::state::ProcessState> {
    loop_
        .snapshot()
        .process
        .into_iter()
        .find(|p| p.name == name)
}

fn pid_of(loop_: &SupervisorLoop<FakeClock>, name: &str) -> Option<i32> {
    find(loop_, name).and_then(|p| p.pid).map(|p| p as i32)
}

fn state_of(loop_: &SupervisorLoop<FakeClock>, name: &str) -> Option<ProcState> {
    find(loop_, name).map(|p| p.state)
}

fn restart_count_of(loop_: &SupervisorLoop<FakeClock>, name: &str) -> Option<u32> {
    find(loop_, name).map(|p| p.restart_count)
}

/// Number of processes in the snapshot.
fn proc_count(loop_: &SupervisorLoop<FakeClock>) -> usize {
    loop_.snapshot().process.len()
}

/// Ensures the process `name` has a live pid, ticking once if needed.
fn ensure_running(loop_: &mut SupervisorLoop<FakeClock>, name: &str) -> i32 {
    for _ in 0..100 {
        if let Some(pid) = pid_of(loop_, name) {
            return pid;
        }
        loop_.tick();
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("process {name} never came up");
}

/// Ticks with short real sleeps until the process `name` is fully reaped
/// (pid gone) — but still present in the snapshot.
fn tick_until_reaped(loop_: &mut SupervisorLoop<FakeClock>, name: &str) {
    for _ in 0..500 {
        loop_.tick();
        if pid_of(loop_, name).is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {name} to be reaped");
}

/// Ticks until the process `name` is alive again with a *different* pid.
fn tick_until_new_pid(
    loop_: &mut SupervisorLoop<FakeClock>,
    name: &str,
    old_pid: Option<i32>,
) -> i32 {
    for _ in 0..500 {
        loop_.tick();
        if let Some(pid) = pid_of(loop_, name) {
            if Some(pid) != old_pid {
                return pid;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for a new instance of {name}");
}

/// Ticks until the process `name` has vanished from the snapshot entirely.
fn tick_until_gone(loop_: &mut SupervisorLoop<FakeClock>, name: &str) {
    for _ in 0..500 {
        loop_.tick();
        if find(loop_, name).is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {name} to be pruned");
}

fn probe_count(path: &Path) -> usize {
    match std::fs::read_to_string(path) {
        Ok(text) => text.lines().filter(|l| !l.trim().is_empty()).count(),
        Err(_) => 0,
    }
}

fn counting_fail_probe(counter: &Path) -> String {
    format!(r#"echo x >> "{}"; exit 1"#, counter.display())
}

// ---- tests ----

#[test]
fn unchanged_process_is_untouched() {
    // A healthy process reloaded with the identical config keeps its pid and
    // restart count, and — the strictest part — its health schedule is not
    // reset mid-interval. threshold 3, interval 10, start-period 0: two failed
    // probes before the reload plus one after (still on the old schedule)
    // reach the threshold, proving the reload did not zero the failure count.
    let dir = tempfile::tempdir().unwrap();
    let counter = dir.path().join("probes");
    let base = cfg(
        "web",
        &["/usr/bin/env", "sleep", "100"],
        RestartPolicy::Always,
    );
    let config = with_exec_health(
        base,
        &["/usr/bin/env", "sh", "-c", &counting_fail_probe(&counter)],
        10,
        5,
        3,
        0,
    );
    let configs = [config.clone()];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid = ensure_running(&mut loop_, "web");

    // Two failing probes (t=10, t=20).
    for _ in 0..2 {
        loop_.clock().advance(Duration::from_secs(10));
        loop_.run_due_health_check();
    }
    assert_eq!(state_of(&loop_, "web"), Some(ProcState::Running));

    // Reload with the identical config: nothing changes, schedule intact.
    loop_.apply_config(vec![config.clone()]);
    assert_eq!(pid_of(&loop_, "web"), Some(pid), "unchanged pid changed");
    assert_eq!(restart_count_of(&loop_, "web"), Some(0));
    assert_eq!(state_of(&loop_, "web"), Some(ProcState::Running));

    // Third consecutive failure → threshold → restart. If the reload had reset
    // the counter this would still be Running.
    loop_.clock().advance(Duration::from_secs(10)); // t=30, on the OLD schedule
    loop_.run_due_health_check();
    assert_eq!(
        state_of(&loop_, "web"),
        Some(ProcState::Stopping),
        "reload reset the health schedule/counter"
    );
    assert_eq!(probe_count(&counter), 3);
}

/// Key test of the stage: a changed command forces a restart with the new
/// config.
#[test]
fn changed_command_forces_restart_with_new_config() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("markers");
    let old = marker_stub("web", &marker, "OLD", RestartPolicy::Always);
    let new = marker_stub("web", &marker, "NEW", RestartPolicy::Always);
    let configs = [old];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid = ensure_running(&mut loop_, "web");

    loop_.apply_config(vec![new]);
    assert_eq!(
        state_of(&loop_, "web"),
        Some(ProcState::Stopping),
        "changed command did not arm the stop"
    );

    tick_until_reaped(&mut loop_, "web");
    let new_pid = tick_until_new_pid(&mut loop_, "web", Some(pid));
    assert_ne!(new_pid, pid);
    assert_eq!(restart_count_of(&loop_, "web"), Some(1));

    // The new instance wrote the NEW marker.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_new = false;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&marker) {
            if text.lines().any(|l| l.trim() == "NEW") {
                saw_new = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(saw_new, "new instance did not run the new command");

    // Teardown.
    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, "web");
}

/// The new `stop-grace-secs` applies to the *current* stop: an old grace of 600
/// with a new grace of 2 must SIGKILL a deaf stub after the new, short grace.
#[test]
fn changed_stop_grace_applies_to_current_stop() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pids");
    let old = deaf("web", RestartPolicy::Always, &pidfile, 600);
    let mut new = deaf("web", RestartPolicy::Always, &pidfile, 2);
    // Make the command differ so this is a "changed" (not "unchanged") entry.
    new.env
        .as_mut()
        .unwrap()
        .insert("SUP_TAG".to_string(), "v2".to_string());
    let configs = [old];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid = wait_for_pid(&pidfile, Duration::from_secs(5));
    loop_.tick();

    loop_.apply_config(vec![new]);
    assert_eq!(state_of(&loop_, "web"), Some(ProcState::Stopping));

    // Advance past the NEW grace of 2s → SIGKILL fires.
    loop_.clock().advance(Duration::from_secs(3));
    tick_until_reaped(&mut loop_, "web");
    let deadline = Instant::now() + Duration::from_secs(5);
    while is_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!is_alive(pid), "deaf stub survived the new short grace");

    // And it respawns with the new config.
    let new_pid = tick_until_new_pid(&mut loop_, "web", Some(pid));
    assert_ne!(new_pid, pid);

    // Teardown.
    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, "web");
}

/// A changed `[process.health-check]` section is probed by the *new* probe after
/// the restart; the old probe stops running (grabla §6.3).
#[test]
fn changed_health_check_probes_with_new_probe() {
    let dir = tempfile::tempdir().unwrap();
    let counter_a = dir.path().join("probes_a");
    let counter_b = dir.path().join("probes_b");
    // Both probes succeed (exit 0) so they never force a restart; we only watch
    // which counter grows.
    let ok_a = format!(r#"echo x >> "{}"; exit 0"#, counter_a.display());
    let ok_b = format!(r#"echo x >> "{}"; exit 0"#, counter_b.display());
    let base = cfg(
        "web",
        &["/usr/bin/env", "sleep", "100"],
        RestartPolicy::Always,
    );
    let old = with_exec_health(
        base.clone(),
        &["/usr/bin/env", "sh", "-c", &ok_a],
        10,
        5,
        3,
        0,
    );
    let new = with_exec_health(base, &["/usr/bin/env", "sh", "-c", &ok_b], 10, 5, 3, 0);
    let configs = [old];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid = ensure_running(&mut loop_, "web");

    // Old probe runs once.
    loop_.clock().advance(Duration::from_secs(10));
    loop_.run_due_health_check();
    assert_eq!(probe_count(&counter_a), 1);

    // Reload with only the health-check changed → forced restart (health-check
    // is a config field).
    loop_.apply_config(vec![new]);
    assert_eq!(state_of(&loop_, "web"), Some(ProcState::Stopping));
    tick_until_reaped(&mut loop_, "web");
    let new_pid = tick_until_new_pid(&mut loop_, "web", Some(pid));
    assert_ne!(new_pid, pid);

    let a_before = probe_count(&counter_a);
    // Advance the new instance to its first probe deadline.
    loop_.clock().advance(Duration::from_secs(10));
    loop_.run_due_health_check();
    assert!(probe_count(&counter_b) >= 1, "new probe (B) did not run");
    assert_eq!(
        probe_count(&counter_a),
        a_before,
        "old probe (A) kept running after the health-check changed"
    );

    // Teardown.
    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, "web");
}

#[test]
fn removed_process_is_stopped_and_pruned() {
    let configs = [
        cfg(
            "keep",
            &["/usr/bin/env", "sleep", "100"],
            RestartPolicy::Always,
        ),
        cfg(
            "gone",
            &["/usr/bin/env", "sleep", "100"],
            RestartPolicy::Always,
        ),
    ];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let keep_pid = ensure_running(&mut loop_, "keep");
    let gone_pid = ensure_running(&mut loop_, "gone");

    // New config without "gone".
    loop_.apply_config(vec![cfg(
        "keep",
        &["/usr/bin/env", "sleep", "100"],
        RestartPolicy::Always,
    )]);
    assert_eq!(state_of(&loop_, "gone"), Some(ProcState::Stopping));

    tick_until_gone(&mut loop_, "gone");
    assert_eq!(proc_count(&loop_), 1, "removed entry not pruned");
    assert!(find(&loop_, "gone").is_none());
    let deadline = Instant::now() + Duration::from_secs(5);
    while is_alive(gone_pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!is_alive(gone_pid), "removed process's tree still alive");

    // "keep" untouched.
    assert_eq!(pid_of(&loop_, "keep"), Some(keep_pid));
    assert_eq!(restart_count_of(&loop_, "keep"), Some(0));

    // Teardown.
    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, "keep");
}

#[test]
fn removed_deaf_process_is_killed_and_pruned() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pids");
    let configs = [deaf("web", RestartPolicy::Always, &pidfile, 2)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid = wait_for_pid(&pidfile, Duration::from_secs(5));
    loop_.tick();

    // Removed from config: TERM sent, but the stub is deaf.
    loop_.apply_config(vec![]);
    assert_eq!(state_of(&loop_, "web"), Some(ProcState::Stopping));

    // Advance past grace → SIGKILL → reaped → pruned.
    loop_.clock().advance(Duration::from_secs(3));
    tick_until_gone(&mut loop_, "web");
    assert_eq!(proc_count(&loop_), 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while is_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!is_alive(pid), "deaf removed stub survived");
}

#[test]
fn removed_stopped_process_is_pruned_immediately() {
    let configs = [
        cfg(
            "keep",
            &["/usr/bin/env", "sleep", "100"],
            RestartPolicy::Always,
        ),
        cfg(
            "gone",
            &["/usr/bin/env", "sleep", "100"],
            RestartPolicy::Always,
        ),
    ];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    ensure_running(&mut loop_, "keep");
    ensure_running(&mut loop_, "gone");

    // Operator-stop "gone" and reap it: running == None.
    loop_.handle_command(&Request::Stop("gone".to_string()));
    tick_until_reaped(&mut loop_, "gone");
    assert_eq!(state_of(&loop_, "gone"), Some(ProcState::Stopped));

    // Reload without "gone": pruned in the same apply, no tick needed.
    loop_.apply_config(vec![cfg(
        "keep",
        &["/usr/bin/env", "sleep", "100"],
        RestartPolicy::Always,
    )]);
    assert!(
        find(&loop_, "gone").is_none(),
        "stopped removed entry not pruned immediately"
    );
    assert_eq!(proc_count(&loop_), 1);

    // Teardown.
    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, "keep");
}

#[test]
fn added_process_is_spawned() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("markers");
    let configs = [cfg(
        "web",
        &["/usr/bin/env", "sleep", "100"],
        RestartPolicy::Always,
    )];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let web_pid = ensure_running(&mut loop_, "web");

    loop_.apply_config(vec![
        cfg(
            "web",
            &["/usr/bin/env", "sleep", "100"],
            RestartPolicy::Always,
        ),
        marker_stub("added", &marker, "ADDED", RestartPolicy::Always),
    ]);

    let added_pid = ensure_running(&mut loop_, "added");
    assert!(added_pid > 0);
    assert_eq!(state_of(&loop_, "added"), Some(ProcState::Running));
    // Added goes to the end of the list.
    let snap = loop_.snapshot();
    assert_eq!(snap.process.last().unwrap().name, "added");
    // Its marker appeared.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw = false;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&marker) {
            if text.lines().any(|l| l.trim() == "ADDED") {
                saw = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(saw, "added process did not run");

    // "web" untouched.
    assert_eq!(pid_of(&loop_, "web"), Some(web_pid));
    assert_eq!(restart_count_of(&loop_, "web"), Some(0));

    // Teardown.
    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, "web");
    tick_until_reaped(&mut loop_, "added");
}

/// §6.3 п. 2: a process removed by one reload (TERM in flight, deaf so not yet
/// reaped) and re-added by the next reload comes back rather than vanishing.
#[test]
fn readded_during_removal_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("pids");
    // Deaf, grace 2s: the removal arms a 2s SIGKILL deadline at *this* apply's
    // signal_terminate. The re-add does not re-signal (resurrect only clears the
    // mark and arms RestartPending), so the outgoing instance is killed on the
    // deadline already in flight.
    let configs = [deaf("web", RestartPolicy::Always, &pidfile, 2)];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let pid = wait_for_pid(&pidfile, Duration::from_secs(5));
    loop_.tick();

    // Remove: TERM sent, still alive (deaf), pending_removal set.
    loop_.apply_config(vec![]);
    assert_eq!(state_of(&loop_, "web"), Some(ProcState::Stopping));
    loop_.tick();
    assert!(find(&loop_, "web").is_some(), "removed too early");

    // Re-add the same name before the old instance is reaped.
    let pidfile2 = dir.path().join("pids2");
    loop_.apply_config(vec![deaf("web", RestartPolicy::Always, &pidfile2, 2)]);

    // The outgoing deaf instance is SIGKILLed on the in-flight 2s deadline,
    // then a fresh instance respawns because resurrect armed RestartPending.
    loop_.clock().advance(Duration::from_secs(3));
    let new_pid = tick_until_new_pid(&mut loop_, "web", Some(pid));
    assert_ne!(new_pid, pid);
    assert!(
        find(&loop_, "web").is_some(),
        "process was lost during re-add"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while is_alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!is_alive(pid), "old instance not killed");

    // Teardown.
    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, "web");
}

/// Operator wins: a changed config for a user-stopped process does not start it,
/// but a later `start` uses the new command.
#[test]
fn changed_user_stopped_process_stays_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("markers");
    let old = marker_stub("web", &marker, "OLD", RestartPolicy::Always);
    let new = marker_stub("web", &marker, "NEW", RestartPolicy::Always);
    let configs = [old];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    ensure_running(&mut loop_, "web");

    // Operator stop + reap.
    loop_.handle_command(&Request::Stop("web".to_string()));
    tick_until_reaped(&mut loop_, "web");
    assert_eq!(state_of(&loop_, "web"), Some(ProcState::Stopped));

    // Reload with a changed command: still stopped, no respawn.
    loop_.apply_config(vec![new]);
    assert_eq!(
        state_of(&loop_, "web"),
        Some(ProcState::Stopped),
        "reload started a user-stopped process"
    );
    for _ in 0..10 {
        loop_.tick();
    }
    assert!(
        pid_of(&loop_, "web").is_none(),
        "user-stopped process was respawned"
    );

    // Truncate marker to isolate what the started instance writes.
    std::fs::write(&marker, b"").unwrap();
    // A later `start` uses the new command.
    loop_.handle_command(&Request::Start("web".to_string()));
    tick_until_new_pid(&mut loop_, "web", None);
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_new = false;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(&marker) {
            if text.lines().any(|l| l.trim() == "NEW") {
                saw_new = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(saw_new, "start after reload used the old command");

    // Teardown.
    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, "web");
}

/// "All or nothing": a broken new config (parse error, then a validation error)
/// changes nothing; a valid config afterwards applies.
#[test]
fn reload_error_keeps_everything() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        br#"
[[process]]
name = "web"
command = ["/usr/bin/env", "sleep", "100"]
restart = "always"
"#,
    )
    .unwrap();

    let initial = supervisor_rs::config::load(&config_path).unwrap();
    let mut loop_ = SupervisorLoop::new(&initial.process, FakeClock::new(Instant::now()))
        .with_config_reload(config_path.clone());
    let pid = ensure_running(&mut loop_, "web");

    // Parse error: nothing changes.
    std::fs::write(&config_path, b"[[process\n").unwrap();
    loop_.reload_config();
    assert_eq!(
        pid_of(&loop_, "web"),
        Some(pid),
        "parse error changed the pid"
    );
    assert_eq!(state_of(&loop_, "web"), Some(ProcState::Running));

    // Validation error (tcp health-check without a port): nothing changes.
    std::fs::write(
        &config_path,
        br#"
[[process]]
name = "web"
command = ["/usr/bin/env", "sleep", "100"]
restart = "always"

[process.health-check]
type = "tcp"
"#,
    )
    .unwrap();
    loop_.reload_config();
    assert_eq!(
        pid_of(&loop_, "web"),
        Some(pid),
        "invalid config changed the pid"
    );
    assert_eq!(state_of(&loop_, "web"), Some(ProcState::Running));

    // A valid changed config now applies.
    std::fs::write(
        &config_path,
        br#"
[[process]]
name = "web"
command = ["/usr/bin/env", "sleep", "200"]
restart = "always"
"#,
    )
    .unwrap();
    loop_.reload_config();
    assert_eq!(
        state_of(&loop_, "web"),
        Some(ProcState::Stopping),
        "valid reload after failures did not apply"
    );

    tick_until_reaped(&mut loop_, "web");
    let new_pid = tick_until_new_pid(&mut loop_, "web", Some(pid));
    assert_ne!(new_pid, pid);

    // Teardown.
    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    loop_.escalate_to_kill();
    tick_until_reaped(&mut loop_, "web");
}

/// SIGHUP during shutdown is ignored: nothing is spawned, and the shutdown
/// completes normally.
#[test]
fn reload_ignored_during_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        br#"
[[process]]
name = "web"
command = ["/usr/bin/env", "sleep", "100"]
restart = "always"

[[process]]
name = "added"
command = ["/usr/bin/env", "sleep", "100"]
restart = "always"
"#,
    )
    .unwrap();

    let configs = [cfg(
        "web",
        &["/usr/bin/env", "sleep", "100"],
        RestartPolicy::Always,
    )];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()))
        .with_config_reload(config_path.clone());
    ensure_running(&mut loop_, "web");

    loop_.begin_shutdown(nix::sys::signal::Signal::SIGTERM);
    // The config file lists an "added" process; a reload would spawn it — but
    // during shutdown reload_config no-ops.
    loop_.reload_config();
    assert!(
        find(&loop_, "added").is_none(),
        "reload spawned during shutdown"
    );
    assert_eq!(proc_count(&loop_), 1);

    // Shutdown still completes.
    tick_until_reaped(&mut loop_, "web");
    for _ in 0..5 {
        loop_.tick();
    }
    assert_eq!(state_of(&loop_, "web"), Some(ProcState::Stopped));
}

/// Removing every process ends the run: after reaping, `procs` is empty.
#[test]
fn removing_every_process_ends_the_run() {
    let configs = [
        cfg(
            "a",
            &["/usr/bin/env", "sleep", "100"],
            RestartPolicy::Always,
        ),
        cfg(
            "b",
            &["/usr/bin/env", "sleep", "100"],
            RestartPolicy::Always,
        ),
    ];
    let mut loop_ = SupervisorLoop::new(&configs, FakeClock::new(Instant::now()));
    let a_pid = ensure_running(&mut loop_, "a");
    let b_pid = ensure_running(&mut loop_, "b");

    loop_.apply_config(vec![]);
    // Both stopping.
    assert_eq!(state_of(&loop_, "a"), Some(ProcState::Stopping));
    assert_eq!(state_of(&loop_, "b"), Some(ProcState::Stopping));

    for _ in 0..500 {
        loop_.tick();
        if proc_count(&loop_) == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(proc_count(&loop_), 0, "procs not fully cleared");

    let deadline = Instant::now() + Duration::from_secs(5);
    while (is_alive(a_pid) || is_alive(b_pid)) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        !is_alive(a_pid) && !is_alive(b_pid),
        "leftover live processes"
    );
}
