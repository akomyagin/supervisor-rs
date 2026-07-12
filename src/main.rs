//! supervisor-rs — a minimal process supervisor (mini-systemd) for Unix.
//!
//! Этап 0 (bootstrap): this is a skeleton only. `main` wires up logging and
//! prints a banner so `cargo run` works; the actual supervise loop is added in
//! later stages. See `docs/TECHNICAL_PLAN.md` for the per-stage breakdown.

mod config;
mod process;

fn main() {
    // Structured logging is initialised once, here at the top of the process.
    // RUST_LOG controls verbosity (e.g. RUST_LOG=debug); defaults to info.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "supervisor-rs starting");

    // TODO(Этап 1): parse CLI args, load the config file via `config::load`,
    // and hand the process list to the supervise loop.
    // TODO(Этап 2): apply restart policy (always / on-failure / never) + backoff.
    // TODO(Этап 3): install signal handlers and forward SIGTERM/SIGINT to children.
    // TODO(Этап 4): put each child in its own process group and tear the whole
    //               tree down with SIGTERM → timeout → SIGKILL.
    // TODO(Этап 5): expose a status/control CLI subcommand.

    tracing::info!("nothing to supervise yet — see docs/TECHNICAL_PLAN.md");
}
