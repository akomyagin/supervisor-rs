//! cgroup v2 mechanics for per-process resource limits (Этап 10).
//!
//! This module is pure cgroupfs file mechanics: it knows nothing about
//! `SupervisorLoop` or `Clock`. Every write is a plain `std::fs` call against a
//! path, so on a real cgroup2 mount the kernel backs those paths with its own
//! files, but under a tempdir they are ordinary files — which is exactly how the
//! whole write path (`setup` / `attach` / `Drop`) is unit-tested without
//! privileges (see the tests below and the gated integration tests in
//! `tests/limits.rs`).
//!
//! Contract with cgroupfs: every write here is a *command*, not file content —
//! writing "+cpu" to `cgroup.subtree_control` enables the cpu controller, and
//! the kernel normalises what it stores. That is why the reads in the gated
//! tests compare parsed values, not raw strings.
//!
//! The cgroup is used ONLY for CPU/memory limits, never for teardown: killpg
//! (Этап 4) remains the sole stop mechanism, by the user's decision.

use crate::config::CgroupConfig;
use std::path::{Path, PathBuf};

/// Default root of the supervisor's cgroup subtree. Overridable with
/// `--cgroup-root` (test isolation; deployments with a delegated subtree).
pub const DEFAULT_CGROUP_ROOT: &str = "/sys/fs/cgroup/supervisor-rs";

/// The fixed cpu.max period, microseconds: 100 ms. `cpu-max-percent` is expressed
/// against this period (50% → quota = 50000 µs of every 100000 µs).
const CPU_MAX_PERIOD_US: u32 = 100_000;

pub fn default_root() -> PathBuf {
    PathBuf::from(DEFAULT_CGROUP_ROOT)
}

/// Formats the cgroup v2 `cpu.max` line for a percentage of one CPU over a fixed
/// 100 ms period: 50 → "50000 100000". Pure; unit-tested directly.
fn cpu_max_line(percent: u32) -> String {
    // quota = percent * period / 100; period is 100_000 µs, so quota = percent *
    // 1000 µs. Computed in u64 to avoid overflow for large percentages.
    let quota_us = u64::from(percent) * u64::from(CPU_MAX_PERIOD_US) / 100;
    format!("{quota_us} {CPU_MAX_PERIOD_US}")
}

/// A created per-process cgroup directory. Owning handle: `Drop` removes the
/// directory best-effort (`rmdir`; EBUSY/ENOENT and the like → debug-log, keep
/// going) — one mechanism covers the daemon-exit epilogue, the reload prune (the
/// entry is dropped) and the respawn handle replacement (the new instance is
/// already attached, so rmdir of the same path fails EBUSY and is a no-op). A
/// daemon killed with SIGKILL leaves the directory behind — the same class as a
/// stale state file; the next start reuses it and rewrites the limits.
#[derive(Debug)]
pub struct ProcessCgroup {
    path: PathBuf,
}

