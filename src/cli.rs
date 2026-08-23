//! Command-line parsing for `supervisor-rs` (Этап 5, extended in Этап 6).
//!
//! Subcommands: `run <config>` starts the daemon, `status` prints what a
//! running daemon publishes in its state file, and `start`/`stop`/`restart
//! <name>` are thin clients of the control socket the daemon listens on.
//!
//! The parsing lives in the library crate rather than in `main.rs` so it can be
//! unit-tested as a pure function, leaving `main.rs` a thin shell around IO and
//! exit codes.
//!
//! No CLI crate, by the user's decision: the whole grammar fits in the function
//! below and does not justify a dependency.
//!
//! The pre-Этап-5 form — a single positional argument taken as the config path
//! — is **gone**, deliberately and without a compatibility alias (also the
//! user's decision). It now fails as an unknown subcommand, which
//! `rejects_bare_config_path_without_subcommand` pins as the contract.

use crate::control::Request;
use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run {
        config: PathBuf,
        state_file: Option<PathBuf>,
        control_socket: Option<PathBuf>,
        /// Root of the per-process cgroup subtree (Этап 10). `None` ⇒ main.rs
        /// resolves the default `limits::DEFAULT_CGROUP_ROOT`. Applicable only to
        /// `run`, by the `--state-file` precedent.
        cgroup_root: Option<PathBuf>,
    },
    Status {
        state_file: Option<PathBuf>,
    },
    /// `start`/`stop`/`restart <name>`: a thin client of the control socket.
    /// One variant instead of three zeroed-out siblings — `main.rs` handles
    /// them identically, and the verb is already carried by `Request`.
    Control {
        request: Request,
        control_socket: Option<PathBuf>,
    },
    Help,
}

/// A rejected command line, carrying the message to show the user. `main`
/// prints it followed by [`usage`] and exits with code 2.
#[derive(Debug, PartialEq, Eq)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

fn err(message: impl Into<String>) -> UsageError {
    UsageError(message.into())
}

/// Rejects a name that the line protocol cannot carry: a bare `\n`/`\r` would be
/// indistinguishable from the request terminator, and an empty name is not
/// addressable at all. Ordinary spaces are fine — the protocol keeps them
/// (`split_once(' ')`, not `split_whitespace`).
fn validate_process_name(name: &str) -> Result<(), UsageError> {
    if name.is_empty() || name.contains('\n') || name.contains('\r') {
        return Err(err("process name must be a single line"));
    }
    Ok(())
}

/// Parses the raw arguments, `argv[0]` already stripped.
pub fn parse(args: &[String]) -> Result<Command, UsageError> {
    // Help wins over everything, in any position: someone who asks for help
    // after mistyping a command wants the help, not the complaint.
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Command::Help);
    }

    let mut positional: Vec<&str> = Vec::new();
    let mut state_file: Option<PathBuf> = None;
    let mut control_socket: Option<PathBuf> = None;
    let mut cgroup_root: Option<PathBuf> = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--state-file" => {
                let value = rest
                    .next()
                    .ok_or_else(|| err("--state-file requires a path"))?;
                // Last one wins on repetition — not worth an error.
                state_file = Some(PathBuf::from(value));
            }
            "--control-socket" => {
                let value = rest
                    .next()
                    .ok_or_else(|| err("--control-socket requires a path"))?;
                control_socket = Some(PathBuf::from(value));
            }
            "--cgroup-root" => {
                let value = rest
                    .next()
                    .ok_or_else(|| err("--cgroup-root requires a path"))?;
                // Last one wins on repetition — the `--state-file` precedent.
                cgroup_root = Some(PathBuf::from(value));
            }
            flag if flag.starts_with("--") => {
                return Err(err(format!("unknown option '{flag}'")));
            }
            value => positional.push(value),
        }
    }

    let Some((subcommand, operands)) = positional.split_first() else {
        return Err(err(
            "no subcommand given (expected 'run', 'status', 'start', 'stop' or 'restart')",
        ));
    };

    // Flags are parsed before dispatch, so applicability to the subcommand is
    // checked here, after the match below picks the command out — a
    // `--state-file` on `stop` or a `--control-socket` on `status` must not
    // silently do nothing.
    let command = match *subcommand {
        "run" => match operands {
            [config] => Ok(Command::Run {
                config: PathBuf::from(config),
                state_file,
                control_socket,
                cgroup_root,
            }),
            [] => Err(err("'run' requires a <config-path>")),
            _ => Err(err(format!(
                "'run' takes exactly one <config-path>, got {}",
                operands.len()
            ))),
        },
        "status" => match operands {
            [] => {
                if control_socket.is_some() {
                    return Err(err("'status' does not take --control-socket"));
                }
                if cgroup_root.is_some() {
                    return Err(err("'status' does not take --cgroup-root"));
                }
                Ok(Command::Status { state_file })
            }
            [extra, ..] => Err(err(format!(
                "'status' takes no positional arguments, got '{extra}'"
            ))),
        },
        verb @ ("start" | "stop" | "restart") => {
            if state_file.is_some() {
                return Err(err(format!("'{verb}' does not take --state-file")));
            }
            if cgroup_root.is_some() {
                return Err(err(format!("'{verb}' does not take --cgroup-root")));
            }
            match operands {
                [name] => {
                    validate_process_name(name)?;
                    let request = match verb {
                        "start" => Request::Start(name.to_string()),
                        "stop" => Request::Stop(name.to_string()),
                        "restart" => Request::Restart(name.to_string()),
                        _ => unreachable!(),
                    };
                    Ok(Command::Control {
                        request,
                        control_socket,
                    })
                }
                [] => Err(err(format!("'{verb}' requires a <name>"))),
                _ => Err(err(format!(
                    "'{verb}' takes exactly one <name>, got {}",
                    operands.len()
                ))),
            }
        }
        other => {
            // A path as the first argument used to *be* the command line, so
            // say what changed instead of only that the word is unknown.
            let hint = if other.contains('/') || other.ends_with(".toml") {
                " (a bare config path was the pre-Этап-5 form; use 'run <config-path>')"
            } else {
                ""
            };
            Err(err(format!(
                "unknown subcommand '{other}', expected 'run', 'status', 'start', 'stop' or 'restart'{hint}"
            )))
        }
    }?;
    Ok(command)
}

