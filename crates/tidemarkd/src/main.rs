//! Tidemark's polling daemon.
//!
//! Scaffolding only: it brings up the async runtime and shuts down cleanly on SIGTERM or
//! SIGINT, so the systemd user unit added in Step 5 has something well-behaved to
//! supervise. Polling, storage and the D-Bus interface arrive in later steps.

use tokio::signal::unix::{SignalKind, signal};

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tidemarkd=info".into()),
        )
        .init();

    if std::env::args().any(|a| a == "--version") {
        println!("tidemarkd {}", env!("CARGO_PKG_VERSION"));
        return std::process::ExitCode::SUCCESS;
    }

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::error!(%error, "could not start the async runtime");
            return std::process::ExitCode::FAILURE;
        }
    };

    match runtime.block_on(run()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(%error, "shutting down after a fatal error");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> std::io::Result<()> {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        bus_name = tidemark_types::ids::DAEMON_BUS_NAME,
        user_agent = tidemark_types::user_agent(),
        "tidemarkd started"
    );

    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let reason = tokio::select! {
        _ = term.recv() => "SIGTERM",
        _ = int.recv() => "SIGINT",
    };

    tracing::info!(reason, "tidemarkd stopping");
    Ok(())
}
