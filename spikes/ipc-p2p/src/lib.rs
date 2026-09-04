//! Decision-bearing zbus peer-to-peer transport spike.
//!
//! The crate uses a real pathname AF_UNIX listener, a generated proxy with Tidemark's shipped
//! identity, and the exact `tidemark-types` values carried by the daemon today. It is not linked
//! into any product crate.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::thread;
use std::time::Duration;

use fs4::TryLockError;
use futures_util::StreamExt as _;
use thiserror::Error;
use tidemark_types::{AccountId, DataInfo, Preferences, ProviderId, ProviderStatus, ids};
use tokio::sync::{Mutex, RwLock, mpsc, oneshot, watch};
use zbus::object_server::SignalEmitter;
use zbus::proxy::CacheProperties;
use zbus::{Connection, Guid};

#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(windows)]
use uds_windows::{UnixListener, UnixStream};

/// The bounded publication queue required for every daemon peer.
pub const PEER_QUEUE_BOUND: usize = 128;
/// zbus's unfiltered connection queue from the frozen A1 builder contract.
pub const ZBUS_QUEUE_BOUND: usize = 64;
/// A bounded failure guard, never a source of test progress.
pub const GATE_TIMEOUT: Duration = Duration::from_secs(10);

/// Failures surfaced by the spike rather than converted into apparent success.
#[derive(Debug, Error)]
pub enum SpikeError {
    #[error("AF_UNIX I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("zbus p2p failed: {0}")]
    Zbus(#[from] zbus::Error),
    #[error("the endpoint singleton is already held: {0}")]
    EndpointBusy(PathBuf),
    #[error("IPC setup failed: {0}")]
    Setup(String),
    #[error("IPC synchronization failed: {0}")]
    Synchronization(&'static str),
    #[error("signal delivery failed: {0}")]
    Delivery(String),
    #[error("a bounded gate timed out while waiting for {0}")]
    Timeout(&'static str),
    #[error("a transport task failed: {0}")]
    Task(String),
}

pub type Result<T> = std::result::Result<T, SpikeError>;

/// The generated client contract uses the same public service, path, and interface as Tidemark.
#[zbus::proxy(
    interface = "io.github.zbndev.Tidemark.Daemon1",
    default_service = "io.github.zbndev.Tidemark.Daemon",
    default_path = "/io/github/zbndev/Tidemark"
)]
pub trait Daemon {
    /// Every account the spike server currently publishes.
    fn get_status(&self) -> zbus::Result<Vec<ProviderStatus>>;

    /// A deterministic test handshake. It is not part of the product interface.
    fn hello(&self, label: &str, hold_delivery: bool) -> zbus::Result<()>;

    /// A same-connection ordering fence. It returns this peer's completed deliveries.
    fn fence(&self) -> zbus::Result<u64>;

    /// The daemon version property used by the real client readiness probe.
    #[zbus(property(emits_changed_signal = "false"))]
    fn version(&self) -> zbus::Result<String>;

    /// One account changed.
    #[zbus(signal)]
    fn provider_changed(&self, status: ProviderStatus) -> zbus::Result<()>;

    /// One configured account was removed.
    #[zbus(signal)]
    fn provider_removed(&self, provider: &str, account: &str) -> zbus::Result<()>;

    /// The configured providers are now in this order.
    #[zbus(signal)]
    fn order_changed(&self, providers: Vec<String>) -> zbus::Result<()>;

    /// Application preferences changed.
    #[zbus(signal)]
    fn preferences_changed(&self, preferences: Preferences) -> zbus::Result<()>;

    /// Paths or storage facts changed.
    #[zbus(signal)]
    fn data_changed(&self, data: DataInfo) -> zbus::Result<()>;

    /// Availability of a newer published release changed.
    #[zbus(signal)]
    fn update_changed(&self, version: &str) -> zbus::Result<()>;
}

/// A connected generated proxy and its underlying p2p connection.
#[derive(Debug)]
pub struct Client {
    connection: Connection,
    proxy: DaemonProxy<'static>,
}

impl Client {
    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn proxy(&self) -> &DaemonProxy<'static> {
        &self.proxy
    }

    /// Registers this test client only after its signal subscriptions are in place.
    pub async fn hello(&self, label: &str, hold_delivery: bool) -> Result<()> {
        self.proxy.hello(label, hold_delivery).await?;
        Ok(())
    }

