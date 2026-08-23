//! Config parsing for supervisor-rs.
//!
//! The config is a TOML file describing the set of processes to supervise.
//! Parsing is pure (no side effects beyond reading the file), so it is easy
//! to unit-test with `toml::from_str` directly.
//!
//! ```toml
//! [[process]]
//! name = "web"
//! command = ["/usr/bin/myserver", "--port", "8080"]
//! restart = "on-failure"   # always | on-failure | never
//! ```

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::{NonZeroU16, NonZeroU32, NonZeroU64};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default grace period between the shutdown signal and SIGKILL, seconds.
pub const DEFAULT_STOP_GRACE_SECS: u64 = 5;

/// Default seconds between health-check probes (Этап 7).
pub const DEFAULT_HEALTH_INTERVAL_SECS: u64 = 10;
/// Default per-probe timeout, seconds (Этап 7).
pub const DEFAULT_HEALTH_TIMEOUT_SECS: u64 = 5;
/// Default number of consecutive probe failures that forces a restart (Этап 7).
pub const DEFAULT_HEALTH_FAILURE_THRESHOLD: u32 = 3;
// start-period default is 0 — no separate constant, `#[serde(default)]` gives
// the `u64` zero directly.

/// Default rotation threshold for a captured log file, bytes (10 MiB) (Этап 9).
pub const DEFAULT_LOG_MAX_SIZE_BYTES: u64 = 10 * 1024 * 1024;
/// Default number of rotated files kept (`.1` … `.keep`), besides the current
/// file (Этап 9).
pub const DEFAULT_LOG_KEEP: u32 = 5;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub process: Vec<ProcessConfig>,
}

/// `Clone` + `PartialEq` support the Этап 8 config reload: `Clone` lets `new()`
/// wrap each config in an owning `Arc`, and `PartialEq` is the by-name diff's
/// "changed / unchanged" test. `Eq` is not derived — only equality is needed.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProcessConfig {
    pub name: String,
    pub command: Vec<String>,
    #[serde(default)]
    pub workdir: Option<PathBuf>,
    #[serde(default)]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub restart: RestartPolicy,
    /// Seconds between the shutdown signal sent to the process group and the
    /// SIGKILL escalation. Whole seconds by design: a TOML integer needs no
    /// duration parser, and negative values are rejected by `u64` itself. The
    /// key is kebab-case to match the `on-failure` style of `restart`; the
    /// older single-word keys keep their names, hence a per-field rename
    /// rather than `rename_all` on the struct.
    #[serde(rename = "stop-grace-secs", default = "default_stop_grace_secs")]
    pub stop_grace_secs: u64,
    /// Optional active health check (Этап 7). A `[process.health-check]`
    /// subtable, kept before `log` in the struct so the "scalars before
    /// subtables" TOML rule holds (same convention as `StateSnapshot`), even
    /// though `ProcessConfig` only derives `Deserialize`.
    #[serde(rename = "health-check", default)]
    pub health_check: Option<HealthCheckConfig>,
    /// Optional stdout/stderr capture with size-based rotation (Этап 9). A
    /// `[process.log]` subtable; without it stdio is inherited, exactly as
    /// before. Kept last of the subtables (subtables ordered by stage of
    /// appearance) so the "scalars before subtables" TOML rule holds. Derived
    /// `Clone`/`PartialEq` pick the field up automatically, so the Этап 8
    /// reload diff sees a changed log section as a config change and forces a
    /// restart with no new branch.
    #[serde(default)]
    pub log: Option<LogConfig>,
    /// Optional per-process rlimits (Этап 10), applied via setrlimit(2) in the
    /// pre_exec hook next to `setsid()`, inherited by every descendant. A
    /// `[process.rlimit]` subtable; without it limits are inherited from the
    /// supervisor exactly as before. Derived `Clone`/`PartialEq` pick the field
    /// up automatically, so the Этап 8 reload diff sees a changed rlimit section
    /// as a config change and forces a restart with no new branch.
    #[serde(default)]
    pub rlimit: Option<RlimitConfig>,
    /// Optional cgroup v2 limits for the whole supervised tree (Этап 10). A
    /// `[process.cgroup]` subtable; without it no cgroup is touched. Kept last of
    /// the subtables so the "scalars before subtables" TOML rule holds. Derived
    /// `Clone`/`PartialEq` make the Этап 8 reload diff treat a changed section as
    /// a config change (restart with new limits).
    #[serde(default)]
    pub cgroup: Option<CgroupConfig>,
}

fn default_stop_grace_secs() -> u64 {
    DEFAULT_STOP_GRACE_SECS
}

impl ProcessConfig {
    pub fn stop_grace(&self) -> Duration {
        Duration::from_secs(self.stop_grace_secs)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    Always,
    #[default]
    OnFailure,
    Never,
}

/// How a supervised process finished, as seen by the restart policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitOutcome {
    Success,
    Failure,
}

impl RestartPolicy {
    pub fn should_restart(self, outcome: ExitOutcome) -> bool {
        match self {
            RestartPolicy::Always => true,
            RestartPolicy::Never => false,
            RestartPolicy::OnFailure => outcome == ExitOutcome::Failure,
        }
    }
}

// ---- Health checks (Этап 7) ----
//
// Two architectural decisions here are the user's and are not to be revisited
// (see docs/TECHNICAL_PLAN.md, Этап 7):
//  1. Probes run in a bounded-blocking way, one probe per tick — the same
//     pattern as the Этап 6 control socket. The probe *timeout* is real OS time
//     (SO_RCVTIMEO / connect_timeout / an Instant deadline), deliberately not
//     routed through `Clock`; only the probe *schedule* rides the injected clock.
//  2. The HTTP probe is a hand-rolled minimal HTTP/1.1 GET over `std::net`, with
//     no HTTP crate and no new dependency at all: TCP is `std::net::TcpStream`,
//     exec is the already-used `std::process::Command`.
//
// v1 ships exactly one liveness-style probe per process (threshold failures →
// restart); the startup/readiness/liveness triad is out of scope — see the plan.

