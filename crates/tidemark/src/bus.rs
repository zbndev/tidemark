//! The only thing this process talks to: `tidemarkd`, over the session bus.
//!
//! The client is deliberately the same one a CLI or a Waybar module would write — a proxy
//! generated from the interface, `GetStatus` for the whole picture and `ProviderChanged`
//! for updates. Nothing here knows what a provider is; it moves [`ProviderStatus`] values
//! and stops.
//!
//! # Why it also watches the bus name
//!
//! The daemon is a systemd user unit that gets restarted — after an upgrade, after a crash,
//! and on this machine after every rebuild. A viewer that only subscribed to the signal
//! would sit there showing numbers from before the restart, with no way of knowing they had
//! stopped arriving. So the owner of the bus name is watched too: losing it means the
//! daemon is gone and the window says so, and gaining it means re-reading everything from
//! scratch, because whatever happened while it was away was not announced to anyone.
//!
//! # Why there is no runtime here
//!
//! zbus's `async-io` backend drives its own connection on a thread it owns, so the futures
//! this module awaits can be polled by GTK's main context like any other. That keeps the
//! GUI single-threaded: every status lands on the thread the widgets live on, and there is
//! no channel between the two.

use std::future::poll_fn;
use std::pin::pin;
use std::task::Poll;

use tidemark_types::{ProviderDefinition, ProviderStatus};
use zbus::export::futures_core::Stream;

/// How long to wait before trying the session bus again after a failure. Only reached when
/// the *bus* is unreachable; a daemon that is merely not running is waited for by name,
/// with no polling at all.
const RETRY_SECONDS: u32 = 5;

#[zbus::proxy(
    interface = "io.github.zbndev.Tidemark.Daemon1",
    default_service = "io.github.zbndev.Tidemark.Daemon",
    default_path = "/io/github/zbndev/Tidemark"
)]
pub trait Daemon {
    /// Every provider this build knows how to configure.
    fn list_providers(&self) -> zbus::Result<Vec<ProviderDefinition>>;

    /// Adds a compiled-in provider's default account.
    fn add_provider(&self, provider: &str) -> zbus::Result<()>;

    /// Removes one configured account.
    fn remove_provider(&self, provider: &str, account: &str) -> zbus::Result<()>;

    /// Every account the daemon watches.
    fn get_status(&self) -> zbus::Result<Vec<ProviderStatus>>;

    /// Stored points in the current segment of one window, oldest first.
    fn current_segment(
        &self,
        provider: &str,
        account: &str,
        window: &str,
    ) -> zbus::Result<Vec<tidemark_types::HistoryPoint>>;

    /// Polls now: one provider by slug, or everything when given an empty string.
    fn refresh(&self, provider: &str) -> zbus::Result<()>;

    /// Stores an API key for an account.
    fn set_key(&self, provider: &str, account: &str, key: &str) -> zbus::Result<()>;

    /// Removes whatever credential Tidemark holds for an account.
    fn sign_out(&self, provider: &str, account: &str) -> zbus::Result<()>;

    /// Starts a login and returns the URL to open. Nothing is waited for yet.
    fn begin_login(&self, provider: &str, account: &str) -> zbus::Result<String>;

    /// Waits for a started login to finish. Long-running: up to the browser timeout.
    fn await_login(&self, provider: &str, account: &str) -> zbus::Result<()>;

    /// Abandons a login in progress. Not an error when there is none.
    fn cancel_login(&self, provider: &str, account: &str) -> zbus::Result<()>;

    /// Changes one of a provider's own settings.
    fn set_option(
        &self,
        provider: &str,
        account: &str,
        name: &str,
        value: &str,
    ) -> zbus::Result<()>;

    /// Switches notifications for one of an account's windows on or off.
    fn set_window_notify(
        &self,
        provider: &str,
        account: &str,
        window: &str,
        enabled: bool,
    ) -> zbus::Result<()>;

