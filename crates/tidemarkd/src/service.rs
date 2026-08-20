//! The D-Bus interface, and the state it serves.
//!
//! Shaped as if a CLI and a Waybar module were already consuming it, because they will be:
//! `GetStatus` answers the whole question in one call — every account, its state, its
//! windows, and when the daemon intends to poll next — so a script can render a bar from a
//! single round trip without modelling the scheduler. `ProviderChanged` carries the same
//! shape, so a long-running client never needs a second code path for updates.
//!
//! Everything is verifiable with `busctl` before any GUI exists:
//!
//! ```sh
//! busctl --user introspect io.github.zbndev.Tidemark.Daemon /io/github/zbndev/Tidemark
//! busctl --user call io.github.zbndev.Tidemark.Daemon /io/github/zbndev/Tidemark \
//!     io.github.zbndev.Tidemark.Daemon1 GetStatus
//! busctl --user call io.github.zbndev.Tidemark.Daemon /io/github/zbndev/Tidemark \
//!     io.github.zbndev.Tidemark.Daemon1 Refresh s ""
//! ```

use std::sync::Arc;

use tidemark_types::ProviderStatus;
use tokio::sync::{RwLock, mpsc};
use zbus::object_server::SignalEmitter;
use zbus::{fdo, interface};

use crate::engine::Command;

/// The statuses clients read, in the order the accounts were registered.
///
/// One writer — the task draining the engine's updates — and any number of readers, which
/// is what an `RwLock` is for. The order is stable so a client redrawing a grid does not
/// have to sort to avoid cards jumping around.
#[derive(Debug, Clone, Default)]
pub struct Published(Arc<RwLock<Vec<ProviderStatus>>>);

impl Published {
    /// Replaces the entry for this account, or appends it the first time it is seen.
    pub async fn upsert(&self, status: ProviderStatus) {
        let mut statuses = self.0.write().await;
        match statuses
            .iter_mut()
            .find(|held| held.provider == status.provider && held.account == status.account)
        {
            Some(held) => *held = status,
            None => statuses.push(status),
        }
    }

    /// Everything currently known.
    pub async fn all(&self) -> Vec<ProviderStatus> {
        self.0.read().await.clone()
    }

    /// Whether any account is filed under this provider slug.
    pub async fn knows(&self, provider: &str) -> bool {
        self.0
            .read()
            .await
            .iter()
            .any(|held| held.provider == provider)
    }
}

/// The object served at `/io/github/zbndev/Tidemark`.
#[derive(Debug)]
pub struct Daemon {
    statuses: Published,
    commands: mpsc::Sender<Command>,
}

impl Daemon {
    /// Wires the interface to the published state and the poll loop.
    pub fn new(statuses: Published, commands: mpsc::Sender<Command>) -> Self {
        Self { statuses, commands }
    }
}

#[interface(name = "io.github.zbndev.Tidemark.Daemon1")]
impl Daemon {
    /// Every account the daemon watches, with its current state and last good reading.
    ///
    /// Never empty while accounts are configured: they are published as `pending` before
    /// the first poll, so a client can tell "nothing configured" from "nothing yet".
    async fn get_status(&self) -> Vec<ProviderStatus> {
        self.statuses.all().await
    }

    /// Polls now: one provider by slug, or every account when given an empty string.
    ///
    /// The credential is read again as part of this, so it is also the call the settings
    /// dialog makes after storing a new key.
    async fn refresh(&self, provider: &str) -> fdo::Result<()> {
        let target = (!provider.is_empty()).then(|| provider.to_owned());
        if let Some(slug) = target.as_deref()
            && !self.statuses.knows(slug).await
        {
            return Err(fdo::Error::InvalidArgs(format!(
                "no account is configured for provider {slug}"
            )));
        }
        self.commands
            .send(Command::Refresh(target))
            .await
            .map_err(|_| fdo::Error::Failed("the poll loop has stopped".into()))
    }