fn default_health_interval() -> NonZeroU64 {
    NonZeroU64::new(DEFAULT_HEALTH_INTERVAL_SECS).expect("interval default is non-zero")
}

fn default_health_timeout() -> NonZeroU64 {
    NonZeroU64::new(DEFAULT_HEALTH_TIMEOUT_SECS).expect("timeout default is non-zero")
}

fn default_health_threshold() -> NonZeroU32 {
    NonZeroU32::new(DEFAULT_HEALTH_FAILURE_THRESHOLD).expect("threshold default is non-zero")
}

/// Raw, as-parsed health check section. Cross-field validation (which fields the
/// chosen `type` requires and which it must not carry) happens in
/// [`HealthCheckConfig::probe`], called by [`load`] — a bad section is a config
/// error at load time, never a runtime panic. Manual validation is used instead
/// of `#[serde(flatten)]` + an internally-tagged enum (a known rough edge of
/// serde/toml) so the error messages can name the offending field, consistent
/// with the project's hand-rolled argv parsing. Where a *type* can validate for
/// free it does: `NonZero*` rejects zeros, `IpAddr` rejects hostnames.
///
/// `Clone` + `PartialEq` (Этап 8): a changed `[process.health-check]` section
/// is a config change like any other, so the reload diff compares whole
/// sections by value.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct HealthCheckConfig {
    #[serde(rename = "type")]
    pub kind: ProbeKind,
    /// exec only.
    #[serde(default)]
    pub command: Option<Vec<String>>,
    /// tcp/http; an IP address by design — there is no DNS in v1 (std has no
    /// resolver with a timeout, which would blow the per-probe budget).
    #[serde(default)]
    pub host: Option<IpAddr>,
    /// tcp/http, required there.
    #[serde(default)]
    pub port: Option<NonZeroU16>,
    /// http only; default "/".
    #[serde(default)]
    pub path: Option<String>,
    #[serde(rename = "interval-secs", default = "default_health_interval")]
    pub interval_secs: NonZeroU64,
    #[serde(rename = "timeout-secs", default = "default_health_timeout")]
    pub timeout_secs: NonZeroU64,
    #[serde(rename = "failure-threshold", default = "default_health_threshold")]
    pub failure_threshold: NonZeroU32,
    /// Delay before the probe schedule starts counting, on top of one interval
    /// (see the first-probe formula in `supervise.rs`). Zero is meaningful.
    #[serde(rename = "start-period-secs", default)]
    pub start_period_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeKind {
    Exec,
    Tcp,
    Http,
}

/// A validated, ready-to-run probe. Built from the raw section exactly once at
/// construction; carrying a `SocketAddr` (not host+port) means the runner never
/// re-parses anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthProbe {
    Exec { command: Vec<String> },
    Tcp { addr: SocketAddr },
    Http { addr: SocketAddr, path: String },
}

