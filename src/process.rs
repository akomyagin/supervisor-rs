//! Process spawning for supervisor-rs.
//!
//! Spawn (Этап 1) and signal forwarding (Этап 3) for supervised processes.
//! Process-group teardown lands in Этап 4.

use crate::config::ProcessConfig;
use nix::errno::Errno;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
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
    // TODO(Этап 4): switch reaping to nix::waitpid(WNOHANG) instead of try_wait.
    cmd.spawn().map_err(|source| SpawnError::Spawn {
        name: cfg.name.clone(),
        source,
    })
}

/// Forwards `sig` to the child process.
///
/// Call only for a `Child` that has not been reaped yet: after reaping, the
/// OS may reuse the pid for an unrelated process and the signal would hit it.
// TODO(Этап 4): switch to killpg(pgid, sig) once children get their own process group.
pub fn forward_signal(child: &Child, sig: Signal) -> Result<(), Errno> {
    match kill(Pid::from_raw(child.id() as i32), sig) {
        Err(Errno::ESRCH) => {
            // The child already died but has not been reaped yet — nothing to
            // deliver to, which is fine during shutdown.
            tracing::debug!(pid = child.id(), signal = ?sig, "signal target already gone");
            Ok(())
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RestartPolicy;

    #[test]
    fn spawn_rejects_empty_command() {
        let cfg = ProcessConfig {
            name: "empty".to_string(),
            command: Vec::new(),
            workdir: None,
            env: None,
            restart: RestartPolicy::default(),
        };
        let err = spawn(&cfg).unwrap_err();
        assert!(matches!(err, SpawnError::EmptyCommand { .. }));
    }
}
