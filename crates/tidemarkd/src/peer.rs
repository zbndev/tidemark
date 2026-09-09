//! Per-peer p2p fan-out for the daemon's signals (Windows IPC, decision A1).
//!
//! On Windows there is no session bus to broadcast on, so every accepted p2p client gets
//! its own zbus connection and its own bounded queue. [`PeerHub::publish`] never awaits a
//! laggard: it `try_send`s under the hub lock and evicts — disconnects — whichever peer's
//! queue is full or gone. On Linux this module is compiled and unit-tested but the daemon
//! keeps its session-bus emitters, so the Linux signal path is byte-for-byte what it was.

use std::collections::BTreeMap;
#[cfg(windows)]
use std::fs;
#[cfg(windows)]
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
#[cfg(any(windows, test))]
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
#[cfg(windows)]
use std::thread;

#[cfg(any(windows, test))]
use tidemark_types::ids;
use tidemark_types::{DataInfo, PluginInfo, Preferences, ProviderStatus};
use tokio::sync::mpsc;
use zbus::Connection;
#[cfg(any(windows, test))]
use zbus::Guid;
#[cfg(any(windows, test))]
use zbus::object_server::SignalEmitter;

#[cfg(all(unix, any(windows, test)))]
use std::os::unix::net::UnixStream;
#[cfg(windows)]
use uds_windows::{UnixListener, UnixStream};

#[cfg(any(windows, test))]
use crate::service::Daemon;

/// Every peer's queue holds at most this many announcements. A peer that falls further
/// behind than this is evicted rather than allowed to slow the daemon down.
#[cfg(any(windows, test))]
pub const PEER_QUEUE_BOUND: usize = 128;

/// zbus's own per-connection queue, the value the frozen A1 builder contract fixed.
#[cfg(any(windows, test))]
const ZBUS_QUEUE_BOUND: usize = 64;

/// One of the six daemon signals, ready to hand to any peer's emitter.
///
/// The Linux session-bus path constructs the Preferences/Data/Update variants but never
/// reads them back — only the Windows deliverer matches on announcements — and the
/// Provider/Order variants exist for Windows and the loopback test, so a Linux build
/// would call every variant and its payload dead.
#[allow(dead_code)]
#[derive(Debug, Clone)]
// Not boxed per clippy::large_enum_variant: the announcements are queued
// at a rate of 128 per peer and moved once into the emitter — a heap
// indirection on the hot path buys nothing measurable, and boxing the
// largest variant would touch every construction site for it.
#[allow(clippy::large_enum_variant)]
pub(crate) enum Announcement {
    ProviderChanged(ProviderStatus),
    ProviderRemoved { provider: String, account: String },
    OrderChanged(Vec<String>),
    PreferencesChanged(Preferences),
    DataChanged(DataInfo),
    UpdateChanged(String),
    PluginsChanged(Vec<PluginInfo>),
    ActivateRequested,
}

impl Announcement {
    /// Emits this signal through one emitter — one peer's on p2p, the session bus on Linux.
    #[cfg(any(windows, test))]
    pub(crate) async fn emit(&self, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        match self {
            Self::ProviderChanged(status) => {
                Daemon::provider_changed(emitter, status.clone()).await
            }
            Self::ProviderRemoved { provider, account } => {
                Daemon::provider_removed(emitter, provider, account).await
            }
            Self::OrderChanged(providers) => {
                Daemon::order_changed(emitter, providers.clone()).await
            }
            Self::PreferencesChanged(preferences) => {
                Daemon::preferences_changed(emitter, preferences.clone()).await
            }
            Self::DataChanged(data) => Daemon::data_changed(emitter, data.clone()).await,
            Self::ActivateRequested => Daemon::activate_requested(emitter).await,
            Self::UpdateChanged(version) => Daemon::update_changed(emitter, version).await,
            Self::PluginsChanged(plugins) => {
                Daemon::plugins_changed(emitter, plugins.clone()).await
            }
        }
    }
}

/// One accepted peer: its bounded queue and, once the handshake has completed, its
/// connection — the handle an eviction closes.
#[derive(Debug)]
struct Peer {
    queue: mpsc::Sender<Announcement>,
    connection: Arc<OnceLock<Connection>>,
}

