//! The daemon connection: the GTK client's `bus.rs` with Slint's event loop in place of
//! GLib's. The same proxy, the same signals, the same bus-name watch; zbus's `async-io`
//! backend drives the connection on its own thread, so these futures run on the UI thread.
//!
//! Linux only for now: the Windows p2p endpoint and daemon spawning stay in the GTK client
//! until this prototype earns the move.

use std::future::poll_fn;
use std::pin::pin;
use std::task::Poll;
use std::time::Duration;

use tidemark_types::{DataInfo, Preferences, ProviderDefinition, ProviderStatus, ids};
use zbus::export::futures_core::Stream;

pub use tidemark_ipc::DaemonProxy;

const RETRY: Duration = Duration::from_secs(5);

/// Everything the daemon can tell the window.
#[derive(Debug)]
pub enum Update {
    Connected {
        proxy: DaemonProxy<'static>,
        available: String,
        preferences: Preferences,
        definitions: Vec<ProviderDefinition>,
        statuses: Vec<ProviderStatus>,
        data: DataInfo,
    },
    Changed(ProviderStatus),
    Removed {
        provider: String,
        account: String,
    },
    Reordered(Vec<String>),
    Available(String),
    Preferences(Preferences),
    Data(DataInfo),
    Catalog(Vec<ProviderDefinition>),
    Waiting(String),
}

/// Connects, and keeps reconnecting, for the life of the program.
pub fn watch(on: impl Fn(Update) + 'static) {
    let spawned = slint::spawn_local(async move {
        loop {
            match serve(&on).await {
                Ok(()) => on(Update::Waiting(
                    "The connection to the daemon closed.".into(),
                )),
                Err(error) => {
                    tracing::warn!(%error, "cannot talk to the daemon");
                    on(Update::Waiting(format!(
                        "Cannot reach the session bus: {error}"
                    )));
                }
            }
            async_io::Timer::after(RETRY).await;
        }
    });
    if let Err(error) = spawned {
        tracing::error!(%error, "the event loop refused the daemon watcher");
    }
}

#[cfg(unix)]
async fn connect() -> zbus::Result<zbus::Connection> {
    zbus::Connection::session().await
}

#[cfg(not(unix))]
async fn connect() -> zbus::Result<zbus::Connection> {
    Err(zbus::Error::Unsupported)
}

