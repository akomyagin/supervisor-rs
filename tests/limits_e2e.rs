//! End-to-end tests for Этап 10 resource limits on the real `supervisor-rs`
//! binary: a process with a `[process.rlimit]` section really carries the
//! limits (observed via the stub's `ulimit`); an invalid section fails `run`
//! with a config error; and — where the environment allows — a process with a
//! `[process.cgroup]` section lands in its cgroup, carries the limit values, and
//! the directories are cleaned up on SIGTERM (the acceptance criterion whole).
//!
//! `state_path`/`socket_path`/`start_supervisor`/`wait_for_state`/
//! `wait_with_timeout` are deliberately duplicated from `tests/logs_e2e.rs`, and
//! the cgroup gate (`cgroup_root_for_test` / `try_find_writable_cgroup_root`)
//! from `tests/limits.rs`: every integration test file is its own crate and the
//! existing files already made that trade.
//!
//! Every daemon isolates **both** `--state-file` and `--control-socket` in its
//! own tempdir; the cgroup test additionally passes a gated `--cgroup-root`.
//!
//! The cgroup gate is documented in `tests/limits.rs`: a cgroup test asks for a
//! writable root and returns early (LOUDLY, `CGROUP-TEST SKIPPED` on stderr) if
//! there is none — unless `SUPERVISOR_RS_REQUIRE_CGROUP_TESTS=1`, which turns a
//! skip into a panic. It checks the action, not `test -w`.

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

/// Like `start_supervisor` but also passes `--cgroup-root` (the cgroup e2e test).
fn start_supervisor_with_cgroup(
    config_path: &Path,
    state_path: &Path,
    socket_path: &Path,
    cgroup_root: &Path,
) -> Child {
    Command::new(env!("CARGO_BIN_EXE_supervisor-rs"))
        .arg("run")
        .arg(config_path)
        .arg("--state-file")
        .arg(state_path)
        .arg("--control-socket")
        .arg(socket_path)
        .arg("--cgroup-root")
        .arg(cgroup_root)
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
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ---- cgroup gate (§9.1, duplicated from tests/limits.rs) ----

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

fn try_find_writable_cgroup_root(tag: &str) -> Result<PathBuf, String> {
    const MOUNT: &str = "/sys/fs/cgroup";
    if !Path::new(MOUNT).is_dir() {
        return Err(format!("{MOUNT} is not a directory (no cgroup v2 mount)"));
    }
    let self_cgroup = std::fs::read_to_string("/proc/self/cgroup")
        .map_err(|e| format!("cannot read /proc/self/cgroup: {e}"))?;
    let rel = self_cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .ok_or_else(|| "no unified (0::) cgroup line — not cgroup v2".to_string())?
        .trim()
        .to_string();

    let mut dir = PathBuf::from(MOUNT);
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

fn try_probe_ancestor(ancestor: &Path, probe_name: &str) -> Result<PathBuf, String> {
    let probe = ancestor.join(probe_name);
    let subtree = ancestor.join("cgroup.subtree_control");
    let _ = std::fs::write(&subtree, "+cpu");
    let _ = std::fs::write(&subtree, "+memory");

    std::fs::create_dir(&probe).map_err(|e| format!("mkdir {}: {e}", probe.display()))?;

    let controllers = std::fs::read_to_string(probe.join("cgroup.controllers")).unwrap_or_default();
    let has_cpu = controllers.split_whitespace().any(|c| c == "cpu");
    let has_mem = controllers.split_whitespace().any(|c| c == "memory");
    if has_cpu && has_mem {
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

fn read_limit_u64(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    text.split_whitespace().next()?.parse().ok()
}

// ==== tests ====

/// A process with a `[process.rlimit]` section really carries the limits: the
/// stub reports its `ulimit -n / -v / -t` into a marker, and the daemon shuts
/// down cleanly on SIGTERM. No gate — rlimit works everywhere and only downward.
#[test]
fn rlimit_applied_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("limits.txt");
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[[process]]
name = "web"
restart = "never"
command = ["/usr/bin/env", "sh", "-c", "echo \"$(ulimit -n) $(ulimit -v) $(ulimit -t)\" > '{marker}'; sleep 60"]

[process.rlimit]
nofile = 123
as-bytes = 536870912
cpu-secs = 111
"#,
            marker = marker.display()
        ),
    )
    .unwrap();

    let state_path = state_path(dir.path());
    let socket_path = socket_path(dir.path());
    let mut supervisor = start_supervisor(&config_path, &state_path, &socket_path);

    wait_for_state(&state_path, web_running, READY_TIMEOUT);
    let line = wait_for_marker(&marker, READY_TIMEOUT);
    assert_eq!(line, "123 524288 111", "ulimit output: {line:?}");

    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM).unwrap();
    let exit = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
    assert_eq!(exit.code(), Some(0));
}

