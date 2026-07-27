//! supervisor-rs — a minimal process supervisor (mini-systemd) for Unix.
//!
//! Этап 4: loads a TOML config given as the single CLI argument, spawns each
//! configured process as the leader of its own process group and supervises
//! them — applying the per-process restart policy (always / on-failure /
//! never) with exponential backoff — until a SIGTERM/SIGINT arrives. That
//! signal is forwarded to each process *group*, so the whole tree a child
//! forked goes down with it; a group that outlives its `stop-grace-secs` is
//! SIGKILLed, as is one whose leader exits leaving stragglers behind. See
//! `docs/TECHNICAL_PLAN.md` for the per-stage breakdown.

use std::path::Path;
use std::process::ExitCode;

use supervisor_rs::clock::SystemClock;
use supervisor_rs::config;
use supervisor_rs::signal;
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

    // Handlers go up *before* the first child is spawned: a signal arriving in
    // the window between a successful spawn and handler installation would
    // kill the supervisor by the default disposition and leave the children
    // it had already started orphaned.
    if let Err(err) = signal::install_handlers() {
        tracing::error!(error = %err, "failed to install signal handlers");
        return ExitCode::FAILURE;
    }

    // A spawn failure means not everything ran as configured — reflect that in
    // the exit code rather than reporting a misleading SUCCESS, even though
    // processes that did spawn are still supervised below.
    let mut loop_ = SupervisorLoop::new(&config.process, SystemClock);
    let had_start_errors = loop_.had_start_errors();
    loop_.run();

    // A shutdown requested by a signal is a normal way to stop a daemon (as it
    // is for a systemd unit), not a failure, so it does not affect the exit
    // code: usage → 2, bad config → 1, start errors → 1, otherwise 0. The
    // shell convention 128+signo is deliberately not used: it means "the
    // process was *killed* by a signal", which would only be honest if we
    // re-raised the signal on ourselves — and that would override the
    // had_start_errors code.
    if had_start_errors {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