/// The help text: printed to stdout for `--help`, appended to stderr after a
/// usage error.
pub fn usage() -> &'static str {
    "\
supervisor-rs — a minimal process supervisor (mini-systemd) for Unix

Usage:
  supervisor-rs run <config-path> [--state-file <path>] [--control-socket <path>] [--cgroup-root <path>]
  supervisor-rs status [--state-file <path>]
  supervisor-rs start <name> [--control-socket <path>]
  supervisor-rs stop <name> [--control-socket <path>]
  supervisor-rs restart <name> [--control-socket <path>]

Subcommands:
  run      Start the supervisor daemon with the given TOML config.
  status   Show the state of the supervised processes of a running daemon.
  start    Start a process previously stopped with 'stop'.
  stop     Stop a process and keep it stopped (its restart policy is suspended).
  restart  Stop a running process and start it again.

Options:
  --state-file <path>      Path of the state snapshot the daemon writes and
                           status reads. Default: $XDG_RUNTIME_DIR/supervisor-rs/
                           state.toml, or /tmp/supervisor-rs-<uid>/state.toml
                           when XDG_RUNTIME_DIR is not set.
  --control-socket <path>  Path of the control socket the daemon listens on and
                           start/stop/restart connect to. Default:
                           $XDG_RUNTIME_DIR/supervisor-rs/control.sock, or
                           /tmp/supervisor-rs-<uid>/control.sock when
                           XDG_RUNTIME_DIR is not set.
  --cgroup-root <path>     Root of the per-process cgroup v2 subtree used for
                           [process.cgroup] limits (run only). Requires a
                           delegated, writable cgroup v2 subtree. Default:
                           /sys/fs/cgroup/supervisor-rs.
  -h, --help               Print this help.