/// An empty `[process.rlimit]` section fails `run` with a config error (exit 1,
/// the existing class), and no state file is created (the
/// `invalid_log_section_fails_run_with_config_error` precedent).
#[test]
fn invalid_limits_section_fails_run_with_config_error() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[[process]]
name = "web"
command = ["/usr/bin/env", "true"]

[process.rlimit]
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
        combined.contains("nofile") || combined.contains("rlimit"),
        "config error did not mention the empty rlimit section: {combined}"
    );
    assert!(
        !state_path.exists(),
        "a state file was created despite the config error"
    );
}

/// The acceptance criterion, whole (gated §9.1): a process with a
/// `[process.cgroup]` section, given a delegated `--cgroup-root`, lands in
/// `<root>/<name>/cgroup.procs`; `memory.max` carries the value; SIGTERM → exit 0
/// → the per-process directory and the root are removed (runtime artifact, §2.1),
/// and the state file and socket are removed (existing contract intact).
#[test]
fn cgroup_end_to_end_created_attached_and_removed() {
    let Some(root) = cgroup_root_for_test("e2e") else {
        return; // skipped LOUDLY inside the helper; never a silent pass
    };
    let name = format!("suptest-e2e-{}", std::process::id());
    // The daemon creates `<root>/supervisor-rs-e2e-<pid>` under the delegated
    // ancestor; pass that whole ancestor as --cgroup-root. Use a unique per-run
    // subdir of the delegated root so parallel runs and the root cleanup don't
    // collide with the delegated ancestor itself.
    let run_root = root.join(format!("suprun-{}", std::process::id()));
    std::fs::create_dir_all(&run_root).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            r#"
[[process]]
name = "{name}"
restart = "never"
command = ["/usr/bin/env", "sh", "-c", "sleep 60"]

[process.cgroup]
cpu-max-percent = 50
memory-max-bytes = 268435456
"#,
        ),
    )
    .unwrap();

    let state_path = state_path(dir.path());
    let socket_path = socket_path(dir.path());
    let mut supervisor =
        start_supervisor_with_cgroup(&config_path, &state_path, &socket_path, &run_root);

    let snapshot = wait_for_state(&state_path, web_running, READY_TIMEOUT);
    let pid = snapshot
        .process
        .first()
        .and_then(|p| p.pid)
        .expect("running process must publish a pid");

    let cgdir = run_root.join(&name);
    // pid appears in cgroup.procs (attach is after spawn).
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Ok(text) = std::fs::read_to_string(cgdir.join("cgroup.procs")) {
            if text.split_whitespace().any(|t| t == pid.to_string()) {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "pid {pid} never appeared in {}/cgroup.procs",
            cgdir.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        read_limit_u64(&cgdir.join("memory.max")),
        Some(268435456),
        "memory.max mismatch"
    );

    kill(Pid::from_raw(supervisor.id() as i32), Signal::SIGTERM).unwrap();
    let exit = wait_with_timeout(&mut supervisor, SHUTDOWN_TIMEOUT);
    assert_eq!(exit.code(), Some(0));

    // The per-process dir and the run root are removed (runtime artifact).
    assert!(
        !cgdir.exists(),
        "per-process cgroup dir was not removed on clean exit: {}",
        cgdir.display()
    );
    assert!(
        !run_root.exists(),
        "cgroup root was not removed on clean exit: {}",
        run_root.display()
    );
    // Existing contract intact.
    assert!(!state_path.exists(), "state file not removed on clean exit");
    assert!(!socket_path.exists(), "socket not removed on clean exit");

    // Best-effort: if something above left the run root behind, clean up.
    let _ = std::fs::remove_dir_all(&run_root);
}