impl HealthCheckConfig {
    /// Validates the section into a typed probe. Pure; unit-tested directly.
    /// Every violation is an `Err(String)` naming the offending field, so
    /// [`load`] can turn it into a `ConfigError::Invalid` with the process name.
    pub fn probe(&self) -> Result<HealthProbe, String> {
        let default_host = IpAddr::V4(Ipv4Addr::LOCALHOST);
        match self.kind {
            ProbeKind::Exec => {
                if self.host.is_some() {
                    return Err(r#"health-check type "exec" does not take "host""#.to_string());
                }
                if self.port.is_some() {
                    return Err(r#"health-check type "exec" does not take "port""#.to_string());
                }
                if self.path.is_some() {
                    return Err(r#"health-check type "exec" does not take "path""#.to_string());
                }
                match &self.command {
                    Some(cmd) if !cmd.is_empty() => Ok(HealthProbe::Exec {
                        command: cmd.clone(),
                    }),
                    _ => {
                        Err(r#"health-check type "exec" requires a non-empty "command""#
                            .to_string())
                    }
                }
            }
            ProbeKind::Tcp => {
                if self.command.is_some() {
                    return Err(r#"health-check type "tcp" does not take "command""#.to_string());
                }
                if self.path.is_some() {
                    return Err(r#"health-check type "tcp" does not take "path""#.to_string());
                }
                let port = self
                    .port
                    .ok_or_else(|| r#"health-check type "tcp" requires "port""#.to_string())?;
                let host = self.host.unwrap_or(default_host);
                Ok(HealthProbe::Tcp {
                    addr: SocketAddr::new(host, port.get()),
                })
            }
            ProbeKind::Http => {
                if self.command.is_some() {
                    return Err(r#"health-check type "http" does not take "command""#.to_string());
                }
                let port = self
                    .port
                    .ok_or_else(|| r#"health-check type "http" requires "port""#.to_string())?;
                let host = self.host.unwrap_or(default_host);
                let path = self.path.clone().unwrap_or_else(|| "/".to_string());
                if !path.starts_with('/') {
                    return Err(format!(
                        r#"health-check "path" must start with "/", got "{path}""#
                    ));
                }
                Ok(HealthProbe::Http {
                    addr: SocketAddr::new(host, port.get()),
                    path,
                })
            }
        }
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs.get())
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.get())
    }

    pub fn start_period(&self) -> Duration {
        Duration::from_secs(self.start_period_secs)
    }
}

// ---- Log rotation (Этап 9) ----
//
// Capture a process's stdout/stderr into files with size-based rotation, fully
// inside the supervisor. Decision (user's, not to be revisited — see
// docs/TECHNICAL_PLAN.md, Этап 9): rotation is by size only, at write time, with
// no signals, external tools or interval timers; both files are separate; a bad
// section is a config error at load time, never a runtime panic.

fn default_log_max_size() -> NonZeroU64 {
    NonZeroU64::new(DEFAULT_LOG_MAX_SIZE_BYTES).expect("log max-size default is non-zero")
}

fn default_log_keep() -> NonZeroU32 {
    NonZeroU32::new(DEFAULT_LOG_KEEP).expect("log keep default is non-zero")
}

/// Raw, as-parsed `[process.log]` section (Этап 9). Cross-field validation (at
/// least one path, distinct paths) happens in [`LogConfig::validate`], called by
/// [`load`] — a bad section is a config error at load time, never a runtime
/// panic (the `HealthCheckConfig` pattern). `Clone` + `PartialEq` keep the Этап 8
/// reload diff working: a changed log section is a config change like any other
/// and forces a restart.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LogConfig {
    /// Capture the child's stdout into this file. Optional: either stream can
    /// be captured on its own; an uncaptured stream stays inherited. A relative
    /// path is resolved against the daemon's cwd (like `--state-file`).
    #[serde(rename = "stdout-path", default)]
    pub stdout_path: Option<PathBuf>,
    /// Capture the child's stderr into this file. Same optionality and
    /// relative-path resolution as `stdout-path`.
    #[serde(rename = "stderr-path", default)]
    pub stderr_path: Option<PathBuf>,
    /// Rotate once the current file reaches this many bytes. NonZero: a zero
    /// limit would rotate on every write (free type-level validation, the
    /// `NonZero*` precedent of Этап 7).
    #[serde(rename = "max-size-bytes", default = "default_log_max_size")]
    pub max_size_bytes: NonZeroU64,
    /// How many rotated files to keep (`.1` newest … `.keep` oldest). NonZero:
    /// zero would delete output right after rotating it.
    #[serde(default = "default_log_keep")]
    pub keep: NonZeroU32,
}

impl LogConfig {
    /// Validates the section. Pure; unit-tested directly. Every violation is an
    /// `Err(String)` naming the problem, so [`load`] can turn it into a
    /// `ConfigError::Invalid` with the process name (the `HealthCheckConfig::probe`
    /// pattern).
    pub fn validate(&self) -> Result<(), String> {
        // 1) at least one of stdout-path / stderr-path — an empty section is
        //    meaningless and would mask a typo in a key name.
        if self.stdout_path.is_none() && self.stderr_path.is_none() {
            return Err(
                r#"log section requires at least one of "stdout-path" / "stderr-path""#.to_string(),
            );
        }
        // 2) stdout-path != stderr-path — two writer threads on one file would
        //    race the rotation.
        if let (Some(out), Some(err)) = (&self.stdout_path, &self.stderr_path) {
            if out == err {
                return Err(format!(
                    r#"log "stdout-path" and "stderr-path" must differ, both are "{}""#,
                    out.display()
                ));
            }
        }
        Ok(())
    }
}

// ---- Resource limits (Этап 10) ----
//
// Two opt-in per-process limit mechanisms, both the user's decisions and not to
// be revisited (see docs/TECHNICAL_PLAN.md, Этап 10):
//  1. `[process.rlimit]` — per-process setrlimit(2), applied in the existing
//     pre_exec hook (soft = hard = value), inherited by every descendant.
//  2. `[process.cgroup]` — cgroup v2 limits for the whole tree; the supervisor
//     creates `<cgroup-root>/<name>`, writes the limit files and attaches the
//     child right after spawn. A setup failure fails the spawn of this process
//     (no silent "no limits" degradation). The cgroup is NOT used for teardown —
//     killpg (Этап 4) remains the only stop mechanism.
// Two separate sections, not one `[process.limits]`: the mechanisms differ on
// every axis (scope, where applied, environment requirements, failure mode), so
// a merged section would blur "which key goes where" and make the "at least one
// key" validation ambiguous (health and log are separate sections for the same
// reason).

/// Raw, as-parsed `[process.rlimit]` section (Этап 10): per-process resource
/// limits applied via setrlimit(2) in the pre_exec hook, soft = hard = value,
/// inherited by every descendant. `Copy` on purpose: the pre_exec closure
/// captures it by value, so applying limits allocates nothing in the forked
/// child. Without privileges limits can only be lowered: a value above the
/// supervisor's own hard limit fails the spawn with EPERM (a config/environment
/// error made visible, not hidden). `Clone`+`PartialEq` keep the Этап 8 reload
/// diff working.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct RlimitConfig {
    /// RLIMIT_NOFILE: max open file descriptors.
    #[serde(default)]
    pub nofile: Option<NonZeroU64>,
    /// RLIMIT_AS: max virtual address space, bytes.
    #[serde(rename = "as-bytes", default)]
    pub as_bytes: Option<NonZeroU64>,
    /// RLIMIT_CPU: max CPU time, seconds (SIGXCPU at the soft limit).
    #[serde(rename = "cpu-secs", default)]
    pub cpu_secs: Option<NonZeroU64>,
}

impl RlimitConfig {
    /// At least one of the three keys — an empty section is meaningless and
    /// would mask a typo in a key name (the `LogConfig` precedent).
    pub fn validate(&self) -> Result<(), String> {
        if self.nofile.is_none() && self.as_bytes.is_none() && self.cpu_secs.is_none() {
            return Err(
                r#"rlimit section requires at least one of "nofile" / "as-bytes" / "cpu-secs""#
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// Raw, as-parsed `[process.cgroup]` section (Этап 10): cgroup v2 limits for the
/// whole supervised tree. The supervisor creates `<cgroup-root>/<name>`, writes
/// the limit files and attaches the child right after spawn; a setup failure
/// fails the spawn of this process (no silent "no limits" degradation). The
/// cgroup is NOT used for teardown — killpg (Этап 4) remains the only stop
/// mechanism, by the user's decision. `Clone`+`PartialEq` keep the Этап 8 reload
/// diff working.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub struct CgroupConfig {
    /// cpu.max as a percentage of one CPU (50 = half a CPU, 200 = two CPUs), over
    /// a fixed 100 ms period. No upper bound: the kernel accepts any quota.
    #[serde(rename = "cpu-max-percent", default)]
    pub cpu_max_percent: Option<NonZeroU32>,
    /// memory.max, bytes.
    #[serde(rename = "memory-max-bytes", default)]
    pub memory_max_bytes: Option<NonZeroU64>,
}

impl CgroupConfig {
    /// At least one of the two keys, same rationale as `RlimitConfig`.
    pub fn validate(&self) -> Result<(), String> {
        if self.cpu_max_percent.is_none() && self.memory_max_bytes.is_none() {
            return Err(
                r#"cgroup section requires at least one of "cpu-max-percent" / "memory-max-bytes""#
                    .to_string(),
            );
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    /// A syntactically valid config that fails a semantic, cross-field or
    /// cross-process check: a health-check section that fails cross-field
    /// validation (Этап 7), a process name that is not unique (Этап 8, the
    /// by-name reload diff needs a unique key), a log section that names no
    /// path, self-collides, or duplicates another process's log path (Этап 9),
    /// an empty `[process.rlimit]` / `[process.cgroup]` section, or a process
    /// name unusable as a cgroup directory name when it carries a
    /// `[process.cgroup]` section (Этап 10, e.g.
    /// `process name "a/b" is not usable as a cgroup directory name`).
    /// Reported at load time with exit 1, the existing "config error" class —
    /// no new exit code.
    Invalid {
        path: PathBuf,
        /// Which process and what is wrong, e.g.
        /// `process "web": health-check type "tcp" requires "port"`, or
        /// `duplicate log path "/var/log/x.log"`.
        message: String,
    },
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io { path, source } => {
                write!(
                    f,
                    "failed to read config file {}: {}",
                    path.display(),
                    source
                )
            }
            ConfigError::Parse { path, source } => {
                write!(
                    f,
                    "failed to parse config file {}: {}",
                    path.display(),
                    source
                )
            }
            ConfigError::Invalid { path, message } => {
                write!(f, "invalid config file {}: {}", path.display(), message)
            }
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io { source, .. } => Some(source),
            ConfigError::Parse { source, .. } => Some(source),
            ConfigError::Invalid { .. } => None,
        }
    }
}

pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let contents = fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let config = toml::from_str::<Config>(&contents).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    // Process names must be unique: the Этап 8 reload diffs the old and new
    // process lists by name, so a duplicate key would make the diff undefined
    // (and the control socket already addresses only the first bearer of a
    // name). Checked here, at load time, and thus also at first start — a
    // tightening over Этапы 1–7, the deliberate price of a correct diff key.
    let mut seen = std::collections::HashSet::new();
    for proc in &config.process {
        if !seen.insert(proc.name.as_str()) {
            return Err(ConfigError::Invalid {
                path: path.to_path_buf(),
                message: format!("duplicate process name \"{}\"", proc.name),
            });
        }
    }
    // Cross-field validation of each health-check section: a syntactically valid
    // but semantically wrong probe (e.g. tcp without a port) is caught here, at
    // load time, rather than panicking in the runner.
    for proc in &config.process {
        if let Some(hc) = &proc.health_check {
            if let Err(message) = hc.probe() {
                return Err(ConfigError::Invalid {
                    path: path.to_path_buf(),
                    message: format!("process \"{}\": {message}", proc.name),
                });
            }
        }
    }
    // Cross-field validation of each log section (Этап 9): a section with no
    // path or with self-colliding paths is caught here, at load time, with the
    // process name — never a runtime panic.
    for proc in &config.process {
        if let Some(log) = &proc.log {
            if let Err(message) = log.validate() {
                return Err(ConfigError::Invalid {
                    path: path.to_path_buf(),
                    message: format!("process \"{}\": {message}", proc.name),
                });
            }
        }
    }
    // Log paths must be globally unique (within a section and across processes):
    // two writer threads on one file would race the rotation (both rename, both
    // count size). Precedent: unique process names (Этап 8), same error variant.
    let mut seen_log_paths = std::collections::HashSet::new();
    for proc in &config.process {
        if let Some(log) = &proc.log {
            for candidate in [&log.stdout_path, &log.stderr_path].into_iter().flatten() {
                if !seen_log_paths.insert(candidate.as_path()) {
                    return Err(ConfigError::Invalid {
                        path: path.to_path_buf(),
                        message: format!("duplicate log path \"{}\"", candidate.display()),
                    });
                }
            }
        }
    }
    // Cross-field validation of each rlimit / cgroup section (Этап 10): an empty
    // section (no key set) is caught here, at load time, with the process name —
    // never a runtime panic. The `NonZero*` field types already reject zeros at
    // parse time.
    for proc in &config.process {
        if let Some(rl) = &proc.rlimit {
            if let Err(message) = rl.validate() {
                return Err(ConfigError::Invalid {
                    path: path.to_path_buf(),
                    message: format!("process \"{}\": {message}", proc.name),
                });
            }
        }
        if let Some(cg) = &proc.cgroup {
            if let Err(message) = cg.validate() {
                return Err(ConfigError::Invalid {
                    path: path.to_path_buf(),
                    message: format!("process \"{}\": {message}", proc.name),
                });
            }
            // A cgroup section makes `<cgroup-root>/<name>` a real directory
            // path, so the process name must be usable as a single path
            // component. This tightening applies only to processes with a cgroup
            // section (others are untouched). A name with `/` or NUL, or `.` /
            // `..`, or empty, cannot be a directory name.
            let name = proc.name.as_str();
            if name.is_empty()
                || name == "."
                || name == ".."
                || name.contains('/')
                || name.contains('\0')
            {
                return Err(ConfigError::Invalid {
                    path: path.to_path_buf(),
                    message: format!(
                        "process name \"{name}\" is not usable as a cgroup directory name"
                    ),
                });
            }
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    const VALID_TOML: &str = r#"
        [[process]]
        name = "web"
        command = ["/usr/bin/env", "sleep", "3600"]
        restart = "on-failure"

        [[process]]
        name = "worker"
        command = ["/usr/bin/env", "sleep", "3600"]
        restart = "always"
    "#;

    #[test]
    fn parses_valid_config() {
        let config: Config = toml::from_str(VALID_TOML).unwrap();
        assert_eq!(config.process.len(), 2);
        assert_eq!(config.process[0].name, "web");
        assert_eq!(config.process[1].name, "worker");
        assert_eq!(
            config.process[0].command,
            vec!["/usr/bin/env", "sleep", "3600"]
        );
        assert_eq!(config.process[0].restart, RestartPolicy::OnFailure);
        assert_eq!(config.process[1].restart, RestartPolicy::Always);
    }

    #[test]
    fn parses_process_with_workdir_and_env() {
        let toml_src = r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]
            workdir = "/srv/web"
            env = { KEY = "val" }
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        let proc = &config.process[0];
        assert_eq!(proc.workdir.as_deref(), Some(Path::new("/srv/web")));
        let env = proc.env.as_ref().unwrap();
        assert_eq!(env.get("KEY").map(String::as_str), Some("val"));
    }

    #[test]
    fn parses_process_without_optional_fields() {
        let toml_src = r#"
            [[process]]
            name = "bare"
            command = ["/usr/bin/env", "true"]
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        let proc = &config.process[0];
        assert!(proc.workdir.is_none());
        assert!(proc.env.is_none());
        assert_eq!(proc.restart, RestartPolicy::OnFailure);
    }

    #[test]
    fn parses_restart_never() {
        let toml_src = r#"
            [[process]]
            name = "oneshot"
            command = ["/usr/bin/env", "true"]
            restart = "never"
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(config.process[0].restart, RestartPolicy::Never);
    }

    #[test]
    fn stop_grace_defaults_to_five_secs() {
        let toml_src = r#"
            [[process]]
            name = "bare"
            command = ["/usr/bin/env", "true"]
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(config.process[0].stop_grace_secs, DEFAULT_STOP_GRACE_SECS);
        assert_eq!(config.process[0].stop_grace(), Duration::from_secs(5));
    }

    #[test]
    fn parses_stop_grace_secs() {
        let toml_src = r#"
            [[process]]
            name = "slow"
            command = ["/usr/bin/env", "true"]
            stop-grace-secs = 10
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(config.process[0].stop_grace_secs, 10);
        assert_eq!(config.process[0].stop_grace(), Duration::from_secs(10));
    }

    /// A negative grace is rejected for free by the `u64` field type — no
    /// hand-written validation, which is half the reason the field is integer
    /// seconds rather than a float or a duration string.
    #[test]
    fn rejects_negative_stop_grace() {
        let toml_src = r#"
            [[process]]
            name = "bad"
            command = ["/usr/bin/env", "true"]
            stop-grace-secs = -1
        "#;
        assert!(toml::from_str::<Config>(toml_src).is_err());
    }

    /// Pins the kebab-case spelling as the contract. The parser does not deny
    /// unknown keys, so a snake_case misspelling is silently ignored and the
    /// default applies — asserting the default is exactly what detects a future
    /// accidental rename of the accepted key.
    #[test]
    fn rejects_snake_case_stop_grace() {
        let toml_src = r#"
            [[process]]
            name = "typo"
            command = ["/usr/bin/env", "true"]
            stop_grace_secs = 10
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        assert_eq!(config.process[0].stop_grace_secs, DEFAULT_STOP_GRACE_SECS);
    }

    #[test]
    fn rejects_unknown_restart_policy() {
        let toml_src = r#"
            [[process]]
            name = "bad"
            command = ["/usr/bin/env", "true"]
            restart = "sometimes"
        "#;
        let result = toml::from_str::<Config>(toml_src);
        assert!(result.is_err());
    }

    #[test]
    fn should_restart_follows_policy_and_outcome() {
        use ExitOutcome::{Failure, Success};
        assert!(RestartPolicy::Always.should_restart(Success));
        assert!(RestartPolicy::Always.should_restart(Failure));
        assert!(!RestartPolicy::Never.should_restart(Success));
        assert!(!RestartPolicy::Never.should_restart(Failure));
        assert!(!RestartPolicy::OnFailure.should_restart(Success));
        assert!(RestartPolicy::OnFailure.should_restart(Failure));
    }

    #[test]
    fn rejects_malformed_toml() {
        let result = toml::from_str::<Config>("[[process\nname = ");
        assert!(result.is_err());
    }

    #[test]
    fn rejects_missing_required_field() {
        let toml_src = r#"
            [[process]]
            name = "no-command"
        "#;
        let result = toml::from_str::<Config>(toml_src);
        assert!(result.is_err());
    }

    #[test]
    fn load_returns_io_error_for_missing_file() {
        let err = load(Path::new("/nonexistent/xxx.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::Io { .. }));
    }

    #[test]
    fn load_returns_parse_error_for_bad_content() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(b"[[process\nname = ").unwrap();
        let err = load(file.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }));
    }

    #[test]
    fn load_ok_for_valid_file() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(VALID_TOML.as_bytes()).unwrap();
        let config = load(file.path()).unwrap();
        assert_eq!(config.process.len(), 2);
        assert_eq!(config.process[0].name, "web");
    }

    // ---- Health checks (Этап 7) ----

    /// Parses a config with a single `[process.health-check]` subtable and
    /// returns the raw section.
    fn hc_of(toml_src: &str) -> HealthCheckConfig {
        let config: Config = toml::from_str(toml_src).unwrap();
        config
            .process
            .into_iter()
            .next()
            .unwrap()
            .health_check
            .expect("expected a health-check section")
    }

    #[test]
    fn parses_exec_health_check() {
        let hc = hc_of(
            r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.health-check]
            type = "exec"
            command = ["/usr/bin/curl", "-fsS", "http://localhost/health"]
            "#,
        );
        assert_eq!(hc.kind, ProbeKind::Exec);
        assert_eq!(
            hc.probe().unwrap(),
            HealthProbe::Exec {
                command: vec![
                    "/usr/bin/curl".to_string(),
                    "-fsS".to_string(),
                    "http://localhost/health".to_string(),
                ],
            }
        );
    }

    #[test]
    fn parses_tcp_health_check_with_defaults() {
        let hc = hc_of(
            r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.health-check]
            type = "tcp"
            port = 8080
            "#,
        );
        assert_eq!(hc.host, None);
        assert_eq!(hc.interval_secs.get(), DEFAULT_HEALTH_INTERVAL_SECS);
        assert_eq!(hc.timeout_secs.get(), DEFAULT_HEALTH_TIMEOUT_SECS);
        assert_eq!(hc.failure_threshold.get(), DEFAULT_HEALTH_FAILURE_THRESHOLD);
        assert_eq!(hc.start_period_secs, 0);
        assert_eq!(
            hc.probe().unwrap(),
            HealthProbe::Tcp {
                addr: "127.0.0.1:8080".parse().unwrap(),
            }
        );
    }

    #[test]
    fn parses_http_health_check_full() {
        let hc = hc_of(
            r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.health-check]
            type = "http"
            host = "::1"
            port = 9000
            path = "/health"
            interval-secs = 3
            timeout-secs = 2
            failure-threshold = 5
            start-period-secs = 15
            "#,
        );
        assert_eq!(hc.interval(), Duration::from_secs(3));
        assert_eq!(hc.timeout(), Duration::from_secs(2));
        assert_eq!(hc.failure_threshold.get(), 5);
        assert_eq!(hc.start_period(), Duration::from_secs(15));
        assert_eq!(
            hc.probe().unwrap(),
            HealthProbe::Http {
                addr: "[::1]:9000".parse().unwrap(),
                path: "/health".to_string(),
            }
        );
    }

    #[test]
    fn http_path_defaults_to_slash() {
        let hc = hc_of(
            r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.health-check]
            type = "http"
            port = 9000
            "#,
        );
        assert_eq!(
            hc.probe().unwrap(),
            HealthProbe::Http {
                addr: "127.0.0.1:9000".parse().unwrap(),
                path: "/".to_string(),
            }
        );
    }

    #[test]
    fn rejects_tcp_health_check_without_port() {
        let hc = hc_of(
            r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.health-check]
            type = "tcp"
            "#,
        );
        let err = hc.probe().unwrap_err();
        assert!(err.contains("port"), "{err:?}");
    }

