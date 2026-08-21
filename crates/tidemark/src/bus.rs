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

use tidemark_types::ProviderStatus;
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
    /// Every account the daemon watches.
    fn get_status(&self) -> zbus::Result<Vec<ProviderStatus>>;

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

    /// What the daemon on the other end is.
    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;

    /// One account changed.
    #[zbus(signal)]
    fn provider_changed(&self, status: ProviderStatus) -> zbus::Result<()>;
}

/// What the window is told.
///
/// The variants differ in size — one carries a proxy and every status, another carries one
/// status — and that is deliberate rather than an oversight worth boxing away: this value
/// is constructed a handful of times a minute and consumed immediately.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "constructed a few times a minute"
)]
pub enum Update {
    /// The daemon answered. Carries everything it knows, and the handle to ask it for more.
    Connected(DaemonProxy<'static>, Vec<ProviderStatus>),
    /// One account changed.
    Changed(ProviderStatus),
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

    load(&proxy, on).await;

    loop {
        let event = poll_fn(|context| {
            if let Poll::Ready(owner) = owner.as_mut().poll_next(context) {
                return Poll::Ready(Event::Owner(owner));
            }
            if let Poll::Ready(change) = changes.as_mut().poll_next(context) {
                return Poll::Ready(Event::Changed(change));
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
            // Either stream ending means the connection is finished with.
            Event::Owner(None) | Event::Changed(None) => return Ok(()),
        }
    }
}

/// Asks for the whole picture, and reports what came back.
async fn load(proxy: &DaemonProxy<'static>, on: &impl Fn(Update)) {
    match proxy.get_status().await {
        Ok(statuses) => on(Update::Connected(proxy.clone(), statuses)),
        Err(error) => {
            tracing::info!(%error, "the daemon did not answer GetStatus");
            on(Update::Waiting("The daemon is not running.".into()));
        }
    }
}

/// One of the two streams produced something.
enum Event {
    Owner(Option<Option<zbus::names::UniqueName<'static>>>),
    Changed(Option<ProviderChanged>),
}

#[cfg(test)]
mod tests {
    use tidemark_types::ids;

    #[test]
    fn the_proxy_is_pointed_at_the_interface_the_daemon_serves() {
        // The macro above needs literals; these are what stop the two drifting apart.
        assert_eq!(ids::DAEMON_INTERFACE, "io.github.zbndev.Tidemark.Daemon1");
        assert_eq!(ids::DAEMON_BUS_NAME, "io.github.zbndev.Tidemark.Daemon");
        assert_eq!(ids::OBJECT_PATH, "/io/github/zbndev/Tidemark");
    }
}