    /// The daemon's version, so a client can tell what it is talking to.
    #[zbus(property)]
    async fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }

    /// One account's status changed: it was polled, it failed, or it is waiting for
    /// something. Carries the same shape as `GetStatus`, so a client has one parser.
    #[zbus(signal)]
    pub async fn provider_changed(
        emitter: &SignalEmitter<'_>,
        status: ProviderStatus,
    ) -> zbus::Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderId, ids};

    fn status(provider: &str) -> ProviderStatus {
        ProviderStatus::pending(&ProviderId::new(provider), &AccountId::default())
    }

    #[test]
    fn the_interface_name_is_the_one_clients_are_told_about() {
        // The attribute above needs a literal; this is what stops the two drifting apart.
        assert_eq!("io.github.zbndev.Tidemark.Daemon1", ids::DAEMON_INTERFACE);
    }

    #[tokio::test]
    async fn an_account_keeps_its_place_when_it_is_updated() {
        let published = Published::default();
        published.upsert(status("zai")).await;
        published.upsert(status("kimi")).await;

        let mut updated = status("zai");
        updated.captured_at = Some(1_785_700_000);
        published.upsert(updated).await;

        let all = published.all().await;
        assert_eq!(all.len(), 2, "an update replaces rather than appends");
        assert_eq!(all[0].provider, "zai", "and does not reorder the grid");
        assert_eq!(all[0].captured_at, Some(1_785_700_000));
    }

    /// The bus name the test daemon owns. Not the real one: a developer's session usually
    /// has a real `tidemarkd` running on it, and a test must not fight it for the name.
    const TEST_BUS_NAME: &str = "io.github.zbndev.Tidemark.DaemonTest";

    async fn serve(daemon: Daemon) -> zbus::Result<zbus::Connection> {
        zbus::connection::Builder::session()?
            .name(TEST_BUS_NAME)?
            .serve_at(ids::OBJECT_PATH, daemon)?
            .build()
            .await
    }

    #[tokio::test]
    async fn a_refresh_for_an_unconfigured_provider_is_an_error_rather_than_silence() {
        let published = Published::default();
        published.upsert(status("zai")).await;
        let (tx, mut rx) = mpsc::channel(4);
        let daemon = Daemon::new(published, tx);

        assert!(
            daemon.refresh("codex").await.is_err(),
            "a typo must be visible"
        );
        assert!(daemon.refresh("zai").await.is_ok());
        assert_eq!(
            rx.try_recv().expect("the loop was told"),
            Command::Refresh(Some("zai".into()))
        );

        assert!(
            daemon.refresh("").await.is_ok(),
            "an empty slug means everything"
        );
        assert_eq!(
            rx.try_recv().expect("the loop was told"),
            Command::Refresh(None)
        );
    }

    /// Talks to the interface the way `busctl`, the GUI and any future CLI will: over a
    /// real session bus, with nothing of ours on the client side but the wire types.
    ///
    /// Skipped rather than failed where no bus is reachable, on the same principle as the
    /// Secret Service tests in `tidemark-core`: a headless checkout must still build.
    #[tokio::test]
    async fn a_client_reads_the_daemon_over_a_real_session_bus() {
        use std::pin::pin;
        use std::time::Duration;
        use zbus::export::futures_core::Stream;
        use zbus::{MatchRule, MessageStream};

        let published = Published::default();
        published.upsert(status("zai")).await;
        let (commands, mut command_queue) = mpsc::channel(4);

        let Ok(server) = serve(Daemon::new(published, commands)).await else {
            eprintln!("skipped: no session bus reachable");
            return;
        };
        let client = zbus::Connection::session()
            .await
            .expect("the server connected, so the client can too");

        let call = async |method: &str, argument: &str| {
            client
                .call_method(
                    Some(TEST_BUS_NAME),
                    ids::OBJECT_PATH,
                    Some(ids::DAEMON_INTERFACE),
                    method,
                    &argument,
                )
                .await
        };

        let reply = client
            .call_method(
                Some(TEST_BUS_NAME),
                ids::OBJECT_PATH,
                Some(ids::DAEMON_INTERFACE),
                "GetStatus",
                &(),
            )
            .await
            .expect("GetStatus answers");
        let statuses: Vec<ProviderStatus> = reply
            .body()
            .deserialize()
            .expect("the published shape decodes on the other side of the bus");
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].provider, "zai");
        assert_eq!(statuses[0].state, "pending");

        call("Refresh", "zai")
            .await
            .expect("a known provider refreshes");
        assert_eq!(
            command_queue.recv().await,
            Some(Command::Refresh(Some("zai".into()))),
            "the call reached the poll loop"
        );
        assert!(
            call("Refresh", "codex").await.is_err(),
            "and an unconfigured one comes back as a D-Bus error"
        );

        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(ids::DAEMON_INTERFACE)
            .expect("a valid interface name")
            .member("ProviderChanged")
            .expect("a valid member name")
            .build();
        let signals = MessageStream::for_match_rule(rule, &client, Some(4))
            .await
            .expect("the bus accepts the match rule");
        let mut signals = pin!(signals);

        let emitter = SignalEmitter::new(&server, ids::OBJECT_PATH).expect("a valid path");
        let mut announced = status("zai");
        announced.captured_at = Some(1_785_700_000);
        Daemon::provider_changed(&emitter, announced)
            .await
            .expect("the signal goes out");

        // `poll_next` by hand rather than pulling in `futures-util` for one call: the
        // stream is what zbus hands us, and this crate has no other use for the trait.
        let next = std::future::poll_fn(|cx| signals.as_mut().poll_next(cx));
        let received = tokio::time::timeout(Duration::from_secs(5), next)
            .await
            .expect("the signal arrives")
            .expect("the stream is alive")
            .expect("the message is well formed");
        let carried: ProviderStatus = received
            .body()
            .deserialize()
            .expect("a client parses the signal with the same code as GetStatus");
        assert_eq!(carried.captured_at, Some(1_785_700_000));
    }
}
