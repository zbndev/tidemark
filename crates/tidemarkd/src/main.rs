//! Tidemark's polling daemon.
//!
//! It owns everything that must happen whether or not a window is open: polling the
//! providers, writing history, and publishing what it knows on the session bus. The GUI is
//! a viewer of this process, and a CLI or a Waybar module would be another one — none of
//! them reach a provider themselves.
//!
//! The pieces: `registry` selects configured accounts from the compiled catalog, `keyring`
//! reads their keys, `engine` runs the poll loop over them, `scheduler` decides when the
//! next poll is, and `service` is the D-Bus interface. This file wires them together and
//! handles shutdown.

mod engine;
mod keyring;
mod notify;
mod registry;
mod scheduler;
mod service;

use std::error::Error;
use std::sync::Arc;

use tidemark_core::config::Config;
use tidemark_core::paths;
use tidemark_core::secrets::Secrets;
use tidemark_core::storage::History;
use tidemark_types::ids;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;
use zbus::object_server::SignalEmitter;

use crate::engine::{Command, Engine, Publication};
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
    let config_path = paths::config_path()?;
    // A settings file that does not parse is fatal at startup and only at startup. Coming
    // up with the defaults instead would look like the user's edit had been thrown away,
    // and the first thing this daemon would do with the wrong region is report a rejected
    // key. Once running, a later bad edit is reported and the previous settings stand.
    let config = Config::at(config_path.clone())?;
    let secrets: Arc<dyn Secrets> = Arc::new(keyring::Keyring::default());
    let accounts = registry::accounts(&secrets, &config)?;
    let configured = accounts
        .iter()
        .map(|account| {
            (
                account.status().provider.clone(),
                account.status().account.clone(),
            )
        })
        .collect();
    let catalog = registry::catalog(&config);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        bus_name = ids::DAEMON_BUS_NAME,
        user_agent = tidemark_types::user_agent(),
        accounts = accounts.len(),
        history = %history_path.display(),
        config = %config_path.display(),
        "tidemarkd started"
    );

    let (commands, mut command_queue) = mpsc::channel(COMMAND_QUEUE);
    let (updates, mut update_queue) = mpsc::channel::<Publication>(UPDATE_QUEUE);
    let published = Published::default();

    let connection = zbus::connection::Builder::session()?
        .name(ids::DAEMON_BUS_NAME)?
        .serve_at(
            ids::OBJECT_PATH,
            Daemon::new(
                published.clone(),
                catalog,
                configured,
                commands.clone(),
                Arc::clone(&secrets),
            ),
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
            while let Some(publication) = update_queue.recv().await {
                match publication {
                    Publication::Changed(status) => {
                        published.upsert(status.clone()).await;
                        if let Err(error) = Daemon::provider_changed(&emitter, status).await {
                            tracing::warn!(%error, "could not announce a change");
                        }
                    }
                    Publication::Removed { provider, account } => {
                        let _ = published.remove(&provider, &account).await;
                        if let Err(error) =
                            Daemon::provider_removed(&emitter, &provider, &account).await
                        {
                            tracing::warn!(%error, "could not announce a removal");
                        }
                    }
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

    // The daemon's own session-bus connection carries the notifications too: it is already
    // open, and org.freedesktop.Notifications is on the same bus as everything else here.
    let notifier = Arc::new(notify::Desktop::new(connection.clone()));
    let mut engine = Engine::new(
        accounts,
        history,
        secrets,
        updates,
        config_path,
        notifier as Arc<dyn notify::Notifier>,
    );
    // Before the first announcement, so a client connecting immediately is told whether
    // each account has a credential rather than having to wait a poll to find out.
    engine.probe_credentials(None).await;
    engine.announce().await;
    engine.run(&mut command_queue).await;

    // Dropping the engine closes the update channel, which ends the publisher; that
    // ordering is what makes the last status of the session reach its clients.
    drop(engine);
    let _ = publisher.await;
    signals.abort();
    Ok(())
}