    /// Subscribes to the six generated proxy signals as one ordered stream.
    pub async fn signals(&self) -> Result<zbus::proxy::SignalStream<'static>> {
        Ok(self.proxy.inner().receive_all_signals().await?)
    }
}

/// Connects over a real pathname AF_UNIX stream and builds the generated daemon proxy.
pub async fn connect(endpoint: &Path) -> Result<Client> {
    let stream = UnixStream::connect(endpoint)?;
    let connection = zbus::connection::Builder::async_io_unix_stream(stream)
        .p2p()
        .max_queued(ZBUS_QUEUE_BOUND)
        .method_timeout(GATE_TIMEOUT)
        .build()
        .await?;
    let proxy = DaemonProxy::builder(&connection)
        .destination(ids::DAEMON_BUS_NAME)?
        .path(ids::OBJECT_PATH)?
        .cache_properties(CacheProperties::No)
        .build()
        .await?;
    Ok(Client { connection, proxy })
}

/// Runs one future with a bounded failure guard.
pub async fn bounded<T>(what: &'static str, future: impl Future<Output = T>) -> Result<T> {
    tokio::time::timeout(GATE_TIMEOUT, future)
        .await
        .map_err(|_| SpikeError::Timeout(what))
}

/// Returns the endpoint the Windows daemon contract fixes under LocalAppData.
pub fn default_endpoint() -> Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| SpikeError::Setup("LOCALAPPDATA is not set".into()))?;
    Ok(PathBuf::from(local)
        .join("tidemark")
        .join("run")
        .join("d.sock"))
}