/// The set of live p2p peers and the fan-out point for announcements.
#[derive(Debug, Default)]
pub(crate) struct PeerHub {
    peers: StdMutex<BTreeMap<u64, Peer>>,
    /// The counter's only reader is register(), which is Windows-or-test-only; the hub
    /// itself stays live on Linux through the shared announce paths.
    #[allow(dead_code)]
    next_id: AtomicU64,
}

impl PeerHub {
    /// Files a queue for a peer before its connection is built, so an announcement can
    /// never race the window between accept and registration.
    #[cfg(any(windows, test))]
    pub(crate) fn register(&self) -> (u64, mpsc::Receiver<Announcement>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (queue, receiver) = mpsc::channel(PEER_QUEUE_BOUND);
        self.peers
            .lock()
            .expect("no code panics while holding the peer hub")
            .insert(
                id,
                Peer {
                    queue,
                    connection: Arc::new(OnceLock::new()),
                },
            );
        (id, receiver)
    }

    /// Remembers the peer's connection once the p2p handshake has completed.
    #[cfg(any(windows, test))]
    fn attach(&self, id: u64, connection: &Connection) {
        if let Some(peer) = self
            .peers
            .lock()
            .expect("no code panics while holding the peer hub")
            .get(&id)
        {
            let _ = peer.connection.set(connection.clone());
        }
    }

    /// Drops a registration: the handshake failed, or the peer is gone.
    #[cfg(any(windows, test))]
    pub(crate) fn forget(&self, id: u64) {
        self.peers
            .lock()
            .expect("no code panics while holding the peer hub")
            .remove(&id);
    }

    #[cfg(any(windows, test))]
    pub(crate) fn peer_count(&self) -> usize {
        self.peers
            .lock()
            .expect("no code panics while holding the peer hub")
            .len()
    }

    /// Fans one announcement out to every peer without awaiting any of them.
    ///
    /// A peer whose queue is full, or whose deliverer is gone, is evicted whole — its
    /// registration is dropped here and its connection closed — never coalesced, never
    /// dropped selectively. The caller commits shared state before publishing, so a peer
    /// woken by this signal and calling back into the daemon sees the newer value.
    pub(crate) async fn publish(&self, announcement: Announcement) {
        let evicted = {
            let mut peers = self
                .peers
                .lock()
                .expect("no code panics while holding the peer hub");
            let mut evicted = Vec::new();
            peers.retain(
                |_id, peer| match peer.queue.try_send(announcement.clone()) {
                    Ok(()) => true,
                    Err(
                        mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_),
                    ) => {
                        evicted.push(Arc::clone(&peer.connection));
                        false
                    }
                },
            );
            evicted
        };
        for connection in evicted {
            if let Some(connection) = connection.get()
                && let Err(error) = connection.clone().close().await
            {
                tracing::warn!(%error, "could not close an evicted peer's connection");
            }
        }
    }
}

/// Emits one peer's queued announcements in order until the hub drops the registration
/// (which closes this stream) or an emission fails because the peer is gone.
#[cfg(any(windows, test))]
async fn deliver(mut queue: mpsc::Receiver<Announcement>, connection: Connection) {
    let emitter = match SignalEmitter::new(&connection, ids::OBJECT_PATH) {
        Ok(emitter) => emitter,
        Err(error) => {
            tracing::warn!(%error, "a p2p peer could not be given an emitter");
            return;
        }
    };
    while let Some(announcement) = queue.recv().await {
        if let Err(error) = announcement.emit(&emitter).await {
            tracing::debug!(%error, "a p2p peer stopped taking signals");
            return;
        }
    }
}

/// Serves one accepted stream: registers the queue first, builds the peer's p2p
/// connection second, and forgets the peer on either a failed handshake or a disconnect.
#[cfg(any(windows, test))]
async fn serve_peer(stream: UnixStream, guid: Guid<'static>, daemon: Daemon, hub: Arc<PeerHub>) {
    let (id, queue) = hub.register();
    let builder = zbus::connection::Builder::async_io_unix_stream(stream)
        .server(guid)
        .map(|builder| builder.p2p())
        .and_then(|builder| builder.name(ids::DAEMON_BUS_NAME))
        .map(|builder| builder.max_queued(ZBUS_QUEUE_BOUND))
        .and_then(|builder| builder.serve_at(ids::OBJECT_PATH, daemon));
    let connection = match builder {
        Ok(builder) => builder.build().await,
        Err(error) => Err(error),
    };
    let connection = match connection {
        Ok(connection) => connection,
        Err(error) => {
            hub.forget(id);
            tracing::warn!(%error, "a p2p peer failed its handshake");
            return;
        }
    };
    hub.attach(id, &connection);
    tokio::spawn(deliver(queue, connection.clone()));
    tracing::debug!(peers = hub.peer_count(), "a p2p peer connected");
    // There is no owner-changed signal to watch on p2p: the connection closing is the
    // whole story of a peer leaving.
    connection.closed().await;
    hub.forget(id);
}