/// Creates (or reuses) `<root>/<name>` and writes its limit files:
/// 1. `create_dir_all(root)` — EEXIST is fine;
/// 2. append the needed controllers to `<root>/cgroup.subtree_control`
///    ("+cpu" iff cpu_max_percent, "+memory" iff memory_max_bytes) — incremental,
///    idempotent writes; requires the root's *ancestor* to have delegated these
///    controllers (its own subtree_control), which is outside the supervisor's
///    control — a violation surfaces here as a write error;
/// 3. `create_dir(<root>/<name>)` — EEXIST is fine (respawn reuses the directory;
///    a leftover from a SIGKILLed daemon is adopted);
/// 4. write cpu.max / memory.max for the configured keys (rewritten on every
///    spawn, which is what makes a reloaded config's new limits apply).
///
/// Any error aborts the whole setup: the caller fails the process spawn (no
/// silent "no limits" degradation).
pub fn setup(root: &Path, name: &str, cfg: &CgroupConfig) -> std::io::Result<ProcessCgroup> {
    // 1) root — EEXIST is fine (create_dir_all is idempotent).
    std::fs::create_dir_all(root)?;

    // 2) enable only the controllers this process needs, in the root's
    //    subtree_control. Each write is a command ("+cpu" / "+memory"); writing
    //    the same controller twice is idempotent for the kernel. On a tempdir
    //    (unit tests) this just appends to an ordinary file, which the test then
    //    reads back.
    let subtree_control = root.join("cgroup.subtree_control");
    if cfg.cpu_max_percent.is_some() {
        enable_controller(&subtree_control, "+cpu")?;
    }
    if cfg.memory_max_bytes.is_some() {
        enable_controller(&subtree_control, "+memory")?;
    }

    // 3) the per-process directory — EEXIST is fine (respawn / adopted leftover).
    let path = root.join(name);
    match std::fs::create_dir(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }

    // 4) write the limit files for the configured keys. Rewritten on every spawn,
    //    which is what makes a reloaded config's changed limits take effect.
    if let Some(percent) = cfg.cpu_max_percent {
        std::fs::write(path.join("cpu.max"), cpu_max_line(percent.get()))?;
    }
    if let Some(bytes) = cfg.memory_max_bytes {
        std::fs::write(path.join("memory.max"), bytes.get().to_string())?;
    }

    Ok(ProcessCgroup { path })
}

/// Appends a controller command to `cgroup.subtree_control`. On real cgroupfs
/// every write is an incremental *command* the kernel applies, not content it
/// stores, so this would be correct even truncated — but opening in append
/// mode costs nothing there and means the tempdir-simulated file in unit tests
/// accumulates every requested controller instead of only the last one,
/// letting a test assert that *both* "+cpu" and "+memory" were actually
/// requested when a config needs both.
fn enable_controller(subtree_control: &Path, controller: &str) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(subtree_control)?
        .write_all(controller.as_bytes())
}

