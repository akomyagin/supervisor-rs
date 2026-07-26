//! supervisor-rs — a minimal process supervisor (mini-systemd) for Unix.
//!
//! Этап 2: loads a TOML config given as the single CLI argument, spawns each
//! configured process and supervises them — applying the per-process restart
//! policy (always / on-failure / never) with exponential backoff. Signal
//! handling and process-group teardown land in later stages — see
//! `docs/TECHNICAL_PLAN.md` for the per-stage breakdown.

use std::path::Path;
use std::process::ExitCode;

use supervisor_rs::clock::SystemClock;
use supervisor_rs::config;
use supervisor_rs::supervise::SupervisorLoop;

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

    // A spawn failure means not everything ran as configured — reflect that in
    // the exit code rather than reporting a misleading SUCCESS, even though
    // processes that did spawn are still supervised below.
    let mut loop_ = SupervisorLoop::new(&config.process, SystemClock);
    let had_start_errors = loop_.had_start_errors();
    loop_.run();

    if had_start_errors {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
