//! supervisor-rs — a minimal process supervisor (mini-systemd) for Unix.
//!
//! Since Этап 5 the binary has multiple subcommands. `run <config-path>` is the
//! daemon: it loads a TOML config, spawns each configured process as the leader
//! of its own process group and supervises them — applying the per-process
//! restart policy (always / on-failure / never) with exponential backoff —
//! until a SIGTERM/SIGINT arrives. That signal is forwarded to each process
//! *group*, so the whole tree a child forked goes down with it; a group that
//! outlives its `stop-grace-secs` is SIGKILLed, as is one whose leader exits
//! leaving stragglers behind. While it runs, the daemon publishes a snapshot of
//! its processes to a state file, which `status` reads and prints, and listens
//! on a control socket (Этап 6) for `start`/`stop`/`restart <name>`, operator
//! commands that reach the running daemon without restarting it.
//!
//! See `docs/TECHNICAL_PLAN.md` for the per-stage breakdown.

use std::path::PathBuf;
use std::process::ExitCode;

use supervisor_rs::cli::{self, Command};
use supervisor_rs::clock::SystemClock;
use supervisor_rs::config;
use supervisor_rs::control::{self, ControlServer, Request, Response};
use supervisor_rs::signal;
use supervisor_rs::state::{self, ReadError};
use supervisor_rs::supervise::SupervisorLoop;

/// Exit code for a malformed command line. The rest of the table is unchanged
/// from Этап 3: bad config → 1, start errors → 1, otherwise 0. Этап 6 adds
/// control-socket bind failure to the "1" class (a startup error, like a bad
/// config) and gives the start/stop/restart client 0 or 1 by its response.
const EXIT_USAGE: u8 = 2;

fn main() -> ExitCode {
    // Structured logging is initialised once, here at the top of the process.
    // RUST_LOG controls verbosity (e.g. RUST_LOG=debug); defaults to info.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args) {
        Ok(Command::Help) => {
            print!("{}", cli::usage());
            ExitCode::SUCCESS
        }
        Ok(Command::Run {
            config,
            state_file,
            control_socket,
        }) => run(&config, state_file, control_socket),
        Ok(Command::Status { state_file }) => status(state_file),
        Ok(Command::Control {
            request,
            control_socket,
        }) => control_command(&request, control_socket),
        Err(err) => {
            eprintln!("error: {err}");
            eprint!("{}", cli::usage());
            ExitCode::from(EXIT_USAGE)
        }
    }
}

fn run(
    config_path: &std::path::Path,
    state_file: Option<PathBuf>,
    control_socket: Option<PathBuf>,
) -> ExitCode {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "supervisor-rs starting"
    );

    let config = match config::load(config_path) {
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

    // Bind the control socket *before* the first spawn: a "someone is already
    // running" refusal must not be preceded by any child that would then need
    // tearing down.
    let socket_path = control_socket.unwrap_or_else(control::default_socket_path);
    let server = match ControlServer::bind(&socket_path) {
        Ok(server) => server,
        Err(err) => {
            tracing::error!(path = %socket_path.display(), error = %err, "failed to bind control socket");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(path = %server.path().display(), "listening for control commands on");

    let state_path = state_file.unwrap_or_else(state::default_path);
    tracing::info!(path = %state_path.display(), "publishing state to");

    // A spawn failure means not everything ran as configured — reflect that in
    // the exit code rather than reporting a misleading SUCCESS, even though
    // processes that did spawn are still supervised below.
    let mut loop_ = SupervisorLoop::new(&config.process, SystemClock)
        .with_state_file(state_path)
        .with_control_server(server);
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

/// Sends one `start`/`stop`/`restart` command to the running daemon and prints
/// its answer (Этап 6).
///
/// Everything here goes through `println!`/`eprintln!` rather than `tracing`:
/// this is the result of a command, not the log of a daemon (see
/// `.claude/skills/rust-process-supervisor-dev/SKILL.md`).
fn control_command(request: &Request, control_socket: Option<PathBuf>) -> ExitCode {
    let path = control_socket.unwrap_or_else(control::default_socket_path);
    match control::send_command(&path, request) {
        Ok(Response::Ok(None)) => {
            println!("ok");
            ExitCode::SUCCESS
        }
        Ok(Response::Ok(Some(msg))) => {
            println!("ok: {msg}");
            ExitCode::SUCCESS
        }
        Ok(Response::Error(msg)) => {
            eprintln!("error: {msg}");
            ExitCode::FAILURE
        }
        Err(client_err) => {
            eprintln!("error: {client_err}");
            ExitCode::FAILURE
        }
    }
}

/// Prints the daemon's published snapshot.
///
/// Everything a user asked for goes through `println!`/`eprintln!` rather than
/// `tracing`: this is the result of a command, not the log of a daemon.
///
/// Every failure is exit code 1 — "there is no state to show" — with a message
/// that says which of the four ways it failed.
fn status(state_file: Option<PathBuf>) -> ExitCode {
    let path = state_file.unwrap_or_else(state::default_path);
    let snapshot = match state::read(&path) {
        Ok(snapshot) => snapshot,
        Err(ReadError::NotFound) => {
            eprintln!(
                "error: supervisor is not running (no state file at {})",
                path.display()
            );
            return ExitCode::FAILURE;
        }
        Err(ReadError::UnsupportedVersion(version)) => {
            eprintln!(
                "error: state file {} has unsupported version {version} (expected {})",
                path.display(),
                state::STATE_VERSION
            );
            return ExitCode::FAILURE;
        }
        // The atomic rename means a parse error is real corruption, not a race
        // with the writer.
        Err(err @ (ReadError::Io(_) | ReadError::Parse(_))) => {
            eprintln!("error: failed to read state file {}: {err}", path.display());
            return ExitCode::FAILURE;
        }
    };

    if !state::daemon_alive(snapshot.daemon_pid) {
        // Read-only command: a file belonging to someone else's daemon is
        // reported, never deleted.
        eprintln!(
            "error: supervisor is not running (state file {} is stale: daemon pid {} is gone)",
            path.display(),
            snapshot.daemon_pid
        );
        return ExitCode::FAILURE;
    }

    println!(
        "{:<20} {:<12} {:>8} {:>9} {:>7}",
        "NAME", "STATE", "PID", "RESTARTS", "UPTIME"
    );
    for proc in &snapshot.process {
        let pid = proc
            .pid
            .map_or_else(|| "-".to_string(), |pid| pid.to_string());
        let uptime = proc
            .uptime_secs
            .map_or_else(|| "-".to_string(), |secs| format!("{secs}s"));
        println!(
            "{:<20} {:<12} {:>8} {:>9} {:>7}",
            proc.name, proc.state, pid, proc.restart_count, uptime
        );
    }
    ExitCode::SUCCESS
}
