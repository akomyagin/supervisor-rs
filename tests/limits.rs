//! In-process tests for Этап 10 resource limits: `SupervisorLoop` driven
//! directly with `tick()` and a `FakeClock`.
//!
//! `cfg`/`sh`/`wait_for_pid`/`is_alive`/`get_pid`/`tick_until_reaped`/
//! `tick_until_new_pid` are duplicated from `tests/logs.rs` — every integration
//! test file is its own crate and the existing files already made that trade
//! (see their preambles). The cgroup gate helper (`cgroup_root_for_test` /
//! `try_find_writable_cgroup_root`) is likewise duplicated in `tests/limits_e2e.rs`.
//!
//! **The rlimit observation trick**: the stub reads its own limits with the
//! `ulimit` shell builtin and writes them to a marker file — deterministic,
//! unprivileged, no dying process. `ulimit -n` prints soft NOFILE, `ulimit -v`
//! the address space in KiB (so config as-bytes must be a multiple of 1024),
//! `ulimit -t` CPU seconds.
//!
//! **The cgroup gate** (§9.1 of the plan — the first conditionally-skipped test
//! class in this project). A cgroup test creates a real cgroup subtree, which
//! this unprivileged sandbox cannot do at `/sys/fs/cgroup`. Rather than fail or
//! hang, such a test asks `cgroup_root_for_test` for a writable root and returns
//! early ("skipped") if there is none — LOUDLY: the reason goes to stderr
//! (visible under `--nocapture`), and `SUPERVISOR_RS_REQUIRE_CGROUP_TESTS=1`
//! turns every skip into a panic, so a gate bug that skips where privileges DO
//! exist cannot silently mask a regression. The gate checks the *action* (the
//! same mkdir + subtree_control chain the daemon runs), not `test -w`: writability
//! of the top level is not the same as being able to create a subtree with
//! controllers (delegation can be deep). "Test runs ⟺ the daemon here could too."

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use supervisor_rs::clock::FakeClock;
use supervisor_rs::config::{ProcessConfig, RestartPolicy, DEFAULT_STOP_GRACE_SECS};
use supervisor_rs::control::Request;
use supervisor_rs::supervise::SupervisorLoop;

// ---- config helpers (copied from tests/logs.rs) ----

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

fn sh(name: &str, script: &str, restart: RestartPolicy) -> ProcessConfig {
    cfg(name, &["/usr/bin/env", "sh", "-c", script], restart)
}

/// Attaches a `[process.rlimit]` section built via `toml::from_str` (the
/// `with_exec_health` precedent).
fn with_rlimit(mut c: ProcessConfig, rlimit_toml: &str) -> ProcessConfig {
    c.rlimit = Some(toml::from_str(rlimit_toml).expect("valid rlimit section"));
    c
}

/// Attaches a `[process.cgroup]` section built via `toml::from_str`.
fn with_cgroup(mut c: ProcessConfig, cgroup_toml: &str) -> ProcessConfig {
    c.cgroup = Some(toml::from_str(cgroup_toml).expect("valid cgroup section"));
    c
}

// ---- process-liveness helpers (copied from tests/logs.rs) ----

fn is_alive(pid: i32) -> bool {
    kill(Pid::from_raw(pid), None) != Err(Errno::ESRCH)
}

fn get_pid(loop_: &SupervisorLoop<FakeClock>, name: &str) -> Option<i32> {
    loop_
        .snapshot()
        .process
        .into_iter()
        .find(|p| p.name == name)
        .and_then(|p| p.pid)
        .map(|p| p as i32)
}

fn restart_count_of(loop_: &SupervisorLoop<FakeClock>, name: &str) -> Option<u32> {
    loop_
        .snapshot()
        .process
        .into_iter()
        .find(|p| p.name == name)
        .map(|p| p.restart_count)
}

fn state_is_running(loop_: &SupervisorLoop<FakeClock>, name: &str) -> bool {
    loop_
        .snapshot()
        .process
        .into_iter()
        .find(|p| p.name == name)
        .map(|p| matches!(p.state, supervisor_rs::state::ProcState::Running))
        .unwrap_or(false)
}