    #[test]
    fn rejects_exec_health_check_with_empty_command() {
        // Missing command.
        let hc = hc_of(
            r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.health-check]
            type = "exec"
            "#,
        );
        assert!(hc.probe().is_err());

        // Empty command array.
        let hc = hc_of(
            r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.health-check]
            type = "exec"
            command = []
            "#,
        );
        assert!(hc.probe().is_err());
    }

    #[test]
    fn rejects_inapplicable_probe_field() {
        // `port` on an exec probe.
        let hc = hc_of(
            r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.health-check]
            type = "exec"
            command = ["/usr/bin/env", "true"]
            port = 8080
            "#,
        );
        let err = hc.probe().unwrap_err();
        assert!(err.contains("port"), "{err:?}");

        // `command` on a tcp probe.
        let hc = hc_of(
            r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.health-check]
            type = "tcp"
            port = 8080
            command = ["/usr/bin/env", "true"]
            "#,
        );
        let err = hc.probe().unwrap_err();
        assert!(err.contains("command"), "{err:?}");
    }

    #[test]
    fn rejects_http_path_without_leading_slash() {
        let hc = hc_of(
            r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.health-check]
            type = "http"
            port = 9000
            path = "health"
            "#,
        );
        let err = hc.probe().unwrap_err();
        assert!(err.contains("path"), "{err:?}");
    }

    #[test]
    fn rejects_hostname_in_health_check_host() {
        // `host` is typed `IpAddr`, so a hostname is a *parse* error.
        let toml_src = r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.health-check]
            type = "tcp"
            host = "localhost"
            port = 8080
        "#;
        assert!(toml::from_str::<Config>(toml_src).is_err());
    }

    #[test]
    fn rejects_zero_port_interval_timeout_and_threshold() {
        for field in [
            "port = 0",
            "interval-secs = 0",
            "timeout-secs = 0",
            "failure-threshold = 0",
        ] {
            let toml_src = format!(
                r#"
                [[process]]
                name = "web"
                command = ["/usr/bin/env", "true"]

                [process.health-check]
                type = "tcp"
                port = 8080
                {field}
                "#,
            );
            assert!(
                toml::from_str::<Config>(&toml_src).is_err(),
                "zero must be rejected for: {field}"
            );
        }
    }

    #[test]
    fn rejects_unknown_health_check_type() {
        let toml_src = r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.health-check]
            type = "grpc"
            port = 8080
        "#;
        assert!(toml::from_str::<Config>(toml_src).is_err());
    }

    #[test]
    fn process_without_health_check_parses() {
        let toml_src = r#"
            [[process]]
            name = "bare"
            command = ["/usr/bin/env", "true"]
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        assert!(config.process[0].health_check.is_none());
    }

    #[test]
    fn load_reports_invalid_health_check_with_process_name() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(
            br#"
[[process]]
name = "web"
command = ["/usr/bin/env", "true"]

[process.health-check]
type = "tcp"
"#,
        )
        .unwrap();
        let err = load(file.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }));
        let text = err.to_string();
        assert!(text.contains(r#"process "web""#), "{text}");
        assert!(text.contains("port"), "{text}");
    }

    // ---- Config reload (Этап 8) ----

    #[test]
    fn rejects_duplicate_process_names() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(
            br#"
[[process]]
name = "web"
command = ["/usr/bin/env", "true"]

[[process]]
name = "web"
command = ["/usr/bin/env", "false"]
"#,
        )
        .unwrap();
        let err = load(file.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }));
        let text = err.to_string();
        assert!(text.contains(r#"duplicate process name "web""#), "{text}");
    }

    /// Guards a future hand-written `PartialEq` that "forgets" a field — the
    /// exact silent bug that would make the reload diff miss a change. Every
    /// field of the base config is mutated one at a time and must break
    /// equality.
    #[test]
    fn process_config_equality_notices_every_field() {
        let base = ProcessConfig {
            name: "web".to_string(),
            command: vec!["/usr/bin/env".to_string(), "true".to_string()],
            workdir: None,
            env: None,
            restart: RestartPolicy::OnFailure,
            stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
            health_check: None,
            log: None,
            rlimit: None,
            cgroup: None,
        };
        assert_eq!(base, base.clone());

        let mut command = base.clone();
        command.command = vec!["/usr/bin/env".to_string(), "false".to_string()];
        assert_ne!(base, command);

        let mut env = base.clone();
        env.env = Some(BTreeMap::from([("K".to_string(), "V".to_string())]));
        assert_ne!(base, env);

        let mut workdir = base.clone();
        workdir.workdir = Some(PathBuf::from("/srv"));
        assert_ne!(base, workdir);

        let mut restart = base.clone();
        restart.restart = RestartPolicy::Always;
        assert_ne!(base, restart);

        let mut grace = base.clone();
        grace.stop_grace_secs = base.stop_grace_secs + 1;
        assert_ne!(base, grace);

        let hc: HealthCheckConfig = toml::from_str(
            r#"
            type = "tcp"
            port = 8080
            "#,
        )
        .unwrap();
        let mut health = base.clone();
        health.health_check = Some(hc);
        assert_ne!(base, health);

        // Log section: None -> Some, and Some -> Some with a different
        // max-size-bytes must both break equality (Этап 9).
        let log_a: LogConfig = toml::from_str(
            r#"
            stdout-path = "/var/log/web.out"
            max-size-bytes = 1024
            "#,
        )
        .unwrap();
        let mut with_log = base.clone();
        with_log.log = Some(log_a.clone());
        assert_ne!(base, with_log);

        let log_b: LogConfig = toml::from_str(
            r#"
            stdout-path = "/var/log/web.out"
            max-size-bytes = 2048
            "#,
        )
        .unwrap();
        let mut with_log_b = base.clone();
        with_log_b.log = Some(log_b);
        assert_ne!(with_log, with_log_b);

        // Rlimit section: None -> Some, and Some -> Some with a different
        // `nofile` must both break equality (Этап 10).
        let rl_a: RlimitConfig = toml::from_str("nofile = 1024").unwrap();
        let mut with_rl = base.clone();
        with_rl.rlimit = Some(rl_a);
        assert_ne!(base, with_rl);

        let rl_b: RlimitConfig = toml::from_str("nofile = 2048").unwrap();
        let mut with_rl_b = base.clone();
        with_rl_b.rlimit = Some(rl_b);
        assert_ne!(with_rl, with_rl_b);

        // Cgroup section: None -> Some, and Some -> Some with a different
        // `memory-max-bytes` must both break equality (Этап 10).
        let cg_a: CgroupConfig = toml::from_str("memory-max-bytes = 1048576").unwrap();
        let mut with_cg = base.clone();
        with_cg.cgroup = Some(cg_a);
        assert_ne!(base, with_cg);

        let cg_b: CgroupConfig = toml::from_str("memory-max-bytes = 2097152").unwrap();
        let mut with_cg_b = base.clone();
        with_cg_b.cgroup = Some(cg_b);
        assert_ne!(with_cg, with_cg_b);
    }

    // ---- Log rotation (Этап 9) ----

    #[test]
    fn parses_log_section_full() {
        let toml_src = r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.log]
            stdout-path = "/var/log/web.out"
            stderr-path = "/var/log/web.err"
            max-size-bytes = 1048576
            keep = 3
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        let log = config.process[0].log.as_ref().unwrap();
        assert_eq!(
            log.stdout_path.as_deref(),
            Some(Path::new("/var/log/web.out"))
        );
        assert_eq!(
            log.stderr_path.as_deref(),
            Some(Path::new("/var/log/web.err"))
        );
        assert_eq!(log.max_size_bytes.get(), 1048576);
        assert_eq!(log.keep.get(), 3);
        log.validate().unwrap();
    }

    #[test]
    fn parses_log_with_defaults() {
        let toml_src = r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.log]
            stdout-path = "/var/log/web.out"
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        let log = config.process[0].log.as_ref().unwrap();
        assert_eq!(log.max_size_bytes.get(), DEFAULT_LOG_MAX_SIZE_BYTES);
        assert_eq!(log.max_size_bytes.get(), 10485760);
        assert_eq!(log.keep.get(), DEFAULT_LOG_KEEP);
        assert!(log.stderr_path.is_none());
        log.validate().unwrap();
    }

    #[test]
    fn parses_log_with_only_stderr() {
        let toml_src = r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.log]
            stderr-path = "/var/log/web.err"
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        let log = config.process[0].log.as_ref().unwrap();
        assert!(log.stdout_path.is_none());
        assert_eq!(
            log.stderr_path.as_deref(),
            Some(Path::new("/var/log/web.err"))
        );
        log.validate().unwrap();
    }

    #[test]
    fn rejects_log_without_any_path() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(
            br#"
[[process]]
name = "web"
command = ["/usr/bin/env", "true"]

[process.log]
max-size-bytes = 1024
"#,
        )
        .unwrap();
        let err = load(file.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }));
        let text = err.to_string();
        assert!(text.contains(r#"process "web""#), "{text}");
        assert!(text.contains("stdout-path"), "{text}");
        assert!(text.contains("stderr-path"), "{text}");
    }

    #[test]
    fn rejects_log_with_identical_paths() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(
            br#"
[[process]]
name = "web"
command = ["/usr/bin/env", "true"]

[process.log]
stdout-path = "/var/log/same.log"
stderr-path = "/var/log/same.log"
"#,
        )
        .unwrap();
        let err = load(file.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }));
        let text = err.to_string();
        assert!(text.contains(r#"process "web""#), "{text}");
    }

    #[test]
    fn rejects_duplicate_log_paths_across_processes() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(
            br#"
[[process]]
name = "web"
command = ["/usr/bin/env", "true"]

[process.log]
stdout-path = "/var/log/shared.log"

[[process]]
name = "worker"
command = ["/usr/bin/env", "true"]

[process.log]
stderr-path = "/var/log/shared.log"
"#,
        )
        .unwrap();
        let err = load(file.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }));
        let text = err.to_string();
        assert!(
            text.contains(r#"duplicate log path "/var/log/shared.log""#),
            "{text}"
        );
    }

    #[test]
    fn rejects_zero_log_max_size_and_zero_keep() {
        for field in ["max-size-bytes = 0", "keep = 0"] {
            let toml_src = format!(
                r#"
                [[process]]
                name = "web"
                command = ["/usr/bin/env", "true"]

                [process.log]
                stdout-path = "/var/log/web.out"
                {field}
                "#,
            );
            assert!(
                toml::from_str::<Config>(&toml_src).is_err(),
                "zero must be rejected for: {field}"
            );
        }
    }

    #[test]
    fn process_without_log_parses() {
        let toml_src = r#"
            [[process]]
            name = "bare"
            command = ["/usr/bin/env", "true"]
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        assert!(config.process[0].log.is_none());
    }

    // ---- Resource limits (Этап 10) ----

    #[test]
    fn parses_rlimit_section_full() {
        let toml_src = r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.rlimit]
            nofile = 1024
            as-bytes = 536870912
            cpu-secs = 300
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        let rl = config.process[0].rlimit.as_ref().unwrap();
        assert_eq!(rl.nofile.unwrap().get(), 1024);
        assert_eq!(rl.as_bytes.unwrap().get(), 536870912);
        assert_eq!(rl.cpu_secs.unwrap().get(), 300);
        rl.validate().unwrap();
    }

    #[test]
    fn parses_rlimit_with_single_key() {
        let toml_src = r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.rlimit]
            nofile = 256
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        let rl = config.process[0].rlimit.as_ref().unwrap();
        assert_eq!(rl.nofile.unwrap().get(), 256);
        assert!(rl.as_bytes.is_none());
        assert!(rl.cpu_secs.is_none());
        rl.validate().unwrap();
    }

    #[test]
    fn parses_cgroup_section_full() {
        let toml_src = r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.cgroup]
            cpu-max-percent = 50
            memory-max-bytes = 268435456
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        let cg = config.process[0].cgroup.as_ref().unwrap();
        assert_eq!(cg.cpu_max_percent.unwrap().get(), 50);
        assert_eq!(cg.memory_max_bytes.unwrap().get(), 268435456);
        cg.validate().unwrap();
    }

    #[test]
    fn parses_cgroup_with_only_memory() {
        let toml_src = r#"
            [[process]]
            name = "web"
            command = ["/usr/bin/env", "true"]

            [process.cgroup]
            memory-max-bytes = 1048576
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        let cg = config.process[0].cgroup.as_ref().unwrap();
        assert!(cg.cpu_max_percent.is_none());
        assert_eq!(cg.memory_max_bytes.unwrap().get(), 1048576);
        cg.validate().unwrap();
    }

    #[test]
    fn rejects_empty_rlimit_section() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(
            br#"
[[process]]
name = "web"
command = ["/usr/bin/env", "true"]

[process.rlimit]
"#,
        )
        .unwrap();
        let err = load(file.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }));
        let text = err.to_string();
        assert!(text.contains(r#"process "web""#), "{text}");
        assert!(text.contains("nofile"), "{text}");
        assert!(text.contains("as-bytes"), "{text}");
        assert!(text.contains("cpu-secs"), "{text}");
    }

    #[test]
    fn rejects_empty_cgroup_section() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(
            br#"
[[process]]
name = "web"
command = ["/usr/bin/env", "true"]

[process.cgroup]
"#,
        )
        .unwrap();
        let err = load(file.path()).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }));
        let text = err.to_string();
        assert!(text.contains(r#"process "web""#), "{text}");
        assert!(text.contains("cpu-max-percent"), "{text}");
        assert!(text.contains("memory-max-bytes"), "{text}");
    }

    #[test]
    fn rejects_zero_limit_values() {
        for field in [
            ("rlimit", "nofile = 0"),
            ("rlimit", "as-bytes = 0"),
            ("rlimit", "cpu-secs = 0"),
            ("cgroup", "cpu-max-percent = 0"),
            ("cgroup", "memory-max-bytes = 0"),
        ] {
            let (section, kv) = field;
            let toml_src = format!(
                r#"
                [[process]]
                name = "web"
                command = ["/usr/bin/env", "true"]

                [process.{section}]
                {kv}
                "#,
            );
            assert!(
                toml::from_str::<Config>(&toml_src).is_err(),
                "zero must be rejected for: {kv}"
            );
        }
    }

    #[test]
    fn rejects_snake_case_limit_keys() {
        // snake_case keys are silently ignored by the parser, so the section
        // becomes empty and `load()` rejects it — pins the kebab-case contract.
        for (section, kv) in [
            ("rlimit", "as_bytes = 1024"),
            ("cgroup", "cpu_max_percent = 50"),
        ] {
            let mut file = NamedTempFile::new().unwrap();
            let body = format!(
                "\n[[process]]\nname = \"web\"\ncommand = [\"/usr/bin/env\", \"true\"]\n\n[process.{section}]\n{kv}\n"
            );
            file.write_all(body.as_bytes()).unwrap();
            let err = load(file.path()).unwrap_err();
            assert!(
                matches!(err, ConfigError::Invalid { .. }),
                "snake_case key must leave the section empty and be rejected: {kv}"
            );
        }
    }

    #[test]
    fn rejects_cgroup_process_name_unfit_for_directory() {
        for bad in ["a/b", ".."] {
            let toml_src = format!(
                r#"
                [[process]]
                name = "{bad}"
                command = ["/usr/bin/env", "true"]

                [process.cgroup]
                memory-max-bytes = 1048576
                "#,
            );
            let mut file = NamedTempFile::new().unwrap();
            file.write_all(toml_src.as_bytes()).unwrap();
            let err = load(file.path()).unwrap_err();
            assert!(
                matches!(err, ConfigError::Invalid { .. }),
                "name {bad:?} with a cgroup section must be rejected"
            );
            let text = err.to_string();
            assert!(text.contains("cgroup directory name"), "{text}");

            // The same name WITHOUT a cgroup section is fine (tightening is
            // pointed, not global).
            let ok_src = format!(
                r#"
                [[process]]
                name = "{bad}"
                command = ["/usr/bin/env", "true"]
                "#,
            );
            let mut ok_file = NamedTempFile::new().unwrap();
            ok_file.write_all(ok_src.as_bytes()).unwrap();
            assert!(
                load(ok_file.path()).is_ok(),
                "name {bad:?} without a cgroup section must be accepted"
            );
        }
    }

    #[test]
    fn process_without_limit_sections_parses() {
        let toml_src = r#"
            [[process]]
            name = "bare"
            command = ["/usr/bin/env", "true"]
        "#;
        let config: Config = toml::from_str(toml_src).unwrap();
        assert!(config.process[0].rlimit.is_none());
        assert!(config.process[0].cgroup.is_none());
    }
}
