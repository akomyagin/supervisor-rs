//! Command-line parsing for `supervisor-rs` (Этап 5).
//!
//! Two subcommands: `run <config>` starts the daemon, `status` prints what a
//! running daemon publishes in its state file.
//!
//! The parsing lives in the library crate rather than in `main.rs` so it can be
//! unit-tested as a pure function, leaving `main.rs` a thin shell around IO and
//! exit codes.
//!
//! No CLI crate, by the user's decision: two subcommands and one flag do not
//! justify a dependency, and the whole grammar fits in the function below.
//!
//! The pre-Этап-5 form — a single positional argument taken as the config path
//! — is **gone**, deliberately and without a compatibility alias (also the
//! user's decision). It now fails as an unknown subcommand, which
//! `rejects_bare_config_path_without_subcommand` pins as the contract.

use std::path::PathBuf;

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Run {
        config: PathBuf,
        state_file: Option<PathBuf>,
    },
    Status {
        state_file: Option<PathBuf>,
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

/// Parses the raw arguments, `argv[0]` already stripped.
pub fn parse(args: &[String]) -> Result<Command, UsageError> {
    // Help wins over everything, in any position: someone who asks for help
    // after mistyping a command wants the help, not the complaint.
    if args.iter().any(|arg| arg == "-h" || arg == "--help") {
        return Ok(Command::Help);
    }

    let mut positional: Vec<&str> = Vec::new();
    let mut state_file: Option<PathBuf> = None;
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
            flag if flag.starts_with("--") => {
                return Err(err(format!("unknown option '{flag}'")));
            }
            value => positional.push(value),
        }
    }

    let Some((subcommand, operands)) = positional.split_first() else {
        return Err(err("no subcommand given (expected 'run' or 'status')"));
    };

    match *subcommand {
        "run" => match operands {
            [config] => Ok(Command::Run {
                config: PathBuf::from(config),
                state_file,
            }),
            [] => Err(err("'run' requires a <config-path>")),
            _ => Err(err(format!(
                "'run' takes exactly one <config-path>, got {}",
                operands.len()
            ))),
        },
        "status" => match operands {
            [] => Ok(Command::Status { state_file }),
            [extra, ..] => Err(err(format!(
                "'status' takes no positional arguments, got '{extra}'"
            ))),
        },
        other => {
            // A path as the first argument used to *be* the command line, so
            // say what changed instead of only that the word is unknown.
            let hint = if other.contains('/') || other.ends_with(".toml") {
                " (a bare config path was the pre-Этап-5 form; use 'run <config-path>')"
            } else {
                ""
            };
            Err(err(format!(
                "unknown subcommand '{other}', expected 'run' or 'status'{hint}"
            )))
        }
    }
}

/// The help text: printed to stdout for `--help`, appended to stderr after a
/// usage error.
pub fn usage() -> &'static str {
    "\
supervisor-rs — a minimal process supervisor (mini-systemd) for Unix

Usage:
  supervisor-rs run <config-path> [--state-file <path>]
  supervisor-rs status [--state-file <path>]

Subcommands:
  run      Start the supervisor daemon with the given TOML config.
  status   Show the state of the supervised processes of a running daemon.

Options:
  --state-file <path>  Path of the state snapshot the daemon writes and
                       status reads. Default: $XDG_RUNTIME_DIR/supervisor-rs/
                       state.toml, or /tmp/supervisor-rs-<uid>/state.toml when
                       XDG_RUNTIME_DIR is not set.
  -h, --help           Print this help.
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