impl ProcessCgroup {
    /// Writes `pid` into `<path>/cgroup.procs` — called from the PARENT right
    /// after spawn (formatting a pid allocates; allocation is banned in the
    /// forked child while the Этап 9 reader threads exist — plan §2 p.4).
    pub fn attach(&self, pid: u32) -> std::io::Result<()> {
        std::fs::write(self.path.join("cgroup.procs"), pid.to_string())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ProcessCgroup {
    fn drop(&mut self) {
        // Best-effort rmdir: a non-empty directory (an escaped orphan still
        // inside) yields EBUSY/ENOTEMPTY, a missing one ENOENT — both are
        // expected on some paths (respawn handle replacement, SIGKILLed daemon
        // leftovers) and are only debug-logged, never fatal.
        if let Err(e) = std::fs::remove_dir(&self.path) {
            tracing::debug!(
                path = %self.path.display(),
                error = %e,
                "best-effort cgroup rmdir failed (kept)"
            );
        }
    }
}

/// Best-effort removal of the root directory, for the `run()` epilogue: rmdir,
/// ENOENT (never created) and EBUSY/ENOTEMPTY (another daemon's dirs or an
/// escaped orphan's cgroup still inside) are debug-logged and ignored.
pub fn remove_root_best_effort(root: &Path) {
    if let Err(e) = std::fs::remove_dir(root) {
        tracing::debug!(
            path = %root.display(),
            error = %e,
            "best-effort cgroup root rmdir failed (kept)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::{NonZeroU32, NonZeroU64};
    use tempfile::TempDir;

    fn cg(cpu: Option<u32>, mem: Option<u64>) -> CgroupConfig {
        CgroupConfig {
            cpu_max_percent: cpu.map(|v| NonZeroU32::new(v).unwrap()),
            memory_max_bytes: mem.map(|v| NonZeroU64::new(v).unwrap()),
        }
    }

    #[test]
    fn cpu_max_line_formats_percent() {
        assert_eq!(cpu_max_line(50), "50000 100000");
        assert_eq!(cpu_max_line(250), "250000 100000");
        assert_eq!(cpu_max_line(1), "1000 100000");
    }

    #[test]
    fn setup_creates_dirs_and_writes_limit_files() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let cfg = cg(Some(50), Some(268435456));
        let handle = setup(&root, "web", &cfg).unwrap();

        assert!(root.join("web").is_dir());
        assert_eq!(handle.path(), root.join("web"));
        let cpu = std::fs::read_to_string(root.join("web/cpu.max")).unwrap();
        assert_eq!(cpu, "50000 100000");
        let mem = std::fs::read_to_string(root.join("web/memory.max")).unwrap();
        assert_eq!(mem, "268435456");
        // Both controllers were actually requested, not just the last one
        // (guards the append-not-truncate choice in `enable_controller`).
        let subtree = std::fs::read_to_string(root.join("cgroup.subtree_control")).unwrap();
        assert!(subtree.contains("+cpu"));
        assert!(subtree.contains("+memory"));
    }

    #[test]
    fn setup_enables_only_needed_controllers() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        // memory-only: subtree_control gets "+memory" and never "+cpu"; no
        // cpu.max is written.
        let handle = setup(&root, "web", &cg(None, Some(1048576))).unwrap();
        let subtree = std::fs::read_to_string(root.join("cgroup.subtree_control")).unwrap();
        assert_eq!(subtree, "+memory");
        assert!(!handle.path().join("cpu.max").exists());
        assert!(handle.path().join("memory.max").exists());
    }

    #[test]
    fn setup_reuses_existing_dir_and_rewrites_limits() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let h1 = setup(&root, "web", &cg(None, Some(1000))).unwrap();
        assert_eq!(
            std::fs::read_to_string(h1.path().join("memory.max")).unwrap(),
            "1000"
        );
        // Second setup on the same path with a different value succeeds (EEXIST
        // on the dir) and rewrites the limit file — the respawn/reload contract.
        let h2 = setup(&root, "web", &cg(None, Some(2000))).unwrap();
        assert_eq!(h1.path(), h2.path());
        assert_eq!(
            std::fs::read_to_string(h2.path().join("memory.max")).unwrap(),
            "2000"
        );
    }

    #[test]
    fn attach_writes_pid_to_cgroup_procs() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let handle = setup(&root, "web", &cg(None, Some(1000))).unwrap();
        handle.attach(4242).unwrap();
        let procs = std::fs::read_to_string(handle.path().join("cgroup.procs")).unwrap();
        assert_eq!(procs, "4242");
    }

    #[test]
    fn drop_removes_empty_cgroup_dir() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let handle = setup(&root, "web", &cg(None, Some(1000))).unwrap();
        // Remove the limit file setup wrote so the directory is empty and rmdir
        // can succeed (on a real cgroupfs the kernel files vanish with the dir).
        std::fs::remove_file(handle.path().join("memory.max")).unwrap();
        let path = handle.path().to_path_buf();
        drop(handle);
        assert!(!path.exists(), "empty cgroup dir must be removed on drop");
    }

    #[test]
    fn drop_keeps_nonempty_cgroup_dir() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("root");
        let handle = setup(&root, "web", &cg(None, Some(1000))).unwrap();
        // memory.max is still there → the directory is non-empty → rmdir fails
        // ENOTEMPTY → Drop is a best-effort no-op, no panic, dir stays.
        let path = handle.path().to_path_buf();
        drop(handle);
        assert!(path.exists(), "non-empty dir must survive best-effort drop");
    }

    #[test]
    fn remove_root_ignores_missing_and_nonempty() {
        let tmp = TempDir::new().unwrap();
        // Missing root: no panic.
        remove_root_best_effort(&tmp.path().join("never-created"));
        // Non-empty root: no panic, dir stays.
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("child")).unwrap();
        remove_root_best_effort(&root);
        assert!(
            root.exists(),
            "non-empty root must survive best-effort remove"
        );
    }
}