    /// What the daemon on the other end is.
    #[zbus(property(emits_changed_signal = "false"))]
    fn version(&self) -> zbus::Result<String>;

    /// One account changed.
    #[zbus(signal)]
    fn provider_changed(&self, status: ProviderStatus) -> zbus::Result<()>;

    /// One configured account was removed.
    #[zbus(signal)]
    fn provider_removed(&self, provider: &str, account: &str) -> zbus::Result<()>;
}

/// What the window is told.
///
/// The variants differ in size — one carries a proxy and every status, another carries one
/// status — and that is deliberate rather than an oversight worth boxing away: this value
/// is constructed a handful of times a minute and consumed immediately.
#[derive(Debug)]
pub enum Update {
    /// The daemon answered. Carries everything it knows, and the handle to ask it for more.
    Connected(
        DaemonProxy<'static>,
        Option<String>,
        Vec<ProviderDefinition>,
        Vec<ProviderStatus>,
    ),
    /// One account changed.
    Changed(ProviderStatus),
    /// One configured account was removed.
    Removed { provider: String, account: String },
    /// There is nothing to show, with the reason to put on the screen.
    Waiting(String),
}

/// Starts talking to the daemon, and keeps at it for the life of the process.
///
/// `on` is called on the main thread every time there is something new. It is called with
/// [`Update::Waiting`] rather than being left silent whenever the connection is not
/// usable, so the window never has to guess whether it is still connected.
pub fn watch(on: impl Fn(Update) + 'static) {
    gtk::glib::spawn_future_local(async move {
        loop {
            match serve(&on).await {
                Ok(()) => on(Update::Waiting(
                    "The connection to the daemon closed.".into(),
                )),
                Err(error) => {
                    tracing::warn!(%error, "cannot talk to the session bus");
                    on(Update::Waiting(format!(
                        "Cannot reach the session bus: {error}"
                    )));
                }
            }
            gtk::glib::timeout_future_seconds(RETRY_SECONDS).await;
        }
    });
}

/// One connection's worth of work. Returns when a stream ends, which on a session bus means
/// the bus itself went away.
async fn serve(on: &impl Fn(Update)) -> zbus::Result<()> {
    let connection = zbus::Connection::session().await?;
    let proxy = DaemonProxy::new(&connection).await?;

    // Subscribed before the first `GetStatus`, so that a poll finishing between the two is
    // delivered as a signal rather than missed by both.
    let mut owner = pin!(proxy.inner().receive_owner_changed().await?);
    let mut changes = pin!(proxy.receive_provider_changed().await?);
    let mut removals = pin!(proxy.receive_provider_removed().await?);

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
            Poll::Pending
        })
        .await;

        match event {
            // The daemon appeared, or was replaced by a newer one. Anything it published
            // while we were not listening was announced to nobody, so re-read all of it.
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
            // Either stream ending means the connection is finished with.
            Event::Owner(None) | Event::Changed(None) | Event::Removed(None) => return Ok(()),
        }
    }
}

/// Asks for the whole picture, and reports what came back.
async fn load(proxy: &DaemonProxy<'static>, on: &impl Fn(Update)) {
    let version = match proxy.version().await {
        Ok(version) => Some(version),
        Err(error) => {
            tracing::warn!(%error, "the daemon did not answer Version");
            None
        }
    };
    let definitions = proxy.list_providers().await;
    let statuses = proxy.get_status().await;
    match (definitions, statuses) {
        (Ok(definitions), Ok(statuses)) => on(Update::Connected(
            proxy.clone(),
            version,
            definitions,
            statuses,
        )),
        (Err(error), _) | (_, Err(error)) => {
            tracing::info!(%error, "the daemon did not answer ListProviders or GetStatus");
            on(Update::Waiting("The daemon is not running.".into()));
        }
    }
}

