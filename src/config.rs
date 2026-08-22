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

#[derive(Debug, Deserialize)]
pub struct Config {
    pub process: Vec<ProcessConfig>,
}

#[derive(Debug, Deserialize)]
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
    /// subtable, kept last in the struct so the "scalars before subtables"
    /// TOML rule holds (same convention as `StateSnapshot`), even though
    /// `ProcessConfig` only derives `Deserialize`.
    #[serde(rename = "health-check", default)]
    pub health_check: Option<HealthCheckConfig>,
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
#[derive(Debug, Deserialize)]
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
    /// A syntactically valid config whose health-check section fails
    /// cross-field validation (Этап 7). Reported at load time with exit 1, the
    /// existing "config error" class — no new exit code.
    Invalid {
        path: PathBuf,
        /// Which process and what is wrong, e.g.
        /// `process "web": health-check type "tcp" requires "port"`.
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
}