/// Returns the current Windows user SID used in endpoint ACL receipts.
#[cfg(windows)]
pub fn current_user_sid() -> Result<String> {
    let output = std::process::Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()?;
    if !output.status.success() {
        return Err(SpikeError::Setup(format!(
            "whoami.exe /user failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let row = String::from_utf8(output.stdout)
        .map_err(|_| SpikeError::Setup("whoami.exe returned non-UTF-8 output".into()))?;
    row.trim()
        .trim_matches('"')
        .rsplit_once("\",\"")
        .map(|(_, sid)| sid.trim_matches('"').to_owned())
        .filter(|sid| sid.starts_with("S-1-"))
        .ok_or_else(|| SpikeError::Setup("whoami.exe returned a malformed SID".into()))
}

#[cfg(unix)]
pub fn current_user_sid() -> Result<String> {
    Ok(format!("uid-{}", unsafe { libc::geteuid() }))
}

/// One of the six product signals, decoded from the real wire message.
#[derive(Clone, Debug, PartialEq)]
pub enum ObservedSignal {
    ProviderChanged(ProviderStatus),
    ProviderRemoved(String, String),
    OrderChanged(Vec<String>),
    PreferencesChanged(Preferences),
    DataChanged(DataInfo),
    UpdateChanged(String),
}

/// Parses one signal exactly as a generic Tidemark client would parse it.
pub fn observed_signal(message: &zbus::Message) -> Result<ObservedSignal> {
    let header = message.header();
    let member = header
        .member()
        .ok_or_else(|| SpikeError::Delivery("signal has no member name".into()))?;
    match member.as_str() {
        "ProviderChanged" => Ok(ObservedSignal::ProviderChanged(
            message.body().deserialize::<ProviderStatus>()?,
        )),
        "ProviderRemoved" => {
            let (provider, account) = message.body().deserialize::<(String, String)>()?;
            Ok(ObservedSignal::ProviderRemoved(provider, account))
        }
        "OrderChanged" => Ok(ObservedSignal::OrderChanged(
            message.body().deserialize::<Vec<String>>()?,
        )),
        "PreferencesChanged" => Ok(ObservedSignal::PreferencesChanged(
            message.body().deserialize::<Preferences>()?,
        )),
        "DataChanged" => Ok(ObservedSignal::DataChanged(
            message.body().deserialize::<DataInfo>()?,
        )),
        "UpdateChanged" => Ok(ObservedSignal::UpdateChanged(
            message.body().deserialize::<String>()?,
        )),
        other => Err(SpikeError::Delivery(format!(
            "unexpected signal member {other}"
        ))),
    }
}

/// Reads exactly `count` ordered messages from one generated proxy signal stream.
pub async fn read_signals(
    stream: &mut zbus::proxy::SignalStream<'static>,
    count: usize,
) -> Result<Vec<ObservedSignal>> {
    let mut received = Vec::with_capacity(count);
    for _ in 0..count {
        let message = bounded("the next proxy signal", stream.next())
            .await?
            .ok_or(SpikeError::Synchronization(
                "the proxy signal stream closed early",
            ))?;
        received.push(observed_signal(&message)?);
    }
    Ok(received)
}

/// The six concrete values used by both the emitter and assertions.
pub fn expected_contract(epoch: u32) -> Vec<ObservedSignal> {
    contract_announcements(epoch)
        .into_iter()
        .map(Announcement::observed)
        .collect()
}

fn status(provider: &str, captured_at: Option<i64>) -> ProviderStatus {
    let mut status = ProviderStatus::pending(&ProviderId::new(provider), &AccountId::default());
    status.captured_at = captured_at;
    status
}

#[derive(Clone, Debug)]
enum Announcement {
    ProviderChanged(ProviderStatus),
    ProviderRemoved(String, String),
    OrderChanged(Vec<String>),
    PreferencesChanged(Preferences),
    DataChanged(DataInfo),
    UpdateChanged(String),
}

impl Announcement {
    fn observed(self) -> ObservedSignal {
        match self {
            Self::ProviderChanged(value) => ObservedSignal::ProviderChanged(value),
            Self::ProviderRemoved(provider, account) => {
                ObservedSignal::ProviderRemoved(provider, account)
            }
            Self::OrderChanged(value) => ObservedSignal::OrderChanged(value),
            Self::PreferencesChanged(value) => ObservedSignal::PreferencesChanged(value),
            Self::DataChanged(value) => ObservedSignal::DataChanged(value),
            Self::UpdateChanged(value) => ObservedSignal::UpdateChanged(value),
        }
    }

    async fn emit(self, emitter: &SignalEmitter<'_>) -> zbus::Result<()> {
        match self {
            Self::ProviderChanged(status) => DaemonService::provider_changed(emitter, status).await,
            Self::ProviderRemoved(provider, account) => {
                DaemonService::provider_removed(emitter, &provider, &account).await
            }
            Self::OrderChanged(providers) => DaemonService::order_changed(emitter, providers).await,
            Self::PreferencesChanged(preferences) => {
                DaemonService::preferences_changed(emitter, preferences).await
            }
            Self::DataChanged(data) => DaemonService::data_changed(emitter, data).await,
            Self::UpdateChanged(version) => DaemonService::update_changed(emitter, &version).await,
        }
    }
}

fn contract_announcements(epoch: u32) -> Vec<Announcement> {
    let captured_at = 1_800_000_000_i64 + i64::from(epoch);
    let mut preferences = Preferences::default();
    preferences.refresh_minutes = 10 + epoch;
    vec![
        Announcement::ProviderChanged(status("zai", Some(captured_at))),
        Announcement::ProviderRemoved("codex".into(), "default".into()),
        Announcement::OrderChanged(vec!["zai".into(), "claude".into()]),
        Announcement::PreferencesChanged(preferences),
        Announcement::DataChanged(DataInfo {
            config_path: format!(r"C:\Users\wire\config-{epoch}.toml"),
            history_path: format!(r"C:\Users\wire\history-{epoch}.db"),
            history_bytes: 4096 + u64::from(epoch),
            key_schema: ids::SECRET_SCHEMA.into(),
            token_schema: ids::TOKEN_SCHEMA.into(),
            release_check_available: true,
        }),
        Announcement::UpdateChanged(format!("0.4.{epoch}")),
    ]
}

#[derive(Debug)]
struct QueuedAnnouncement {
    announcement: Announcement,
    acknowledged: Option<oneshot::Sender<std::result::Result<(), String>>>,
}

#[derive(Debug)]
struct Peer {
    id: u64,
    queue: mpsc::Sender<QueuedAnnouncement>,
    connection: OnceLock<Connection>,
    ready: watch::Sender<bool>,
    delivery_enabled: watch::Sender<bool>,
    label: StdMutex<Option<String>>,
    delivered: AtomicUsize,
}

impl Peer {
    fn label(&self) -> String {
        self.label
            .lock()
            .expect("no code panics while holding a peer label")
            .clone()
            .unwrap_or_else(|| format!("peer-{}", self.id))
    }
}

#[derive(Debug, Default)]
struct Hub {
    peers: StdMutex<BTreeMap<u64, Arc<Peer>>>,
}

#[derive(Debug)]
struct PeerRegistration {
    peer: Arc<Peer>,
    queue: mpsc::Receiver<QueuedAnnouncement>,
    delivery_enabled: watch::Receiver<bool>,
}

/// Result of one non-blocking fan-out attempt.
#[derive(Debug, PartialEq, Eq)]
pub struct PublishOutcome {
    pub accepted: usize,
    pub evicted: Vec<String>,
}

impl Hub {
    fn register(&self, id: u64) -> PeerRegistration {
        let (queue, receiver) = mpsc::channel(PEER_QUEUE_BOUND);
        let (ready, _) = watch::channel(false);
        let (delivery_enabled, delivery_receiver) = watch::channel(false);
        let peer = Arc::new(Peer {
            id,
            queue,
            connection: OnceLock::new(),
            ready,
            delivery_enabled,
            label: StdMutex::new(None),
            delivered: AtomicUsize::new(0),
        });
        self.peers
            .lock()
            .expect("no code panics while holding the peer hub")
            .insert(id, Arc::clone(&peer));
        PeerRegistration {
            peer,
            queue: receiver,
            delivery_enabled: delivery_receiver,
        }
    }

    async fn configure(&self, id: u64, label: &str, hold_delivery: bool) -> Result<()> {
        let peer = self.peer(id)?;
        let mut ready = peer.ready.subscribe();
        ready
            .wait_for(|value| *value)
            .await
            .map_err(|_| SpikeError::Synchronization("peer activation was cancelled"))?;

        let duplicate = self
            .peers
            .lock()
            .expect("no code panics while holding the peer hub")
            .values()
            .any(|candidate| candidate.id != id && candidate.label() == label);
        if duplicate {
            return Err(SpikeError::Setup(format!(
                "duplicate client label {label:?}"
            )));
        }
        *peer
            .label
            .lock()
            .expect("no code panics while holding a peer label") = Some(label.to_owned());
        if !hold_delivery {
            peer.delivery_enabled.send_replace(true);
        }
        Ok(())
    }

    fn peer(&self, id: u64) -> Result<Arc<Peer>> {
        self.peers
            .lock()
            .expect("no code panics while holding the peer hub")
            .get(&id)
            .cloned()
            .ok_or(SpikeError::Synchronization("peer registration disappeared"))
    }

    fn remove(&self, id: u64) {
        self.peers
            .lock()
            .expect("no code panics while holding the peer hub")
            .remove(&id);
    }

    fn peer_count(&self) -> usize {
        self.peers
            .lock()
            .expect("no code panics while holding the peer hub")
            .len()
    }

    fn queue_remaining(&self, label: &str) -> Result<usize> {
        self.peers
            .lock()
            .expect("no code panics while holding the peer hub")
            .values()
            .find(|peer| peer.label() == label)
            .map(|peer| peer.queue.capacity())
            .ok_or(SpikeError::Synchronization("named peer is not registered"))
    }

    fn delivery_counts(&self) -> BTreeMap<String, usize> {
        self.peers
            .lock()
            .expect("no code panics while holding the peer hub")
            .values()
            .map(|peer| (peer.label(), peer.delivered.load(Ordering::Acquire)))
            .collect()
    }

    async fn publish_and_wait(&self, announcement: Announcement) -> Result<()> {
        let mut acknowledgements = Vec::new();
        let mut evicted = Vec::new();
        {
            let mut peers = self
                .peers
                .lock()
                .expect("no code panics while holding the peer hub");
            let ids: Vec<u64> = peers.keys().copied().collect();
            for id in ids {
                let peer = Arc::clone(peers.get(&id).expect("peer id came from this map"));
                let (acknowledged, received) = oneshot::channel();
                let queued = QueuedAnnouncement {
                    announcement: announcement.clone(),
                    acknowledged: Some(acknowledged),
                };
                match peer.queue.try_send(queued) {
                    Ok(()) => acknowledgements.push(received),
                    Err(
                        mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_),
                    ) => {
                        peers.remove(&id);
                        evicted.push(peer);
                    }
                }
            }
        }
        close_peers(&evicted).await?;
        if !evicted.is_empty() {
            return Err(SpikeError::Delivery(format!(
                "{} peer(s) were evicted during acknowledged publication",
                evicted.len()
            )));
        }
        for acknowledgement in acknowledgements {
            acknowledgement
                .await
                .map_err(|_| SpikeError::Synchronization("signal worker dropped its receipt"))?
                .map_err(SpikeError::Delivery)?;
        }
        Ok(())
    }

    async fn try_publish(&self, announcement: Announcement) -> Result<PublishOutcome> {
        let mut accepted = 0;
        let mut evicted = Vec::new();
        {
            let mut peers = self
                .peers
                .lock()
                .expect("no code panics while holding the peer hub");
            let ids: Vec<u64> = peers.keys().copied().collect();
            for id in ids {
                let peer = Arc::clone(peers.get(&id).expect("peer id came from this map"));
                let queued = QueuedAnnouncement {
                    announcement: announcement.clone(),
                    acknowledged: None,
                };
                match peer.queue.try_send(queued) {
                    Ok(()) => accepted += 1,
                    Err(
                        mpsc::error::TrySendError::Full(_) | mpsc::error::TrySendError::Closed(_),
                    ) => {
                        peers.remove(&id);
                        evicted.push(peer);
                    }
                }
            }
        }
        let labels = evicted.iter().map(|peer| peer.label()).collect();
        close_peers(&evicted).await?;
        Ok(PublishOutcome {
            accepted,
            evicted: labels,
        })
    }

    async fn close_all(&self) -> Result<()> {
        let peers: Vec<_> = {
            let mut peers = self
                .peers
                .lock()
                .expect("no code panics while holding the peer hub");
            std::mem::take(&mut *peers).into_values().collect()
        };
        close_peers(&peers).await
    }
}

async fn close_peers(peers: &[Arc<Peer>]) -> Result<()> {
    let mut first_error = None;
    for peer in peers {
        if let Some(connection) = peer.connection.get()
            && let Err(error) = connection.clone().close().await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    match first_error {
        Some(error) => Err(SpikeError::Zbus(error)),
        None => Ok(()),
    }
}

#[derive(Debug)]
struct RaceHook {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}

#[derive(Debug)]
struct ServerState {
    version: String,
    statuses: RwLock<Vec<ProviderStatus>>,
    race: Mutex<Option<RaceHook>>,
    hub: Hub,
    stopping: watch::Sender<bool>,
}

#[derive(Clone, Debug)]
struct DaemonService {
    state: Arc<ServerState>,
    peer_id: u64,
}

#[zbus::interface(name = "io.github.zbndev.Tidemark.Daemon1")]
impl DaemonService {
    async fn get_status(&self) -> zbus::fdo::Result<Vec<ProviderStatus>> {
        let snapshot = self.state.statuses.read().await.clone();
        if let Some(hook) = self.state.race.lock().await.take() {
            hook.entered.send(()).map_err(|()| {
                zbus::fdo::Error::Failed("GetStatus race observer disappeared".into())
            })?;
            hook.release.await.map_err(|_| {
                zbus::fdo::Error::Failed("GetStatus race release disappeared".into())
            })?;
        }
        Ok(snapshot)
    }

    async fn hello(&self, label: &str, hold_delivery: bool) -> zbus::fdo::Result<()> {
        self.state
            .hub
            .configure(self.peer_id, label, hold_delivery)
            .await
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    fn fence(&self) -> zbus::fdo::Result<u64> {
        self.state
            .hub
            .peer(self.peer_id)
            .map(|peer| peer.delivered.load(Ordering::Acquire) as u64)
            .map_err(|error| zbus::fdo::Error::Failed(error.to_string()))
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn version(&self) -> String {
        self.state.version.clone()
    }

    #[zbus(signal)]
    async fn provider_changed(
        emitter: &SignalEmitter<'_>,
        status: ProviderStatus,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn provider_removed(
        emitter: &SignalEmitter<'_>,
        provider: &str,
        account: &str,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn order_changed(emitter: &SignalEmitter<'_>, providers: Vec<String>)
    -> zbus::Result<()>;

    #[zbus(signal)]
    async fn preferences_changed(
        emitter: &SignalEmitter<'_>,
        preferences: Preferences,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn data_changed(emitter: &SignalEmitter<'_>, data: DataInfo) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn update_changed(emitter: &SignalEmitter<'_>, version: &str) -> zbus::Result<()>;
}

/// Handle used by deterministic tests to mutate state and publish after exact barriers.
#[derive(Clone, Debug)]
pub struct ServerHandle {
    state: Arc<ServerState>,
}

impl ServerHandle {
    pub async fn publish_contract(&self, epoch: u32) -> Result<()> {
        for announcement in contract_announcements(epoch) {
            self.state.hub.publish_and_wait(announcement).await?;
        }
        Ok(())
    }

    /// Commits status before publishing, matching the daemon's state-before-signal invariant.
    pub async fn replace_status_and_publish(&self, status: ProviderStatus) -> Result<()> {
        *self.state.statuses.write().await = vec![status.clone()];
        self.state
            .hub
            .publish_and_wait(Announcement::ProviderChanged(status))
            .await
    }

    pub async fn arm_get_status_race(&self) -> Result<RaceControl> {
        let (entered, entered_receiver) = oneshot::channel();
        let (release, release_receiver) = oneshot::channel();
        let mut race = self.state.race.lock().await;
        if race.is_some() {
            return Err(SpikeError::Setup(
                "a GetStatus race is already armed".into(),
            ));
        }
        *race = Some(RaceHook {
            entered,
            release: release_receiver,
        });
        Ok(RaceControl {
            entered: entered_receiver,
            release: Some(release),
        })
    }

    pub async fn publish_unacknowledged_update(&self, sequence: u32) -> Result<PublishOutcome> {
        self.state
            .hub
            .try_publish(Announcement::UpdateChanged(format!("queue-{sequence}")))
            .await
    }

    pub fn peer_count(&self) -> usize {
        self.state.hub.peer_count()
    }

    pub fn queue_remaining(&self, label: &str) -> Result<usize> {
        self.state.hub.queue_remaining(label)
    }

    pub fn delivery_counts(&self) -> BTreeMap<String, usize> {
        self.state.hub.delivery_counts()
    }
}

/// Exact two-phase barrier for the subscribe-before-GetStatus race gate.
#[derive(Debug)]
pub struct RaceControl {
    entered: oneshot::Receiver<()>,
    release: Option<oneshot::Sender<()>>,
}

impl RaceControl {
    pub async fn wait_until_entered(&mut self) -> Result<()> {
        bounded("GetStatus to enter the armed race", &mut self.entered)
            .await?
            .map_err(|_| SpikeError::Synchronization("GetStatus never entered its race hook"))
    }

    pub fn release(mut self) -> Result<()> {
        self.release
            .take()
            .expect("RaceControl releases at most once")
            .send(())
            .map_err(|()| SpikeError::Synchronization("GetStatus stopped before race release"))
    }
}

/// A live listener and all server-side peer connections.
#[derive(Debug)]
pub struct RunningServer {
    endpoint: PathBuf,
    lock_path: PathBuf,
    lease: Option<EndpointLease>,
    shutdown: Option<oneshot::Sender<()>>,
    accept_thread: Option<thread::JoinHandle<()>>,
    server_task: Option<tokio::task::JoinHandle<Result<()>>>,
    handle: ServerHandle,
    stopped: bool,
}

impl RunningServer {
    /// Acquires the singleton before stale cleanup, applies the user/SYSTEM DACL, then binds.
    pub fn start(
        endpoint: impl Into<PathBuf>,
        version: impl Into<String>,
        statuses: Vec<ProviderStatus>,
    ) -> Result<Self> {
        let endpoint = endpoint.into();
        let (lease, lock_path) = EndpointLease::acquire(&endpoint)?;
        if endpoint.exists() {
            fs::remove_file(&endpoint)?;
        }
        let listener = UnixListener::bind(&endpoint)?;
        let (accepted, mut incoming) = mpsc::unbounded_channel();
        let accept_thread = thread::Builder::new()
            .name("ipc-p2p-af-unix-accept".into())
            .spawn(move || {
                while let Ok((stream, _)) = listener.accept() {
                    if accepted.send(stream).is_err() {
                        break;
                    }
                }
            })?;

        let state = Arc::new(ServerState {
            version: version.into(),
            statuses: RwLock::new(statuses),
            race: Mutex::new(None),
            hub: Hub::default(),
            stopping: watch::channel(false).0,
        });
        let handle = ServerHandle {
            state: Arc::clone(&state),
        };
        let guid = Guid::generate();
        let next_peer = Arc::new(AtomicU64::new(1));
        let endpoint_for_wake = endpoint.clone();
        let (shutdown, mut stopping) = oneshot::channel();
        let server_task = tokio::spawn(async move {
            let mut peers = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stopping => {
                        state.stopping.send_replace(true);
                        break;
                    }
                    stream = incoming.recv() => match stream {
                        Some(stream) => {
                            let id = next_peer.fetch_add(1, Ordering::Relaxed);
                            let state = Arc::clone(&state);
                            let guid = guid.clone();
                            peers.spawn(async move { serve_peer(stream, state, guid, id).await });
                        }
                        None => break,
                    },
                    finished = peers.join_next(), if !peers.is_empty() => {
                        match finished {
                            Some(Ok(Ok(()))) => {}
                            Some(Ok(Err(error))) => return Err(error),
                            Some(Err(error)) => return Err(SpikeError::Task(error.to_string())),
                            None => {}
                        }
                    }
                }
            }
            // Deterministic accept-thread exit: drop the receiver first so the next
            // accepted wake's send fails, then force exactly one more accept to return.
            drop(incoming);
            let _ = UnixStream::connect(&endpoint_for_wake);
            state.hub.close_all().await?;
            while let Some(finished) = peers.join_next().await {
                match finished {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => return Err(error),
                    Err(error) => return Err(SpikeError::Task(error.to_string())),
                }
            }
            Ok(())
        });

        Ok(Self {
            endpoint,
            lock_path,
            lease: Some(lease),
            shutdown: Some(shutdown),
            accept_thread: Some(accept_thread),
            server_task: Some(server_task),
            handle,
            stopped: false,
        })
    }

    pub fn handle(&self) -> ServerHandle {
        self.handle.clone()
    }

    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }

    /// Stops every peer, wakes and joins the blocking accept thread, then removes all artifacts.
    pub async fn shutdown(mut self) -> Result<()> {
        self.stop().await
    }

    async fn stop(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }

        // A connection is the exact wakeup event for the blocking accept call. It is dropped
        // immediately and never participates in a zbus handshake.
        let wake = UnixStream::connect(&self.endpoint)?;
        drop(wake);

        if let Some(accept_thread) = self.accept_thread.take() {
            tokio::task::spawn_blocking(move || accept_thread.join())
                .await
                .map_err(|error| SpikeError::Task(error.to_string()))?
                .map_err(|_| SpikeError::Task("AF_UNIX accept thread panicked".into()))?;
        }
        if let Some(server_task) = self.server_task.take() {
            bounded("server task shutdown", server_task)
                .await?
                .map_err(|error| SpikeError::Task(error.to_string()))??;
        }
        match fs::remove_file(&self.endpoint) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        drop(self.lease.take());
        if self.lock_path.exists() {
            fs::remove_file(&self.lock_path)?;
        }
        self.stopped = true;
        Ok(())
    }
}

impl Drop for RunningServer {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let _ = UnixStream::connect(&self.endpoint);
        if let Some(accept_thread) = self.accept_thread.take() {
            let _ = accept_thread.join();
        }
        if let Some(server_task) = self.server_task.take() {
            server_task.abort();
        }
        let _ = fs::remove_file(&self.endpoint);
        drop(self.lease.take());
        let _ = fs::remove_file(&self.lock_path);
    }
}

async fn serve_peer(
    stream: UnixStream,
    state: Arc<ServerState>,
    guid: Guid<'static>,
    peer_id: u64,
) -> Result<()> {
    let registration = state.hub.register(peer_id);
    let service = DaemonService {
        state: Arc::clone(&state),
        peer_id,
    };
    let connection = match zbus::connection::Builder::async_io_unix_stream(stream)
        .server(guid)
        .map(|builder| builder.p2p())
        .and_then(|builder| builder.name(ids::DAEMON_BUS_NAME))
        .map(|builder| builder.max_queued(ZBUS_QUEUE_BOUND))
        .and_then(|builder| builder.serve_at(ids::OBJECT_PATH, service))
    {
        Ok(builder) => match builder.build().await {
            Ok(connection) => connection,
            Err(error) => {
                state.hub.remove(peer_id);
                return Err(error.into());
            }
        },
        Err(error) => {
            state.hub.remove(peer_id);
            return Err(error.into());
        }
    };

    registration
        .peer
        .connection
        .set(connection.clone())
        .map_err(|_| SpikeError::Synchronization("peer connection was activated twice"))?;
    tokio::spawn(deliver(
        Arc::clone(&registration.peer),
        registration.queue,
        registration.delivery_enabled,
    ));
    registration.peer.ready.send_replace(true);

    let mut server_stopping = state.stopping.subscribe();
    tokio::select! {
        _ = connection.closed() => {}
        _ = server_stopping.wait_for(|stopping| *stopping) => {}
    }
    state.hub.remove(peer_id);
    Ok(())
}

async fn deliver(
    peer: Arc<Peer>,
    mut queue: mpsc::Receiver<QueuedAnnouncement>,
    mut delivery_enabled: watch::Receiver<bool>,
) {
    if delivery_enabled.wait_for(|enabled| *enabled).await.is_err() {
        return;
    }
    let Some(connection) = peer.connection.get() else {
        return;
    };
    let emitter = match SignalEmitter::new(connection, ids::OBJECT_PATH) {
        Ok(emitter) => emitter,
        Err(error) => {
            while let Some(queued) = queue.recv().await {
                if let Some(acknowledged) = queued.acknowledged {
                    let _ = acknowledged.send(Err(error.to_string()));
                }
            }
            return;
        }
    };
    while let Some(queued) = queue.recv().await {
        let result = queued
            .announcement
            .emit(&emitter)
            .await
            .map_err(|error| error.to_string());
        if result.is_ok() {
            peer.delivered.fetch_add(1, Ordering::Release);
        }
        if let Some(acknowledged) = queued.acknowledged {
            let _ = acknowledged.send(result.clone());
        }
        if result.is_err() {
            break;
        }
    }
}

#[derive(Debug)]
struct EndpointLease {
    file: File,
    lock_path: PathBuf,
}

impl EndpointLease {
    fn acquire(endpoint: &Path) -> Result<(Self, PathBuf)> {
        let parent = endpoint
            .parent()
            .ok_or_else(|| SpikeError::Setup("the endpoint has no parent directory".into()))?;
        secure_directory(parent)?;
        let lock_path = endpoint.with_file_name(format!(
            "{}.singleton",
            endpoint
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| SpikeError::Setup("the endpoint filename is not UTF-8".into()))?
        ));
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&lock_path)?;
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => Ok((
                Self {
                    file,
                    lock_path: lock_path.clone(),
                },
                lock_path,
            )),
            Err(TryLockError::WouldBlock) => Err(SpikeError::EndpointBusy(endpoint.to_owned())),
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

impl Drop for EndpointLease {
    fn drop(&mut self) {
        let _ = fs4::FileExt::unlock(&self.file);
        let _ = fs::remove_file(&self.lock_path);
    }
}

#[cfg(windows)]
fn secure_directory(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    let sid = current_user_sid()?;
    let user_grant = format!("*{sid}:(OI)(CI)F");
    let output = std::process::Command::new("icacls.exe")
        .arg(path)
        .args([
            "/inheritance:r",
            "/grant:r",
            &user_grant,
            "*S-1-5-18:(OI)(CI)F",
        ])
        .output()?;
    if !output.status.success() {
        return Err(SpikeError::Setup(format!(
            "icacls.exe could not apply the user/SYSTEM DACL: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

/// Constructs a real pending wire value for process probes and restart tests.
pub fn pending_status(provider: &str) -> ProviderStatus {
    status(provider, None)
}

/// Reads the ACL text for an evidence receipt.
#[cfg(windows)]
pub fn acl_receipt(path: &Path) -> Result<String> {
    let output = std::process::Command::new("icacls.exe")
        .arg(path)
        .output()?;
    if !output.status.success() {
        return Err(SpikeError::Setup(format!(
            "icacls.exe could not read the endpoint ACL: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|_| SpikeError::Setup("icacls.exe returned non-UTF-8 ACL text".into()))
}

#[cfg(unix)]
pub fn acl_receipt(path: &Path) -> Result<String> {
    let mode = fs::metadata(path)?.permissions();
    use std::os::unix::fs::PermissionsExt as _;
    Ok(format!("mode={:o}", mode.mode() & 0o777))
}

/// Confirms a completed contract has no queued seventh signal after a same-connection fence.
pub fn no_signal_is_ready(stream: &mut zbus::proxy::SignalStream<'static>) -> bool {
    use futures_util::FutureExt as _;
    stream.next().now_or_never().is_none()
}

/// The labels currently attached to peers, useful in cleanup diagnostics.
pub fn labels(handle: &ServerHandle) -> HashSet<String> {
    handle.delivery_counts().into_keys().collect()
}
