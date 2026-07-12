//! Config parsing for supervisor-rs.
//!
//! The config is a TOML file describing the set of processes to supervise and,
//! for each, its restart policy. This module is a skeleton in Этап 0 — the
//! types and `load` function are fleshed out in Этап 1 (parsing) and Этап 2
//! (restart policy + backoff).
//!
//! Intended shape (subject to change during Этап 1):
//!
//! ```toml
//! [[process]]
//! name = "web"
//! command = ["/usr/bin/myserver", "--port", "8080"]
//! restart = "on-failure"   # always | on-failure | never
//! ```

// TODO(Этап 1): define `Config` (top-level, holds `Vec<ProcessConfig>`) and
//               `ProcessConfig` (name, command argv, working dir, env, restart)
//               as `#[derive(Debug, serde::Deserialize)]` structs.
// TODO(Этап 2): define a `RestartPolicy` enum { Always, OnFailure, Never } with
//               a serde rename to the kebab-case config values, plus backoff
//               parameters (initial delay, max delay, factor).
// TODO(Этап 1): implement `pub fn load(path: &Path) -> Result<Config, ConfigError>`
//               that reads the file and parses it with `toml::from_str`, with a
//               dedicated `ConfigError` type for IO vs parse failures.
