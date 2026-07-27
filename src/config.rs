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
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Default grace period between the shutdown signal and SIGKILL, seconds.
pub const DEFAULT_STOP_GRACE_SECS: u64 = 5;

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
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io { source, .. } => Some(source),
            ConfigError::Parse { source, .. } => Some(source),
        }
    }
}

pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let contents = fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str::<Config>(&contents).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
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
}