/// The p2p endpoint the Windows daemon serves, under the user's own profile.
#[cfg(windows)]
fn endpoint_path() -> std::io::Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "LOCALAPPDATA is not set; the daemon endpoint has nowhere to live",
        )
    })?;
    Ok(PathBuf::from(local)
        .join("tidemark")
        .join("run")
        .join("d.sock"))
}

/// Binds the endpoint, clearing whatever a killed daemon left on it.
///
/// A socket file outlives the process that bound it — Windows unlinks it no more than
/// Unix does — and `bind` over one that is still on disk fails, so a daemon killed
/// outright would otherwise be the last one this user ever starts.
///
/// The leftover cannot be recognised by asking the filesystem about it. `Path::exists`
/// opens the file to answer, and an `AF_UNIX` socket refuses to be opened: on a machine
/// whose socket namespace has been disturbed the open fails outright (`ERROR_CANT_ACCESS_FILE`,
/// os error 1920) and `exists()` reports `false` about a socket plainly listed in its own
/// directory. Gating the cleanup on that answer is what left this daemon binding over a
/// file it had just been told was not there, to fail with a bare `os error 10022` in
/// `daemon.log` and a client waiting forever.
///
/// So the question is put to the socket rather than to the filesystem, and only the one
/// answer that means "live daemon" is honoured: a connection that is *accepted*. Every
/// other outcome — a refusal, a socket whose file is unopenable, no file at all (Windows
/// answers `ConnectionRefused` for a path that does not exist, not `NotFound`) — is a
/// leftover or nothing, and the path is cleared unconditionally before the bind. Removing
/// what is not there is not an error; failing to remove what is there is not fatal on its
/// own, because the bind that follows is the one that decides, but it is the likeliest
/// reason that bind fails and so it is carried into the message.
#[cfg(windows)]
fn take_endpoint(endpoint: &std::path::Path) -> std::io::Result<UnixListener> {
    if UnixStream::connect(endpoint).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!(
                "another tidemarkd already answers at {}; this one is not needed",
                endpoint.display()
            ),
        ));
    }
    let unremoved = match fs::remove_file(endpoint) {
        Ok(()) => {
            tracing::info!(
                endpoint = %endpoint.display(),
                "cleared the socket a previous daemon left behind"
            );
            None
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(error),
    };
    UnixListener::bind(endpoint).map_err(|error| {
        let reason = match unremoved {
            Some(unremoved) => format!(
                "cannot serve {}: {error}; the socket left there could not be removed either: {unremoved}",
                endpoint.display()
            ),
            None => format!("cannot serve {}: {error}", endpoint.display()),
        };
        std::io::Error::new(error.kind(), reason)
    })
}

