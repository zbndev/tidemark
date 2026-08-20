//! Tidemark's polling daemon.
//!
//! It owns everything that must happen whether or not a window is open: polling the
//! providers, writing history, and publishing what it knows on the session bus. The GUI is
//! a viewer of this process, and a CLI or a Waybar module would be another one — none of
//! them reach a provider themselves.
//!
//! The pieces: `registry` says which accounts exist, `keyring` reads their keys, `engine`
//! runs the poll loop over them, `scheduler` decides when the next poll is, and `service`
//! is the D-Bus interface. This file wires them together and handles shutdown.

mod engine;
mod keyring;
mod registry;
mod scheduler;
mod service;

use std::error::Error;
use std::sync::Arc;

use tidemark_core::paths;
use tidemark_core::storage::History;
use tidemark_types::{ProviderStatus, ids};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;

use crate::engine::{Command, Engine};
use crate::service::{Daemon, Published};

/// Commands from D-Bus clients. Small: a burst of refreshes is a user hammering a button,
/// and the loop collapses them into one poll anyway.
const COMMAND_QUEUE: usize = 16;

/// Finished statuses waiting to be published. One per account per poll.
const UPDATE_QUEUE: usize = 64;

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
            tracing::error!(error = %error, "shutting down after a fatal error");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let history_path = paths::history_path()?;
    let history = History::open(&history_path)?;
    let accounts = registry::accounts()?;

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        bus_name = ids::DAEMON_BUS_NAME,
        user_agent = tidemark_types::user_agent(),
        accounts = accounts.len(),
        history = %history_path.display(),
        "tidemarkd started"
    );

    let (commands, mut command_queue) = mpsc::channel(COMMAND_QUEUE);
    let (updates, mut update_queue) = mpsc::channel::<ProviderStatus>(UPDATE_QUEUE);
    let published = Published::default();

    let connection = zbus::connection::Builder::session()?
        .name(ids::DAEMON_BUS_NAME)?
        .serve_at(
            ids::OBJECT_PATH,
            Daemon::new(published.clone(), commands.clone()),
        )?
        .build()
        .await
        .map_err(|error| match error {
            zbus::Error::NameTaken => format!(
                "another tidemarkd already owns {}; this one is not needed",
                ids::DAEMON_BUS_NAME
            )
            .into(),
            other => Box::<dyn Error>::from(other),
        })?;

    // Publishing is one task so that the shared state is always written before the signal
    // that announces it: a client woken by `ProviderChanged` and calling `GetStatus`
    // straight away must never see the older value.
    let emitter = SignalEmitter::new(&connection, ids::OBJECT_PATH)?;
    let publisher = tokio::spawn({
        let published = published.clone();
        async move {
            while let Some(status) = update_queue.recv().await {
                published.upsert(status.clone()).await;
                if let Err(error) = Daemon::provider_changed(&emitter, status).await {
                    tracing::warn!(%error, "could not announce a change");
                }
            }
        }
    });

    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let signals = tokio::spawn({
        let commands = commands.clone();
        async move {
            let reason = tokio::select! {
                _ = term.recv() => "SIGTERM",
                _ = interrupt.recv() => "SIGINT",
            };
            tracing::info!(reason, "tidemarkd stopping");
            let _ = commands.send(Command::Shutdown).await;
        }
    });

    let mut engine = Engine::new(
        accounts,
        history,
        Arc::new(keyring::Keyring::default()),
        updates,
    );
    engine.announce().await;
    engine.run(&mut command_queue).await;

    // Dropping the engine closes the update channel, which ends the publisher; that
    // ordering is what makes the last status of the session reach its clients.
    drop(engine);
    let _ = publisher.await;
    signals.abort();
    Ok(())
}
