//! Process spawning for supervisor-rs.
//!
//! Этап 1: plain spawn from a `ProcessConfig` (argv, optional workdir, env
//! additions), no restart or teardown yet. Later stages add the monitor loop
//! (Этап 2), signal forwarding (Этап 3) and process-group teardown (Этап 4).

use crate::config::ProcessConfig;
use std::process::{Child, Command};

#[derive(Debug)]
pub enum SpawnError {
    EmptyCommand {
        name: String,
    },
    Spawn {
        name: String,
        source: std::io::Error,
    },
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::EmptyCommand { name } => {
                write!(f, "process '{name}': command is empty")
            }
            SpawnError::Spawn { name, source } => {
                write!(f, "process '{name}': failed to spawn: {source}")
            }
        }
    }
}

impl std::error::Error for SpawnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SpawnError::EmptyCommand { .. } => None,
            SpawnError::Spawn { source, .. } => Some(source),
        }
    }
}

pub fn spawn(cfg: &ProcessConfig) -> Result<Child, SpawnError> {
    if cfg.command.is_empty() {
        return Err(SpawnError::EmptyCommand {
            name: cfg.name.clone(),
        });
    }

    let mut cmd = Command::new(&cfg.command[0]);
    cmd.args(&cfg.command[1..]);
    if let Some(dir) = &cfg.workdir {
        cmd.current_dir(dir);
    }
    if let Some(map) = &cfg.env {
        // Adds to the inherited environment; deliberately no env_clear.
        cmd.envs(map);
    }

    // TODO(Этап 4): put child in its own process group via pre_exec(setsid) — see SKILL.md
    cmd.spawn().map_err(|source| SpawnError::Spawn {
        name: cfg.name.clone(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_rejects_empty_command() {
        let cfg = ProcessConfig {
            name: "empty".to_string(),
            command: Vec::new(),
            workdir: None,
            env: None,
            restart: String::new(),
        };
        let err = spawn(&cfg).unwrap_err();
        assert!(matches!(err, SpawnError::EmptyCommand { .. }));
    }
}