/// One of the two streams produced something.
enum Event {
    Owner(Option<Option<zbus::names::UniqueName<'static>>>),
    Changed(Option<ProviderChanged>),
    Removed(Option<ProviderRemoved>),
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::process;

    use tidemark_types::{ProviderDefinition, ProviderStatus, ids};

    use super::{DaemonProxy, Update, load};

    #[derive(Debug)]
    struct VersionService(&'static str);

    #[zbus::interface(name = "io.github.zbndev.Tidemark.Daemon1")]
    impl VersionService {
        #[zbus(property)]
        fn version(&self) -> String {
            self.0.to_owned()
        }
    }

    #[derive(Debug)]
    struct StatusService;

    #[zbus::interface(name = "io.github.zbndev.Tidemark.Daemon1")]
    impl StatusService {
        fn list_providers(&self) -> Vec<ProviderDefinition> {
            Vec::new()
        }

        fn get_status(&self) -> Vec<ProviderStatus> {
            Vec::new()
        }
    }

    #[test]
    fn the_proxy_is_pointed_at_the_interface_the_daemon_serves() {
        // The macro above needs literals; these are what stop the two drifting apart.
        assert_eq!(ids::DAEMON_INTERFACE, "io.github.zbndev.Tidemark.Daemon1");
        assert_eq!(ids::DAEMON_BUS_NAME, "io.github.zbndev.Tidemark.Daemon");
        assert_eq!(ids::OBJECT_PATH, "/io/github/zbndev/Tidemark");
    }

    #[test]
    fn version_is_read_from_a_replacement_daemon_owner() {
        zbus::block_on(async {
            let Ok(client_connection) = zbus::Connection::session().await else {
                eprintln!("skipped: no session bus is reachable");
                return;
            };
            let name = format!("io.github.zbndev.Tidemark.Test.p{}", process::id());
            let first = zbus::connection::Builder::session()
                .expect("session bus address")
                .name(name.as_str())
                .expect("valid test bus name")
                .serve_at(ids::OBJECT_PATH, VersionService("0.1.0"))
                .expect("valid test object")
                .build()
                .await
                .expect("first test service");
            let proxy = DaemonProxy::builder(&client_connection)
                .destination(name.as_str())
                .expect("test destination")
                .build()
                .await
                .expect("daemon proxy");

            assert_eq!(proxy.version().await.unwrap(), "0.1.0");
            assert!(first.release_name(name.as_str()).await.unwrap());

            let _second = zbus::connection::Builder::session()
                .expect("session bus address")
                .name(name.as_str())
                .expect("valid test bus name")
                .serve_at(ids::OBJECT_PATH, VersionService("0.2.0"))
                .expect("valid test object")
                .build()
                .await
                .expect("replacement test service");

            assert_eq!(proxy.version().await.unwrap(), "0.2.0");
        });
    }

    #[test]
    fn an_unavailable_version_does_not_hide_the_daemons_status() {
        zbus::block_on(async {
            let Ok(client_connection) = zbus::Connection::session().await else {
                eprintln!("skipped: no session bus is reachable");
                return;
            };
            let name = format!("io.github.zbndev.Tidemark.StatusTest.p{}", process::id());
            let _service = zbus::connection::Builder::session()
                .expect("session bus address")
                .name(name.as_str())
                .expect("valid test bus name")
                .serve_at(ids::OBJECT_PATH, StatusService)
                .expect("valid test object")
                .build()
                .await
                .expect("test service");
            let destination = zbus::names::OwnedBusName::try_from(name.as_str())
                .expect("valid owned test destination");
            let proxy = DaemonProxy::builder(&client_connection)
                .destination(destination)
                .expect("test destination")
                .build()
                .await
                .expect("daemon proxy");
            let seen = RefCell::new(None);

            load(&proxy, &|update| {
                seen.replace(Some(update));
            })
            .await;

            match seen.into_inner() {
                Some(Update::Connected(_, version, definitions, statuses)) => {
                    assert_eq!(version, None);
                    assert!(definitions.is_empty());
                    assert!(statuses.is_empty());
                }
                other => panic!("expected connected status, got {other:?}"),
            }
        });
    }
}