/// Accepts p2p peers forever, one zbus connection per peer.
///
/// Returns the accept task's handle so shutdown can stop handing out new connections.
/// The blocking listener thread dies with the process; a daemon that stops accepting is
/// a daemon that is about to exit.
#[cfg(windows)]
pub(crate) async fn listen(
    daemon: Daemon,
    hub: Arc<PeerHub>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let endpoint = endpoint_path()?;
    if let Some(parent) = endpoint.parent() {
        fs::create_dir_all(parent)?;
    }
    let listener = take_endpoint(&endpoint)?;

    let (accepted, mut incoming) = mpsc::unbounded_channel();
    thread::Builder::new()
        .name("tidemarkd-ipc-accept".into())
        .spawn(move || {
            while let Ok((stream, _)) = listener.accept() {
                if accepted.send(stream).is_err() {
                    break;
                }
            }
        })?;

    let guid = Guid::generate();
    Ok(tokio::spawn(async move {
        while let Some(stream) = incoming.recv().await {
            tokio::spawn(serve_peer(
                stream,
                guid.clone(),
                daemon.clone(),
                Arc::clone(&hub),
            ));
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own under the temp dir, named for the test and the
    /// process, so a parallel run and a leftover from a previous one cannot collide.
    #[cfg(windows)]
    fn a_directory_for(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tidemark-endpoint-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a directory under the temp dir");
        dir
    }

    /// Whether the directory still lists the file — the one question about a socket the
    /// filesystem answers reliably, and the reason [`take_endpoint`] does not ask
    /// `Path::exists`, which opens the file to answer and can be refused.
    #[cfg(windows)]
    fn listed(path: &std::path::Path) -> bool {
        fs::read_dir(path.parent().expect("the endpoint has a directory"))
            .expect("the directory is readable")
            .flatten()
            .any(|entry| entry.file_name() == path.file_name().expect("the endpoint is a file"))
    }

    #[cfg(windows)]
    #[test]
    fn the_socket_a_killed_daemon_left_behind_does_not_stop_the_next_one() {
        let endpoint = a_directory_for("killed").join("d.sock");
        drop(UnixListener::bind(&endpoint).expect("the first daemon binds"));
        assert!(
            listed(&endpoint),
            "a closed socket still occupies its path; without that this test proves nothing"
        );

        let listener = take_endpoint(&endpoint).expect("the next daemon takes the endpoint over");
        assert!(
            UnixStream::connect(&endpoint).is_ok(),
            "the endpoint the next daemon bound is the one clients reach"
        );
        drop(listener);
    }

    #[cfg(windows)]
    #[test]
    fn an_endpoint_a_live_daemon_answers_is_left_to_it() {
        let endpoint = a_directory_for("live").join("d.sock");
        let live = UnixListener::bind(&endpoint).expect("the running daemon binds");

        let refused = take_endpoint(&endpoint).expect_err("a second daemon must not take it over");
        assert_eq!(refused.kind(), std::io::ErrorKind::AddrInUse);
        assert!(
            UnixStream::connect(&endpoint).is_ok(),
            "the running daemon still answers on the endpoint it owns"
        );
        drop(live);
    }

    /// The real blocker is a socket the filesystem will not let go of, which cannot be
    /// manufactured in a test; a directory in the endpoint's place is the same shape —
    /// something that cannot be removed and cannot be bound over — and it is what the
    /// message has to survive. `daemon.log` used to carry the bare errno and nothing else,
    /// which is a fatal error with no way to act on it.
    #[cfg(windows)]
    #[test]
    fn an_endpoint_that_cannot_be_taken_says_which_one_and_why() {
        let endpoint = a_directory_for("blocked").join("d.sock");
        fs::create_dir(&endpoint).expect("something in the endpoint's place");

        let refused = take_endpoint(&endpoint).expect_err("nothing can be bound there");
        let reason = refused.to_string();
        assert!(
            reason.contains(&endpoint.display().to_string()),
            "the failure must name the endpoint, got {reason}"
        );
        assert!(
            reason.contains("could not be removed"),
            "the failure must say what is in the way, got {reason}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn a_first_run_binds_an_endpoint_nothing_has_ever_used() {
        let endpoint = a_directory_for("first-run").join("d.sock");
        assert!(!listed(&endpoint));

        let listener = take_endpoint(&endpoint).expect("the first daemon of a fresh install binds");
        assert!(UnixStream::connect(&endpoint).is_ok());
        drop(listener);
    }

    /// Whether the announcement would be recognisable to a test, without demanding full
    /// wire equality in the queue-level tests.
    fn label(announcement: &Announcement) -> String {
        match announcement {
            Announcement::UpdateChanged(version) => format!("update:{version}"),
            Announcement::OrderChanged(providers) => format!("order:{}", providers.join(",")),
            Announcement::ProviderChanged(status) => format!("changed:{}", status.provider),
            Announcement::ProviderRemoved { provider, .. } => format!("removed:{provider}"),
            Announcement::PreferencesChanged(_) => "preferences".into(),
            Announcement::DataChanged(_) => "data".into(),
            Announcement::PluginsChanged(plugins) => format!("plugins:{}", plugins.len()),
            Announcement::ActivateRequested => "activate".into(),
        }
    }

    fn announcement(sequence: usize) -> Announcement {
        Announcement::UpdateChanged(format!("1.0.{sequence}"))
    }

    #[tokio::test]
    async fn two_peers_receive_the_same_announcements_in_order() {
        let hub = PeerHub::default();
        let (first_id, mut first) = hub.register();
        let (second_id, mut second) = hub.register();
        assert_ne!(first_id, second_id);

        for sequence in 0..3 {
            hub.publish(announcement(sequence)).await;
        }

        for receiver in [&mut first, &mut second] {
            let mut labels = Vec::new();
            for _ in 0..3 {
                labels.push(label(
                    &receiver.recv().await.expect("the queue keeps its side"),
                ));
            }
            assert_eq!(
                labels,
                vec![
                    "update:1.0.0".to_owned(),
                    "update:1.0.1".to_owned(),
                    "update:1.0.2".to_owned()
                ]
            );
        }
        assert_eq!(hub.peer_count(), 2);
    }

    #[tokio::test]
    async fn an_overflowing_peer_is_evicted_whole() {
        let hub = PeerHub::default();
        let (_id, mut queue) = hub.register();

        // Fill the queue to exactly its bound: all of these must be accepted.
        for sequence in 0..PEER_QUEUE_BOUND {
            hub.publish(announcement(sequence)).await;
        }
        assert_eq!(hub.peer_count(), 1);

        // One past the bound: the whole peer goes, no coalescing, no drop-oldest.
        hub.publish(announcement(PEER_QUEUE_BOUND)).await;
        assert_eq!(hub.peer_count(), 0);

        // The 128 queued announcements stay theirs — unread, about to be disconnected —
        // and the hub is clean for the next peer.
        assert!(matches!(
            queue.recv().await,
            Some(Announcement::UpdateChanged(version)) if version == "1.0.0"
        ));
        hub.publish(announcement(999)).await;
        assert_eq!(hub.peer_count(), 0, "the evicted peer is not re-registered");
    }

    #[tokio::test]
    async fn a_registration_that_never_attaches_is_dropped_cleanly() {
        let hub = PeerHub::default();
        let (id, mut queue) = hub.register();
        hub.forget(id);

        hub.publish(announcement(0)).await;

        assert_eq!(hub.peer_count(), 0);
        // Dropping the registration closed the queue: the deliverer's `recv` yields
        // `None`, which is what ends its task.
        assert!(queue.recv().await.is_none());
    }

    #[tokio::test]
    async fn a_peer_whose_deliverer_is_gone_is_evicted_on_the_next_publish() {
        let hub = PeerHub::default();
        let (_id, queue) = hub.register();
        drop(queue);

        hub.publish(announcement(0)).await;

        assert_eq!(
            hub.peer_count(),
            0,
            "a closed queue is an eviction, not an error"
        );
    }

    // The loopback test runs a real p2p server connection per the A1 contract —
    // Unix-stream transport, p2p, well-known name, bounded queue, served interface — and
    // a real client against it. uds_windows has no `pair`, so this stays on unix where
    // CI runs it; Windows covers the same path in the manual QA gate.
    #[cfg(all(test, unix))]
    mod loopback {
        use super::*;

        use std::future::poll_fn;
        use std::pin::Pin;

        use tidemark_core::providers::BoxFuture;
        use tidemark_core::providers::Credential;
        use tidemark_core::secrets::{Kind, SecretError};
        use tidemark_types::{AccountId, ProviderId, ids};
        use zbus::MessageStream;
        use zbus::export::futures_core::Stream;

        use crate::engine::Command;
        use crate::service::{Published, PublishedUpdate};

        #[derive(Debug)]
        struct FakeSecrets;

        impl tidemark_core::secrets::Secrets for FakeSecrets {
            fn get<'a>(
                &'a self,
                _kind: Kind,
                _provider: &'a ProviderId,
                _account: &'a AccountId,
            ) -> BoxFuture<'a, Result<Option<Credential>, SecretError>> {
                Box::pin(async { Ok(None) })
            }

            fn set<'a>(
                &'a self,
                _kind: Kind,
                _provider: &'a ProviderId,
                _account: &'a AccountId,
                _secret: &'a Credential,
            ) -> BoxFuture<'a, Result<(), SecretError>> {
                Box::pin(async { Ok(()) })
            }

            fn compare_and_set<'a>(
                &'a self,
                _kind: Kind,
                _provider: &'a ProviderId,
                _account: &'a AccountId,
                _expected: &'a Credential,
                _replacement: &'a Credential,
            ) -> BoxFuture<'a, Result<bool, SecretError>> {
                Box::pin(async { Ok(false) })
            }

            fn delete<'a>(
                &'a self,
                _kind: Kind,
                _provider: &'a ProviderId,
                _account: &'a AccountId,
            ) -> BoxFuture<'a, Result<(), SecretError>> {
                Box::pin(async { Ok(()) })
            }
        }

        fn daemon() -> Daemon {
            Daemon::new(
                Published::default(),
                PublishedUpdate::default(),
                Vec::new(),
                Vec::new(),
                mpsc::channel::<Command>(4).0,
                Arc::new(FakeSecrets),
            )
        }

        const BOUNDED: std::time::Duration = std::time::Duration::from_secs(10);

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn a_p2p_client_is_served_signals_methods_and_properties() {
            let hub = Arc::new(PeerHub::default());
            let (client_stream, server_stream) =
                std::os::unix::net::UnixStream::pair().expect("a connected socket pair");

            let served = tokio::spawn(serve_peer(
                server_stream,
                Guid::generate(),
                daemon().with_hub(Arc::clone(&hub)),
                Arc::clone(&hub),
            ));

            let client = zbus::connection::Builder::async_io_unix_stream(client_stream)
                .p2p()
                .max_queued(ZBUS_QUEUE_BOUND)
                .build()
                .await
                .expect("the client connects");

            // The property and method round trip a real client's readiness probe makes.
            let proxy = zbus::Proxy::new(
                &client,
                ids::DAEMON_BUS_NAME,
                ids::OBJECT_PATH,
                ids::DAEMON_INTERFACE,
            )
            .await
            .expect("the proxy builds");
            let version: String = proxy
                .get_property("Version")
                .await
                .expect("Version answers over p2p");
            assert_eq!(version, env!("CARGO_PKG_VERSION"));

            let reply = client
                .call_method(
                    Some(ids::DAEMON_BUS_NAME),
                    ids::OBJECT_PATH,
                    Some(ids::DAEMON_INTERFACE),
                    "GetStatus",
                    &(),
                )
                .await
                .expect("GetStatus answers over p2p");
            let statuses: Vec<ProviderStatus> = reply.body().deserialize().expect("the shape");
            assert!(statuses.is_empty());

            // Fan-out through the hub reaches the peer, in order, on its own connection.
            let mut messages = MessageStream::from(&client);
            hub.publish(Announcement::UpdateChanged("9.9.0".into()))
                .await;
            hub.publish(Announcement::OrderChanged(vec![
                "zai".into(),
                "codex".into(),
            ]))
            .await;

            // Read until both signals have been seen, rather than reading exactly two
            // messages: the stream was built after the property get and the method call
            // above, and zbus hands a new stream whatever is still sitting in the
            // connection's broadcast queue. Whether either reply is still there when the
            // stream is constructed is a matter of timing, so counting messages instead
            // of signals loses a signal to a leftover method return often enough to fail
            // a run. Each poll stays under `BOUNDED`, so a signal that never comes still
            // fails the test rather than hanging it.
            let mut members = Vec::new();
            while members.len() < 2 {
                let message = tokio::time::timeout(
                    BOUNDED,
                    poll_fn(|cx| Stream::poll_next(Pin::new(&mut messages), cx)),
                )
                .await
                .expect("the signal arrives on its own")
                .expect("the stream stays alive")
                .expect("the message is well formed");
                if message.header().message_type() == zbus::message::Type::Signal {
                    members.push(
                        message
                            .header()
                            .member()
                            .expect("a signal has a member")
                            .as_str()
                            .to_owned(),
                    );
                }
            }
            assert_eq!(members, vec!["UpdateChanged", "OrderChanged"]);

            // The client hanging up is the whole story of a peer leaving: the served
            // task completing is the deterministic signal that `connection.closed()`
            // fired and the hub forgot the peer.
            drop(proxy);
            drop(client);
            // The message stream owns a full `Connection` clone: without this drop the
            // client's socket stays open and the hangup below never happens.
            drop(messages);
            tokio::time::timeout(BOUNDED, served)
                .await
                .expect("the served task ends when the client hangs up")
                .expect("the served task did not panic");
            assert_eq!(hub.peer_count(), 0, "the disconnected peer is forgotten");
        }
    }
}