enum Event {
    Owner(Option<Option<zbus::names::UniqueName<'static>>>),
    Changed(Option<tidemark_ipc::ProviderChanged>),
    Removed(Option<tidemark_ipc::ProviderRemoved>),
    Reordered(Option<tidemark_ipc::OrderChanged>),
    Available(Option<tidemark_ipc::UpdateChanged>),
    Preferences(Option<tidemark_ipc::PreferencesChanged>),
    Data(Option<tidemark_ipc::DataChanged>),
    Plugins(Option<tidemark_ipc::PluginsChanged>),
}

async fn serve(on: &impl Fn(Update)) -> zbus::Result<()> {
    let connection = connect().await?;
    let proxy = DaemonProxy::new(&connection).await?;

    let mut owner = pin!(proxy.inner().receive_owner_changed().await?);
    let mut changes = pin!(proxy.receive_provider_changed().await?);
    let mut removals = pin!(proxy.receive_provider_removed().await?);
    let mut orders = pin!(proxy.receive_order_changed().await?);
    let mut updates = pin!(proxy.receive_update_changed().await?);
    let mut preference_changes = pin!(proxy.receive_preferences_changed().await?);
    let mut data_changes = pin!(proxy.receive_data_changed().await?);
    let mut plugin_changes = pin!(proxy.receive_plugins_changed().await?);

    load(&proxy, on).await;

    loop {
        let event = poll_fn(|context| {
            if let Poll::Ready(owner) = owner.as_mut().poll_next(context) {
                return Poll::Ready(Event::Owner(owner));
            }
            if let Poll::Ready(change) = changes.as_mut().poll_next(context) {
                return Poll::Ready(Event::Changed(change));
            }
            if let Poll::Ready(removal) = removals.as_mut().poll_next(context) {
                return Poll::Ready(Event::Removed(removal));
            }
            if let Poll::Ready(order) = orders.as_mut().poll_next(context) {
                return Poll::Ready(Event::Reordered(order));
            }
            if let Poll::Ready(update) = updates.as_mut().poll_next(context) {
                return Poll::Ready(Event::Available(update));
            }
            if let Poll::Ready(preferences) = preference_changes.as_mut().poll_next(context) {
                return Poll::Ready(Event::Preferences(preferences));
            }
            if let Poll::Ready(data) = data_changes.as_mut().poll_next(context) {
                return Poll::Ready(Event::Data(data));
            }
            if let Poll::Ready(plugins) = plugin_changes.as_mut().poll_next(context) {
                return Poll::Ready(Event::Plugins(plugins));
            }
            Poll::Pending
        })
        .await;

        match event {
            Event::Owner(Some(Some(unique))) => {
                tracing::info!(%unique, "the daemon is on the bus");
                load(&proxy, on).await;
            }
            Event::Owner(Some(None)) => {
                tracing::info!("the daemon left the bus");
                on(Update::Waiting("The daemon is not running.".into()));
            }
            Event::Changed(Some(signal)) => match signal.args() {
                Ok(args) => on(Update::Changed(args.status)),
                Err(error) => tracing::warn!(%error, "a ProviderChanged signal did not parse"),
            },
            Event::Removed(Some(signal)) => match signal.args() {
                Ok(args) => on(Update::Removed {
                    provider: args.provider.to_owned(),
                    account: args.account.to_owned(),
                }),
                Err(error) => tracing::warn!(%error, "a ProviderRemoved signal did not parse"),
            },
            Event::Reordered(Some(signal)) => match signal.args() {
                Ok(args) => on(Update::Reordered(args.providers)),
                Err(error) => tracing::warn!(%error, "an OrderChanged signal did not parse"),
            },
            Event::Available(Some(signal)) => match signal.args() {
                Ok(args) => on(Update::Available(args.version.to_owned())),
                Err(error) => tracing::warn!(%error, "an UpdateChanged signal did not parse"),
            },
            Event::Preferences(Some(signal)) => match signal.args() {
                Ok(args) => on(Update::Preferences(args.preferences)),
                Err(error) => tracing::warn!(%error, "a PreferencesChanged signal did not parse"),
            },
            Event::Data(Some(signal)) => match signal.args() {
                Ok(args) => on(Update::Data(args.data)),
                Err(error) => tracing::warn!(%error, "a DataChanged signal did not parse"),
            },
            Event::Plugins(Some(_)) => match proxy.list_providers().await {
                Ok(definitions) => on(Update::Catalog(definitions)),
                Err(error) => {
                    tracing::warn!(%error, "the catalog could not be re-read after PluginsChanged");
                }
            },
            Event::Owner(None)
            | Event::Changed(None)
            | Event::Removed(None)
            | Event::Reordered(None)
            | Event::Available(None)
            | Event::Preferences(None)
            | Event::Data(None)
            | Event::Plugins(None) => return Ok(()),
        }
    }
}

/// Reads the whole picture. Optional calls an older daemon lacks fall back to defaults.
async fn load(proxy: &DaemonProxy<'static>, on: &impl Fn(Update)) {
    let definitions = proxy.list_providers().await;
    let statuses = proxy.get_status().await;
    let available = proxy.get_update().await.unwrap_or_default();
    let preferences = proxy.get_preferences().await.unwrap_or_else(|error| {
        tracing::info!(%error, "the daemon did not answer GetPreferences; using defaults");
        Preferences::default()
    });
    let data = proxy.get_data_info().await.unwrap_or_else(|_| DataInfo {
        config_path: String::new(),
        history_path: String::new(),
        history_bytes: 0,
        key_schema: ids::SECRET_SCHEMA.into(),
        token_schema: ids::TOKEN_SCHEMA.into(),
        release_check_available: false,
        plugin_icons_path: String::new(),
    });
    match (definitions, statuses) {
        (Ok(definitions), Ok(statuses)) => on(Update::Connected {
            proxy: proxy.clone(),
            available,
            preferences,
            definitions,
            statuses,
            data,
        }),
        (Err(error), _) | (_, Err(error)) => {
            tracing::info!(%error, "the daemon did not answer ListProviders or GetStatus");
            on(Update::Waiting("The daemon is not running.".into()));
        }
    }
}