"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parses_run_with_config() {
        assert_eq!(
            parse(&args(&["run", "/etc/sup.toml"])).unwrap(),
            Command::Run {
                config: PathBuf::from("/etc/sup.toml"),
                state_file: None,
                control_socket: None,
                cgroup_root: None,
            }
        );
    }

    #[test]
    fn parses_run_with_state_file() {
        assert_eq!(
            parse(&args(&[
                "run",
                "/etc/sup.toml",
                "--state-file",
                "/tmp/s.toml"
            ]))
            .unwrap(),
            Command::Run {
                config: PathBuf::from("/etc/sup.toml"),
                state_file: Some(PathBuf::from("/tmp/s.toml")),
                control_socket: None,
                cgroup_root: None,
            }
        );
    }

    #[test]
    fn parses_run_with_control_socket() {
        assert_eq!(
            parse(&args(&[
                "run",
                "/etc/sup.toml",
                "--control-socket",
                "/tmp/c.sock"
            ]))
            .unwrap(),
            Command::Run {
                config: PathBuf::from("/etc/sup.toml"),
                state_file: None,
                control_socket: Some(PathBuf::from("/tmp/c.sock")),
                cgroup_root: None,
            }
        );
    }

    #[test]
    fn parses_status_without_flags() {
        assert_eq!(
            parse(&args(&["status"])).unwrap(),
            Command::Status { state_file: None }
        );
    }

    #[test]
    fn parses_status_with_state_file() {
        assert_eq!(
            parse(&args(&["status", "--state-file", "/tmp/s.toml"])).unwrap(),
            Command::Status {
                state_file: Some(PathBuf::from("/tmp/s.toml")),
            }
        );
    }

    #[test]
    fn parses_stop_with_name() {
        assert_eq!(
            parse(&args(&["stop", "web"])).unwrap(),
            Command::Control {
                request: Request::Stop("web".to_string()),
                control_socket: None,
            }
        );
    }

    #[test]
    fn parses_start_with_name() {
        assert_eq!(
            parse(&args(&["start", "web"])).unwrap(),
            Command::Control {
                request: Request::Start("web".to_string()),
                control_socket: None,
            }
        );
    }

    #[test]
    fn parses_restart_with_name() {
        assert_eq!(
            parse(&args(&["restart", "web"])).unwrap(),
            Command::Control {
                request: Request::Restart("web".to_string()),
                control_socket: None,
            }
        );
    }

    #[test]
    fn parses_stop_with_control_socket() {
        assert_eq!(
            parse(&args(&["stop", "web", "--control-socket", "/tmp/c.sock"])).unwrap(),
            Command::Control {
                request: Request::Stop("web".to_string()),
                control_socket: Some(PathBuf::from("/tmp/c.sock")),
            }
        );
    }

    #[test]
    fn rejects_stop_without_name() {
        assert!(parse(&args(&["stop"])).is_err());
    }

    #[test]
    fn rejects_stop_with_two_names() {
        assert!(parse(&args(&["stop", "web", "worker"])).is_err());
    }

    #[test]
    fn rejects_control_socket_without_value() {
        assert!(parse(&args(&["stop", "web", "--control-socket"])).is_err());
    }

    #[test]
    fn rejects_multiline_process_name() {
        let err = parse(&args(&["stop", "a\nb"])).unwrap_err();
        assert!(err.0.contains("single line"), "{err}");
    }

    #[test]
    fn rejects_state_file_on_stop() {
        let err = parse(&args(&["stop", "web", "--state-file", "/tmp/s.toml"])).unwrap_err();
        assert!(err.0.contains("--state-file"), "{err}");
    }

    #[test]
    fn rejects_control_socket_on_status() {
        let err = parse(&args(&["status", "--control-socket", "/tmp/c.sock"])).unwrap_err();
        assert!(err.0.contains("--control-socket"), "{err}");
    }

    #[test]
    fn help_flag_wins_anywhere() {
        assert_eq!(parse(&args(&["--help"])).unwrap(), Command::Help);
        assert_eq!(parse(&args(&["-h"])).unwrap(), Command::Help);
        assert_eq!(
            parse(&args(&["run", "cfg", "--help"])).unwrap(),
            Command::Help
        );
    }

    #[test]
    fn rejects_empty_args() {
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn rejects_unknown_subcommand() {
        assert!(parse(&args(&["reload"])).is_err());
    }

    /// The removal of the old CLI form, as a contract: a path where the
    /// subcommand belongs is a usage error, not an alias for `run`.
    #[test]
    fn rejects_bare_config_path_without_subcommand() {
        let err = parse(&args(&["/etc/supervisor.toml"])).unwrap_err();
        assert!(err.0.contains("run <config-path>"), "{err}");
    }

    #[test]
    fn rejects_run_without_config() {
        assert!(parse(&args(&["run"])).is_err());
        assert!(parse(&args(&["run", "--state-file", "/tmp/s.toml"])).is_err());
    }

    #[test]
    fn rejects_run_with_two_configs() {
        assert!(parse(&args(&["run", "a.toml", "b.toml"])).is_err());
    }

    #[test]
    fn rejects_status_with_positional() {
        assert!(parse(&args(&["status", "web"])).is_err());
    }

    #[test]
    fn rejects_state_file_without_value() {
        assert!(parse(&args(&["status", "--state-file"])).is_err());
        assert!(parse(&args(&["run", "cfg.toml", "--state-file"])).is_err());
    }

    #[test]
    fn rejects_unknown_flag() {
        assert!(parse(&args(&["status", "--json"])).is_err());
    }

    /// Not a rule anyone should rely on, but it must not be an error either —
    /// pinned so the "last one wins" comment stays true.
    #[test]
    fn repeated_state_file_takes_the_last_value() {
        assert_eq!(
            parse(&args(&["status", "--state-file", "a", "--state-file", "b"])).unwrap(),
            Command::Status {
                state_file: Some(PathBuf::from("b")),
            }
        );
    }
}
