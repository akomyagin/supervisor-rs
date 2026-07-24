//! supervisor-rs — a minimal process supervisor (mini-systemd) for Unix.
//!
//! Этап 1: loads a TOML config given as the single CLI argument, spawns each
//! configured process and waits for them to exit. Restart policy, signal
//! handling and process-group teardown land in later stages — see
//! `docs/TECHNICAL_PLAN.md` for the per-stage breakdown.

use std::path::Path;
use std::process::ExitCode;

use supervisor_rs::{config, process};

fn main() -> ExitCode {
    // Structured logging is initialised once, here at the top of the process.
    // RUST_LOG controls verbosity (e.g. RUST_LOG=debug); defaults to info.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "supervisor-rs starting"
    );

    // TODO(Этап 2): apply restart policy (always / on-failure / never) + backoff.
    // TODO(Этап 3): install signal handlers and forward SIGTERM/SIGINT to children.
    // TODO(Этап 4): put each child in its own process group and tear the whole
    //               tree down with SIGTERM → timeout → SIGKILL.
    // TODO(Этап 5): expose a status/control CLI subcommand.

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 1 {
        eprintln!("Usage: supervisor-rs <config-path>");
        tracing::error!("expected exactly one argument: <config-path>");
        return ExitCode::from(2);
    }

    let config = match config::load(Path::new(&args[0])) {
        Ok(config) => config,
        Err(err) => {
            tracing::error!(error = %err, "failed to load config");
            return ExitCode::FAILURE;
        }
    };

    let mut children: Vec<(String, std::process::Child)> = Vec::new();
    let mut had_error = false;
    for cfg in &config.process {
        match process::spawn(cfg) {
            Ok(child) => {
                tracing::info!(name = %cfg.name, pid = child.id(), "process spawned");
                children.push((cfg.name.clone(), child));
            }
            Err(err) => {
                tracing::error!(name = %cfg.name, error = %err, "failed to spawn process");
                had_error = true;
            }
        }
    }

    for (name, mut child) in children {
        match child.wait() {
            Ok(status) => {
                tracing::info!(name = %name, status = ?status, "process exited");
            }
            Err(err) => {
                tracing::error!(name = %name, error = %err, "failed to wait for process");
                had_error = true;
            }
        }
    }

    // A spawn/wait failure means not everything ran as configured — reflect
    // that in the exit code rather than reporting a misleading SUCCESS.
    // Partially-started processes are not torn down here (Этап 4).
    if had_error {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
