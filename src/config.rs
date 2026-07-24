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
    // TODO(Этап 2): replace with a `RestartPolicy` enum (always | on-failure |
    // never) without breaking the TOML format.
    #[serde(default)]
    pub restart: String,
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
        assert_eq!(config.process[0].restart, "on-failure");
        assert_eq!(config.process[1].restart, "always");
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
        assert_eq!(proc.restart, "");
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