/// Ticks until `name` has a live pid, or panics.
fn tick_until_running(loop_: &mut SupervisorLoop<FakeClock>, name: &str) -> i32 {
    for _ in 0..500 {
        if let Some(pid) = get_pid(loop_, name) {
            return pid;
        }
        loop_.tick();
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("process {name} never came up");
}

/// Ticks with short real sleeps until `name` is fully reaped.
fn tick_until_reaped(loop_: &mut SupervisorLoop<FakeClock>, name: &str) {
    for _ in 0..500 {
        loop_.tick();
        if get_pid(loop_, name).is_none() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {name} to be reaped");
}

/// Ticks until `name` is alive again with a *different* pid.
fn tick_until_new_pid(
    loop_: &mut SupervisorLoop<FakeClock>,
    name: &str,
    old_pid: Option<i32>,
) -> i32 {
    for _ in 0..500 {
        loop_.tick();
        if let Some(pid) = get_pid(loop_, name) {
            if Some(pid) != old_pid {
                return pid;
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for a new instance of {name}");
}

/// Polls a marker file's content with a real deadline until non-empty. The stub
/// writes it from another process, asynchronously to `FakeClock` — the same
/// two-mode discipline as Этап 9 (`wait_for_log`): real time to observe a live
/// process. Returns the trimmed content.
fn wait_for_marker(path: &Path, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            let line = text.trim();
            if !line.is_empty() {
                return line.to_string();
            }
        }
        if Instant::now() >= deadline {
            panic!("marker {} not written within {timeout:?}", path.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ---- cgroup gate (§9.1) ----

/// Finds a writable cgroup-v2 root for this test, or `None` — in which case the
/// caller MUST return early (a conditionally-skipped test). Never silent: the
/// reason goes to stderr, and `SUPERVISOR_RS_REQUIRE_CGROUP_TESTS=1` turns a skip
/// into a panic.
fn cgroup_root_for_test(tag: &str) -> Option<PathBuf> {
    match try_find_writable_cgroup_root(tag) {
        Ok(root) => Some(root),
        Err(reason) => {
            if std::env::var_os("SUPERVISOR_RS_REQUIRE_CGROUP_TESTS").is_some() {
                panic!(
                    "cgroup test cannot run here ({reason}), but \
                     SUPERVISOR_RS_REQUIRE_CGROUP_TESTS demands it"
                );
            }
            eprintln!("CGROUP-TEST SKIPPED ({tag}): {reason}");
            None
        }
    }
}

/// Reads our own cgroup from `/proc/self/cgroup` ("0::<path>") and walks from
/// `/sys/fs/cgroup/<path>` up towards `/sys/fs/cgroup`, trying at each ancestor A
/// to (1) mkdir `A/suptest-<pid>-<tag>`, (2) ensure "cpu memory" are usable inside
/// it (its `cgroup.controllers`), enabling them in A's `cgroup.subtree_control`
/// if needed. Under systemd user delegation this succeeds somewhere below
/// `user@<uid>.service`; without delegation nothing is writable and we skip. The
/// returned probe directory is the caller's `--cgroup-root` and is removed by the
/// caller.
fn try_find_writable_cgroup_root(tag: &str) -> Result<PathBuf, String> {
    const MOUNT: &str = "/sys/fs/cgroup";
    if !Path::new(MOUNT).is_dir() {
        return Err(format!("{MOUNT} is not a directory (no cgroup v2 mount)"));
    }
    let self_cgroup = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| format!("cannot read /proc/self/cgroup: {e}"))?;
    // cgroup v2 is the single "0::<path>" line.
    let rel = self_cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .ok_or_else(|| "no unified (0::) cgroup line — not cgroup v2".to_string())?
        .trim()
        .to_string();

    let mut dir = PathBuf::from(MOUNT);
    // rel is like "/user.slice/user-1000.slice/user@1000.service/app.slice/...".
    for comp in rel.trim_start_matches('/').split('/') {
        if !comp.is_empty() {
            dir.push(comp);
        }
    }

    let probe_name = format!("suptest-{}-{}", std::process::id(), tag);
    let mount = PathBuf::from(MOUNT);
    let mut candidate = dir;
    loop {
        if let Ok(root) = try_probe_ancestor(&candidate, &probe_name) {
            return Ok(root);
        }
        if candidate == mount {
            break;
        }
        match candidate.parent() {
            Some(parent) if parent.starts_with(MOUNT) => candidate = parent.to_path_buf(),
            _ => break,
        }
    }
    Err(format!(
        "no writable cgroup v2 subtree with cpu+memory controllers found \
         under {MOUNT} (unprivileged sandbox, no delegation)"
    ))
}

/// Tries to make `ancestor/<probe_name>` a usable cgroup: mkdir it, ensure cpu
/// and memory are available inside it, enabling them in `ancestor`'s
/// subtree_control if needed. Returns the probe dir on success; cleans up and
/// errors otherwise.
fn try_probe_ancestor(ancestor: &Path, probe_name: &str) -> Result<PathBuf, String> {
    let probe = ancestor.join(probe_name);
    // Enable controllers in the ancestor first (allowed only while it has no
    // direct member processes — the "no internal processes" rule). Best-effort:
    // if they are already delegated, writing again is idempotent.
    let subtree = ancestor.join("cgroup.subtree_control");
    let _ = std::fs::write(&subtree, "+cpu");
    let _ = std::fs::write(&subtree, "+memory");

    std::fs::create_dir(&probe).map_err(|e| format!("mkdir {}: {e}", probe.display()))?;

    // The controllers must now be visible inside the probe dir.
    let controllers = std::fs::read_to_string(probe.join("cgroup.controllers")).unwrap_or_default();
    let has_cpu = controllers.split_whitespace().any(|c| c == "cpu");
    let has_mem = controllers.split_whitespace().any(|c| c == "memory");
    if has_cpu && has_mem {
        // Remove the probe dir: the caller passes `ancestor` as the cgroup root
        // and the daemon creates its own `<root>/<name>` beneath it.
        let _ = std::fs::remove_dir(&probe);
        Ok(ancestor.to_path_buf())
    } else {
        let _ = std::fs::remove_dir(&probe);
        Err(format!(
            "probe under {} lacks cpu+memory controllers (have: {controllers:?})",
            ancestor.display()
        ))
    }
}

/// Reads a limit file and parses its first whitespace-separated token as u64.
/// On real cgroupfs the kernel normalises what it stores, so tests compare
/// parsed values, not raw strings.
fn read_limit_u64(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    text.split_whitespace().next()?.parse().ok()
}

/// Polls `cgroup.procs` under `dir` until it contains `pid` (attach happens after
/// spawn, asynchronously to the main loop).
fn wait_for_pid_in_cgroup(dir: &Path, pid: i32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(dir.join("cgroup.procs")) {
            if text.split_whitespace().any(|t| t == pid.to_string()) {
                return true;
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ==== rlimit tests (no gate) ====

/// The stub reports its own ulimits into a marker; the supervised instance must
/// carry the configured NOFILE / address space / CPU limits.
#[test]
fn rlimit_marker_shows_configured_limits() {
    let tmp = tempfile::TempDir::new().unwrap();
    let marker = tmp.path().join("limits.txt");
    let script = format!(
        r#"echo "$(ulimit -n) $(ulimit -v) $(ulimit -t)" > "{}"; sleep 30"#,
        marker.display()
    );
    let c = with_rlimit(
        sh("limited", &script, RestartPolicy::Never),
        "nofile = 123\nas-bytes = 536870912\ncpu-secs = 111\n",
    );
    let mut loop_ = SupervisorLoop::new(&[c], FakeClock::new(Instant::now()));
    tick_until_running(&mut loop_, "limited");
    let line = wait_for_marker(&marker, Duration::from_secs(10));
    assert_eq!(line, "123 524288 111", "ulimit output: {line:?}");
    loop_.handle_command(&Request::Stop("limited".to_string()));
    tick_until_reaped(&mut loop_, "limited");
}

/// A restart (operator) keeps the limits: the new instance's marker shows the
/// same values.
#[test]
fn restarted_instance_keeps_rlimits() {
    let tmp = tempfile::TempDir::new().unwrap();
    let marker = tmp.path().join("limits.txt");
    // Truncate the marker each run so the second instance's value is observable.
    let script = format!(r#"echo "$(ulimit -n)" > "{}"; sleep 30"#, marker.display());
    let c = with_rlimit(sh("web", &script, RestartPolicy::Always), "nofile = 321\n");
    let mut loop_ = SupervisorLoop::new(&[c], FakeClock::new(Instant::now()));
    let pid1 = tick_until_running(&mut loop_, "web");
    assert_eq!(wait_for_marker(&marker, Duration::from_secs(10)), "321");

    // Remove the marker so we can detect the new instance writing it.
    std::fs::remove_file(&marker).ok();
    loop_.handle_command(&Request::Restart("web".to_string()));
    let pid2 = tick_until_new_pid(&mut loop_, "web", Some(pid1));
    assert_ne!(pid1, pid2);
    assert_eq!(wait_for_marker(&marker, Duration::from_secs(10)), "321");

    loop_.handle_command(&Request::Stop("web".to_string()));
    tick_until_reaped(&mut loop_, "web");
}

/// A reload that changes only `nofile` forces a restart with the new limit — with
/// no change to the reload logic (§7 p.7): the derived `PartialEq` on the new
/// `rlimit` field makes `plan_reload` see the config as changed.
#[test]
fn changed_rlimit_forces_restart_with_new_limits() {
    let tmp = tempfile::TempDir::new().unwrap();
    let marker = tmp.path().join("limits.txt");
    let script = format!(r#"echo "$(ulimit -n)" > "{}"; sleep 30"#, marker.display());
    let c = with_rlimit(sh("web", &script, RestartPolicy::Always), "nofile = 100\n");
    let mut loop_ = SupervisorLoop::new(&[c], FakeClock::new(Instant::now()));
    let pid1 = tick_until_running(&mut loop_, "web");
    assert_eq!(wait_for_marker(&marker, Duration::from_secs(10)), "100");
    assert_eq!(restart_count_of(&loop_, "web"), Some(0));

    std::fs::remove_file(&marker).ok();
    let new = with_rlimit(sh("web", &script, RestartPolicy::Always), "nofile = 200\n");
    loop_.apply_config(vec![new]);
    let pid2 = tick_until_new_pid(&mut loop_, "web", Some(pid1));
    assert_ne!(pid1, pid2);
    assert_eq!(restart_count_of(&loop_, "web"), Some(1));
    assert_eq!(
        wait_for_marker(&marker, Duration::from_secs(10)),
        "200",
        "new instance must carry the reloaded limit"
    );

    loop_.handle_command(&Request::Stop("web".to_string()));
    tick_until_reaped(&mut loop_, "web");
}

/// Policy §2.1 without privileges: a cgroup section with an unwritable root fails
/// the spawn of *that* process only; the plain neighbour comes up, and
/// `had_start_errors` is set. `<tempfile>/sub` gives ENOTDIR from create_dir_all.
#[test]
fn cgroup_spawn_failure_is_per_process() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let bad_root = file.path().join("sub");
    let c_bad = with_cgroup(
        sh("bad", "sleep 30", RestartPolicy::Never),
        "memory-max-bytes = 1048576\n",
    );
    let c_plain = sh("plain", "sleep 30", RestartPolicy::Never);
    let mut loop_ = SupervisorLoop::new_with_cgroup_root(
        &[c_bad, c_plain],
        FakeClock::new(Instant::now()),
        Some(bad_root),
    );

    assert!(loop_.had_start_errors(), "bad cgroup must set start errors");
    // "bad" is not tracked; "plain" is, and comes up.
    assert!(
        get_pid(&loop_, "bad").is_none(),
        "bad process must not be tracked"
    );
    let pid = tick_until_running(&mut loop_, "plain");
    assert!(is_alive(pid));
    assert!(state_is_running(&loop_, "plain"));

    loop_.handle_command(&Request::Stop("plain".to_string()));
    tick_until_reaped(&mut loop_, "plain");
}

// ==== cgroup tests (gated, §9.1) ====

#[test]
fn cgroup_attaches_pid_and_writes_limits() {
    let Some(root) = cgroup_root_for_test("attach") else {
        return; // skipped LOUDLY inside the helper; never a silent pass
    };
    let name = format!("suptest-attach-{}", std::process::id());
    let c = with_cgroup(
        sh(&name, "sleep 30", RestartPolicy::Never),
        "cpu-max-percent = 50\nmemory-max-bytes = 268435456\n",
    );
    let mut loop_ = SupervisorLoop::new_with_cgroup_root(
        &[c],
        FakeClock::new(Instant::now()),
        Some(root.clone()),
    );
    let pid = tick_until_running(&mut loop_, &name);
    let cgdir = root.join(&name);

    assert!(
        wait_for_pid_in_cgroup(&cgdir, pid, Duration::from_secs(10)),
        "pid {pid} never appeared in {}/cgroup.procs",
        cgdir.display()
    );
    assert_eq!(
        read_limit_u64(&cgdir.join("memory.max")),
        Some(268435456),
        "memory.max mismatch"
    );
    // cpu.max is "<quota> <period>"; first token is the quota.
    assert_eq!(
        read_limit_u64(&cgdir.join("cpu.max")),
        Some(50000),
        "cpu.max quota mismatch"
    );

    loop_.handle_command(&Request::Stop(name.clone()));
    tick_until_reaped(&mut loop_, &name);
    drop(loop_); // epilogue is not run here; clean up the probe dir ourselves.
    let _ = std::fs::remove_dir(&cgdir);
}

#[test]
fn respawned_instance_lands_in_same_cgroup() {
    let Some(root) = cgroup_root_for_test("respawn") else {
        return;
    };
    let name = format!("suptest-respawn-{}", std::process::id());
    let c = with_cgroup(
        sh(&name, "sleep 30", RestartPolicy::Always),
        "memory-max-bytes = 268435456\n",
    );
    let mut loop_ = SupervisorLoop::new_with_cgroup_root(
        &[c],
        FakeClock::new(Instant::now()),
        Some(root.clone()),
    );
    let pid1 = tick_until_running(&mut loop_, &name);
    let cgdir = root.join(&name);
    assert!(wait_for_pid_in_cgroup(
        &cgdir,
        pid1,
        Duration::from_secs(10)
    ));

    loop_.handle_command(&Request::Restart(name.clone()));
    let pid2 = tick_until_new_pid(&mut loop_, &name, Some(pid1));
    assert_ne!(pid1, pid2);
    assert!(
        wait_for_pid_in_cgroup(&cgdir, pid2, Duration::from_secs(10)),
        "respawned pid {pid2} not in the same cgroup"
    );

    loop_.handle_command(&Request::Stop(name.clone()));
    tick_until_reaped(&mut loop_, &name);
    drop(loop_);
    let _ = std::fs::remove_dir(&cgdir);
}

#[test]
fn stopped_process_leaves_empty_cgroup_dir() {
    let Some(root) = cgroup_root_for_test("stopped") else {
        return;
    };
    let name = format!("suptest-stopped-{}", std::process::id());
    let c = with_cgroup(
        sh(&name, "sleep 30", RestartPolicy::Never),
        "memory-max-bytes = 268435456\n",
    );
    let mut loop_ = SupervisorLoop::new_with_cgroup_root(
        &[c],
        FakeClock::new(Instant::now()),
        Some(root.clone()),
    );
    let pid = tick_until_running(&mut loop_, &name);
    let cgdir = root.join(&name);
    assert!(wait_for_pid_in_cgroup(&cgdir, pid, Duration::from_secs(10)));

    loop_.handle_command(&Request::Stop(name.clone()));
    tick_until_reaped(&mut loop_, &name);

    // The process left the cgroup (asynchronous to its death) — poll for empty.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let procs = std::fs::read_to_string(cgdir.join("cgroup.procs")).unwrap_or_default();
        if procs.trim().is_empty() {
            break;
        }
        assert!(Instant::now() < deadline, "cgroup.procs never emptied");
        std::thread::sleep(Duration::from_millis(10));
    }
    // The directory still exists — it is only removed in the daemon's epilogue.
    assert!(cgdir.exists(), "cgroup dir must survive until the epilogue");

    drop(loop_);
    let _ = std::fs::remove_dir(&cgdir);
}
