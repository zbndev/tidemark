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

use tidemark_types::{
    AuthCandidate, AuthSelection, DataInfo, Preferences, ProviderDefinition, ProviderStatus, ids,
};
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

    /// Adds one more account to a provider the config already has.
    fn add_account(&self, provider: &str, account: &str) -> zbus::Result<()>;

    /// Renames one configured account, carrying its credential and history to the new id.
    fn rename_account(&self, provider: &str, account: &str, new: &str) -> zbus::Result<()>;

    /// Every account the daemon watches.
    fn get_status(&self) -> zbus::Result<Vec<ProviderStatus>>;

    /// A newer published application release, or an empty string when none is known.
    fn get_update(&self) -> zbus::Result<String>;
    /// Application-wide preferences stored by the daemon.
    fn get_preferences(&self) -> zbus::Result<Preferences>;

    /// Paths and storage facts for the Preferences data page.
    fn get_data_info(&self) -> zbus::Result<DataInfo>;

    /// Stored points in the current segment of one window, oldest first.
    fn current_segment(
        &self,
        provider: &str,
        account: &str,
        window: &str,
    ) -> zbus::Result<Vec<tidemark_types::HistoryPoint>>;

    /// Polls now: one provider by slug, or everything when given an empty string.
    fn refresh(&self, provider: &str) -> zbus::Result<()>;

    /// Asks the running window to come forward; the caller exits after this.
    fn request_activate(&self) -> zbus::Result<()>;

    /// Stores an API key for an account.
    fn set_key(&self, provider: &str, account: &str, key: &str) -> zbus::Result<()>;

    /// Stores a browser session the user pasted in, and puts the account on it.
    fn set_session(&self, provider: &str, account: &str, session: &str) -> zbus::Result<()>;

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

    /// Inspects secret-free local authentication candidates for one account.
    fn get_auth_sources(&self, provider: &str, account: &str) -> zbus::Result<Vec<AuthCandidate>>;

    /// Revalidates and stores one local authentication selection.
    fn select_auth_source(
        &self,
        provider: &str,
        account: &str,
        selection: AuthSelection,
    ) -> zbus::Result<()>;

    /// Switches notifications for one of an account's windows on or off.
    fn set_window_notify(
        &self,
        provider: &str,
        account: &str,
        window: &str,
        enabled: bool,
    ) -> zbus::Result<()>;

    fn set_release_check(&self, enabled: bool) -> zbus::Result<()>;
    fn set_minimize_on_close(&self, enabled: bool) -> zbus::Result<()>;
    fn set_theme(&self, theme: &str) -> zbus::Result<()>;
    fn set_startup_mode(&self, mode: &str) -> zbus::Result<()>;
    fn set_history_retention(&self, retention: &str) -> zbus::Result<()>;

    /// Chooses zone-based or fixed-interval polling for healthy accounts.
    fn set_refresh_mode(&self, mode: &str) -> zbus::Result<()>;

    /// Sets the fixed interval Manual mode polls at, in minutes.
    fn set_refresh_minutes(&self, minutes: u32) -> zbus::Result<()>;

    /// All three proxy settings at once: they are one setting, and half of it applied is a
    /// proxy nothing can be reached through.
    fn set_proxy(&self, mode: &str, host: &str, port: u16) -> zbus::Result<()>;

    fn clear_history(&self) -> zbus::Result<()>;

    /// Puts the configured providers in the order the user arranged the cards in.
    fn set_order(&self, providers: &[String]) -> zbus::Result<()>;

    /// Puts one provider's accounts in the order the user arranged them in.
    fn set_account_order(&self, provider: &str, accounts: Vec<String>) -> zbus::Result<()>;

    /// What the daemon on the other end is.
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

    /// Availability of a newer published application release changed.
    #[zbus(signal)]
    fn update_changed(&self, version: &str) -> zbus::Result<()>;

    /// Another client asked this one to come forward.
    #[zbus(signal)]
    fn activate_requested(&self) -> zbus::Result<()>;
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
        String,
        Preferences,
        DataInfo,
        Vec<ProviderDefinition>,
        Vec<ProviderStatus>,
    ),
    /// One account changed.
    Changed(ProviderStatus),
    /// One configured account was removed.
    Removed { provider: String, account: String },
    /// The configured providers were put in this order.
    Reordered(Vec<String>),
    /// Availability of a newer published application release changed.
    Available(String),
    /// Application-wide preferences changed.
    Preferences(Preferences),
    /// Paths or storage facts changed.
    Data(DataInfo),
    /// Another client asked this one to come forward.
    Activate,
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
                    tracing::warn!(%error, "cannot talk to the daemon");
                    #[cfg(unix)]
                    let unreachable = format!("Cannot reach the session bus: {error}");
                    #[cfg(windows)]
                    let unreachable = format!("Cannot reach the daemon: {error}");
                    on(Update::Waiting(unreachable));
                }
            }
            gtk::glib::timeout_future_seconds(RETRY_SECONDS).await;
        }
    });
}

/// One connection's worth of work. Returns when a stream ends, which on a session bus means
/// the bus itself went away.
async fn serve(on: &impl Fn(Update)) -> zbus::Result<()> {
    #[cfg(unix)]
    let connection = zbus::Connection::session().await?;
    #[cfg(windows)]
    let connection = reconnect::connect(on).await?;
    let proxy = DaemonProxy::new(&connection).await?;

    // Subscribed before the first `GetStatus`, so that a poll finishing between the two is
    // delivered as a signal rather than missed by both. On p2p there is no bus name to
    // watch: the connection ending is the whole story of the daemon going away.
    #[cfg(unix)]
    let mut owner = pin!(proxy.inner().receive_owner_changed().await?);
    let mut changes = pin!(proxy.receive_provider_changed().await?);
    let mut removals = pin!(proxy.receive_provider_removed().await?);
    let mut orders = pin!(proxy.receive_order_changed().await?);
    let mut updates = pin!(proxy.receive_update_changed().await?);
    let mut preference_changes = pin!(proxy.receive_preferences_changed().await?);
    let mut data_changes = pin!(proxy.receive_data_changed().await?);
    let mut activations = pin!(proxy.receive_activate_requested().await?);

    load(&proxy, on).await;

    loop {
        let event = poll_fn(|context| {
            #[cfg(unix)]
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
            if let Poll::Ready(activation) = activations.as_mut().poll_next(context) {
                return Poll::Ready(Event::Activate(activation));
            }
            Poll::Pending
        })
        .await;

        match event {
            // The daemon appeared, or was replaced by a newer one. Anything it published
            // while we were not listening was announced to nobody, so re-read all of it.
            #[cfg(unix)]
            Event::Owner(Some(Some(unique))) => {
                tracing::info!(%unique, "the daemon is on the bus");
                load(&proxy, on).await;
            }
            #[cfg(unix)]
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
            Event::Activate(Some(_)) => on(Update::Activate),
            // Any stream ending means the connection is finished with.
            #[cfg(unix)]
            Event::Owner(None) => return Ok(()),
            Event::Changed(None)
            | Event::Removed(None)
            | Event::Reordered(None)
            | Event::Available(None)
            | Event::Preferences(None)
            | Event::Data(None)
            | Event::Activate(None) => return Ok(()),
        }
    }
}

/// One-shot activation forward for the second-instance path (Windows): connect to
/// the daemon and ask the running window to come forward. No GLib main loop runs
/// yet, so the caller drives this with `MainContext::block_on`. Errors are the
/// caller's to log; a missing daemon just means there is no window to raise.
#[cfg(windows)]
pub(crate) async fn request_activation() -> zbus::Result<()> {
    let connection = reconnect::connect(&|_| {}).await?;
    let proxy = DaemonProxy::new(&connection).await?;
    proxy.request_activate().await
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
    let available = proxy.get_update().await.unwrap_or_else(|error| {
        tracing::info!(%error, "the daemon did not answer GetUpdate; hiding update availability");
        String::new()
    });
    let preferences = match proxy.get_preferences().await {
        Ok(preferences) => preferences,
        // A daemon from before the method existed. That is the one failure compiled-in
        // defaults are the right answer to: there is nothing on the other end that could
        // have an opinion yet.
        Err(error) if unknown_method(&error) => {
            tracing::info!(%error, "the daemon predates GetPreferences; using defaults");
            Preferences::default()
        }
        // Anything else is a daemon that has the method and could not answer it — a
        // config.toml that does not parse, most likely. Defaults here would put a
        // configuration the user never wrote on screen and invite them to edit it, so
        // the failure goes on screen instead.
        Err(error) => {
            tracing::warn!(%error, "the daemon could not read its preferences");
            on(Update::Waiting(format!(
                "The daemon could not read its preferences: {error}"
            )));
            return;
        }
    };
    let data = proxy.get_data_info().await.unwrap_or_else(|error| {
        tracing::info!(%error, "the daemon did not answer GetDataInfo; hiding storage facts");
        DataInfo {
            config_path: String::new(),
            history_path: String::new(),
            history_bytes: 0,
            key_schema: ids::SECRET_SCHEMA.into(),
            token_schema: ids::TOKEN_SCHEMA.into(),
            release_check_available: false,
        }
    });
    match (definitions, statuses) {
        (Ok(definitions), Ok(statuses)) => on(Update::Connected(
            proxy.clone(),
            version,
            available,
            preferences,
            data,
            definitions,
            statuses,
        )),
        (Err(error), _) | (_, Err(error)) => {
            tracing::info!(%error, "the daemon did not answer ListProviders or GetStatus");
            on(Update::Waiting("The daemon is not running.".into()));
        }
    }
}

/// Whether an error says the method itself is not there.
///
/// It is the one answer that separates an older daemon, which a client may speak for with
/// its own defaults, from a current daemon reporting a real failure, which it may not.
/// zbus delivers an error reply as [`zbus::Error::MethodError`] carrying the wire name;
/// the [`zbus::fdo`] form is matched too so a locally built error classifies the same way.
fn unknown_method(error: &zbus::Error) -> bool {
    const UNKNOWN_METHOD: &str = "org.freedesktop.DBus.Error.UnknownMethod";
    match error {
        zbus::Error::MethodError(name, _, _) => name.as_str() == UNKNOWN_METHOD,
        zbus::Error::FDO(error) => matches!(error.as_ref(), zbus::fdo::Error::UnknownMethod(_)),
        _ => false,
    }
}

/// The Windows reconnect protocol: probe the daemon's p2p endpoint, spawn `tidemarkd.exe`
/// when nothing is serving it, and retry on the frozen schedule until the 15-second outage
/// deadline. The pure decision pieces (the retry table, the endpoint path and its
/// directory, the spawn throttle) are compiled wherever tests run, so their unit tests run
/// in Linux CI too; only the transport itself is Windows-only.
///
/// # Why a failed probe always spawns
///
/// It did not always. The gate used to read the `io::ErrorKind` of the failed connect and
/// spawn only for `NotFound` or `ConnectionRefused`, on the reasoning that anything else
/// was a state a second daemon could not improve. That reasoning is Unix's: Windows
/// AF_UNIX never answers `NotFound`, and it answers `WSAENETDOWN` when the endpoint's
/// directory is missing — which, on a machine the daemon has never run on, it always is.
/// The client would therefore probe, decide the situation was hopeless, and wait out every
/// outage without ever bringing the daemon up. The directory is now the client's to create
/// and the gate no longer guesses: a connect that fails means nothing is listening, and a
/// throttled spawn is the answer to that regardless of which errno said so.
#[cfg(any(windows, test))]
mod reconnect {
    use std::fs;
    use std::io;
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    #[cfg(windows)]
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    #[cfg(windows)]
    use uds_windows::UnixStream;

    #[cfg(windows)]
    use super::{DaemonProxy, Update};

    /// The frozen retry table: exponential steps per the contract, then the cap.
    const RETRY_TABLE_MS: [u64; 6] = [50, 100, 200, 400, 800, 1000];

    /// A whole outage gets this long before the startup error goes on screen and the
    /// outer five-second loop takes over again.
    #[cfg(windows)]
    const OUTAGE_DEADLINE: Duration = Duration::from_secs(15);

    /// Readiness is bounded: an endpoint that connects but never answers `Version` is not
    /// a ready daemon.
    #[cfg(windows)]
    const READINESS_TIMEOUT: Duration = Duration::from_secs(2);

    /// At most one spawn per outage, and spawns never closer together than this.
    const SPAWN_COOLDOWN: Duration = Duration::from_secs(30);

    /// `CREATE_NO_WINDOW`: the daemon is a console-subsystem program and must not flash a
    /// console window when the GUI brings it up.
    #[cfg(windows)]
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// The endpoint the daemon serves, under a given `%LOCALAPPDATA%`, with the directory
    /// it lives in made to exist. Must stay in step with `tidemarkd::peer`.
    ///
    /// Creating `run\` is the client's job as much as the daemon's, and on a machine where
    /// the daemon has never run it is *only* the client's: the installer does not create it,
    /// the uninstaller does not remove it, and the GUI is what the Start menu launches. An
    /// AF_UNIX connect whose parent directory is missing does not fail like a missing
    /// socket — Windows answers `WSAENETDOWN`, not `WSAECONNREFUSED` — so a client that
    /// leaves the directory to the daemon spends every probe on an error shaped like
    /// something other than "no daemon here".
    fn endpoint_under(local: &Path) -> io::Result<PathBuf> {
        let endpoint = local.join("tidemark").join("run").join("d.sock");
        if let Some(parent) = endpoint.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(endpoint)
    }

    /// The endpoint under this user's `%LOCALAPPDATA%`.
    #[cfg(windows)]
    fn endpoint_path() -> io::Result<PathBuf> {
        let local = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "LOCALAPPDATA is not set; the daemon endpoint has nowhere to live",
            )
        })?;
        endpoint_under(Path::new(&local))
    }

    /// Advances through the frozen table, holding at its cap.
    #[derive(Debug, Default)]
    struct RetrySchedule {
        attempt: usize,
    }

    impl RetrySchedule {
        fn next_delay(&mut self) -> Duration {
            let delay = RETRY_TABLE_MS[self.attempt.min(RETRY_TABLE_MS.len() - 1)];
            self.attempt += 1;
            Duration::from_millis(delay)
        }
    }

    /// Whether a spawn is allowed: at most one per outage, and never within the cooldown
    /// of the previous one. UIs racing are safe — the daemon's mutex is authoritative —
    /// so this throttle is about not stomping, not about correctness.
    fn spawn_allowed(now: Instant, last_spawn: Option<Instant>, spawned_this_outage: bool) -> bool {
        !spawned_this_outage && last_spawn.is_none_or(|at| now.duration_since(at) >= SPAWN_COOLDOWN)
    }

    /// When the last spawn happened, across outages: the cooldown outlives any single
    /// outage, because the outer loop starts a fresh one every five seconds.
    #[cfg(windows)]
    static LAST_SPAWN: Mutex<Option<Instant>> = Mutex::new(None);

    /// Brings `tidemarkd.exe` up: the sibling of this program, found by its own location
    /// — never a `PATH` search — with no arguments and no environment overrides.
    ///
    /// The daemon started here is this client's to end. Nothing else will: it has no
    /// window, no icon and no service manager behind it, so it joins the kill-on-close job
    /// and dies when this process does. See `daemon_job`.
    #[cfg(windows)]
    fn spawn_daemon() {
        let spawned = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("tidemarkd.exe")))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "this program has no directory to find tidemarkd.exe beside",
                )
            })
            .and_then(|path| {
                let mut command = std::process::Command::new(path);
                command.creation_flags(CREATE_NO_WINDOW);
                command.spawn()
            });
        match spawned {
            Ok(child) => crate::daemon_job::adopt(&child),
            Err(error) => tracing::warn!(%error, "could not spawn tidemarkd.exe"),
        }
    }

    /// Connects to the daemon's endpoint and makes sure it is really a daemon: readiness
    /// is the endpoint accepting plus a `Version` answer inside [`READINESS_TIMEOUT`].
    #[cfg(windows)]
    async fn ready(stream: UnixStream) -> zbus::Result<zbus::Connection> {
        let connect = async {
            let connection = zbus::connection::Builder::async_io_unix_stream(stream)
                .p2p()
                .build()
                .await?;
            let proxy = DaemonProxy::new(&connection).await?;
            let _version = proxy.version().await?;
            Ok(connection)
        };
        let mut connect = std::pin::pin!(connect);
        let mut timeout = gtk::glib::timeout_future(READINESS_TIMEOUT);
        super::poll_fn(|context| {
            if let super::Poll::Ready(result) = connect.as_mut().poll(context) {
                return super::Poll::Ready(result);
            }
            if timeout.as_mut().poll(context).is_ready() {
                return super::Poll::Ready(Err(zbus::Error::InputOutput(std::sync::Arc::new(
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "the endpoint answered but never became a ready daemon",
                    ),
                ))));
            }
            super::Poll::Pending
        })
        .await
    }

    /// One outage: probe, spawn when justified, retry on the frozen schedule, and give
    /// up with a visible error at the deadline so the outer loop's five-second retry
    /// takes over.
    #[cfg(windows)]
    pub(super) async fn connect(on: &impl Fn(Update)) -> zbus::Result<zbus::Connection> {
        let endpoint = endpoint_path()
            .map_err(|error| zbus::Error::InputOutput(std::sync::Arc::new(error)))?;
        let deadline = Instant::now() + OUTAGE_DEADLINE;
        let mut schedule = RetrySchedule::default();
        let mut spawned_this_outage = false;
        let mut refusal = String::new();

        loop {
            match UnixStream::connect(&endpoint) {
                Ok(stream) => {
                    // A listener that never becomes a ready daemon is not a missing
                    // daemon: spawning on top of it is exactly the wrong move, so keep
                    // probing until the deadline says otherwise.
                    if let Ok(connection) = ready(stream).await {
                        return Ok(connection);
                    }
                }
                // Nothing accepted, so nothing is serving the endpoint: bring a daemon
                // up, whatever shape the refusal took. The throttle below bounds this to
                // one spawn per outage, the daemon's own mutex makes a redundant one
                // harmless, and a daemon that cannot bind writes the real reason to
                // daemon.log — all of which beats a client that decides on an errno it
                // has never seen that the situation is hopeless, and waits forever.
                Err(error) => {
                    refusal = error.to_string();
                    let now = Instant::now();
                    let mut last = LAST_SPAWN.lock().expect("no code panics holding this");
                    if spawn_allowed(now, *last, spawned_this_outage) {
                        spawned_this_outage = true;
                        *last = Some(now);
                        drop(last);
                        tracing::info!(
                            %error,
                            "nothing is serving the daemon endpoint; spawning tidemarkd.exe"
                        );
                        spawn_daemon();
                    } else {
                        tracing::debug!(%error, "still nothing serving the daemon endpoint");
                    }
                }
            }
            if Instant::now() >= deadline {
                let message = "The daemon did not come up. Waiting for it.";
                // The screen gets the sentence; the log gets the errno. An outage that
                // never ends used to leave no shipped record of why: the reason went to
                // debug, and the only line in ui.log was this deadline with no cause on it.
                tracing::warn!(
                    endpoint = %endpoint.display(),
                    refusal = %refusal,
                    "gave up waiting for the daemon endpoint"
                );
                on(Update::Waiting(message.into()));
                return Err(zbus::Error::InputOutput(std::sync::Arc::new(
                    io::Error::new(io::ErrorKind::TimedOut, message),
                )));
            }
            gtk::glib::timeout_future(schedule.next_delay()).await;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_retry_table_is_the_frozen_one() {
            let mut schedule = RetrySchedule::default();
            let delays: Vec<u64> = std::iter::repeat_with(|| schedule.next_delay())
                .map(|delay| delay.as_millis() as u64)
                .take(8)
                .collect();
            assert_eq!(delays, vec![50, 100, 200, 400, 800, 1000, 1000, 1000]);
        }

        /// A temporary `%LOCALAPPDATA%` that has never held a daemon: no `tidemark\`,
        /// and certainly no `run\`. What a fresh install looks like.
        struct UntouchedLocalAppData(PathBuf);

        impl UntouchedLocalAppData {
            fn new(label: &str) -> Self {
                static SERIAL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
                let serial = SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "tidemark-endpoint-{label}-{}-{serial}",
                    std::process::id()
                ));
                let _ = fs::remove_dir_all(&path);
                fs::create_dir_all(&path).expect("a temporary LOCALAPPDATA");
                Self(path)
            }

            fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for UntouchedLocalAppData {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        /// The client is what the Start menu launches, so on a machine the daemon has
        /// never run on the client is the only thing that can create `run\`. Leaving it
        /// to the daemon is what shipped, and it is why a first launch never ended.
        #[test]
        fn the_endpoint_directory_exists_before_anything_probes_it() {
            let local = UntouchedLocalAppData::new("fresh");
            assert!(!local.path().join("tidemark").exists());

            let endpoint = endpoint_under(local.path()).expect("the endpoint path");

            assert!(
                endpoint.parent().expect("run/").is_dir(),
                "the run directory has to be there before the first connect"
            );
            assert!(!endpoint.exists(), "but the socket itself is the daemon's");
        }

        /// Asking twice is what every retry does.
        #[test]
        fn preparing_an_endpoint_directory_that_is_already_there_is_fine() {
            let local = UntouchedLocalAppData::new("twice");
            let first = endpoint_under(local.path()).expect("the first time");
            let second = endpoint_under(local.path()).expect("the second time");
            assert_eq!(first, second);
        }

        /// The bug, pinned to the errno that caused it.
        ///
        /// The old gate spawned on `NotFound` or `ConnectionRefused` and treated
        /// everything else as a state no daemon could fix. On Windows an absent endpoint
        /// never answers `NotFound` — AF_UNIX gives `WSAECONNREFUSED` for that too — and
        /// an endpoint whose *directory* is missing answers `WSAENETDOWN`, which fell
        /// through to "do not spawn". A fresh install has no `run\`, so the client sat on
        /// that one error for the whole outage, every outage, and never brought the daemon
        /// up. Both shapes have to reach the spawn now.
        #[cfg(windows)]
        #[test]
        fn a_windows_endpoint_no_daemon_is_serving_never_reports_itself_as_missing() {
            use uds_windows::UnixStream;

            let local = UntouchedLocalAppData::new("errno");
            let unprepared = local.path().join("tidemark").join("run").join("d.sock");
            let missing_directory = UnixStream::connect(&unprepared)
                .expect_err("nothing can be serving a socket in a directory that is not there");
            assert_ne!(
                missing_directory.kind(),
                io::ErrorKind::NotFound,
                "the Unix shape of this error is not the one Windows gives"
            );

            let prepared = endpoint_under(local.path()).expect("the endpoint path");
            let no_daemon =
                UnixStream::connect(&prepared).expect_err("no daemon has bound this endpoint");
            assert_eq!(
                no_daemon.kind(),
                io::ErrorKind::ConnectionRefused,
                "an absent Windows endpoint refuses; it is never NotFound"
            );
        }

        #[test]
        fn the_spawn_throttle_is_one_per_outage_and_one_per_thirty_seconds() {
            let start = Instant::now();
            // (now, last spawn, already spawned this outage, expected)
            let table = [
                (start, None, false, true),
                (start + Duration::from_millis(1), Some(start), true, false),
                (start + Duration::from_secs(29), Some(start), false, false),
                (start + Duration::from_secs(30), Some(start), false, true),
                (start + Duration::from_secs(31), Some(start), true, false),
            ];
            for (now, last_spawn, this_outage, allowed) in table {
                assert_eq!(
                    spawn_allowed(now, last_spawn, this_outage),
                    allowed,
                    "now={now:?} last={last_spawn:?} this-outage={this_outage}"
                );
            }
        }
    }
}

/// One of the streams the [`serve`] loop watches produced something.
enum Event {
    #[cfg(unix)]
    Owner(Option<Option<zbus::names::UniqueName<'static>>>),
    Changed(Option<ProviderChanged>),
    Removed(Option<ProviderRemoved>),
    Reordered(Option<OrderChanged>),
    Available(Option<UpdateChanged>),
    Preferences(Option<PreferencesChanged>),
    Data(Option<DataChanged>),
    Activate(Option<ActivateRequested>),
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::process;

    use tidemark_types::{
        AuthCandidate, AuthSelection, Preferences, ProviderDefinition, ProviderStatus, ids,
    };

    use super::{DaemonProxy, Update, load};
    use zbus::fdo;

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

    #[derive(Debug, Clone, Default)]
    struct BrowserAuthService(std::sync::Arc<std::sync::Mutex<Option<AuthSelection>>>);

    #[zbus::interface(name = "io.github.zbndev.Tidemark.Daemon1")]
    impl BrowserAuthService {
        fn get_auth_sources(&self, _provider: &str, _account: &str) -> Vec<AuthCandidate> {
            vec![AuthCandidate {
                id: "cursor-app".into(),
                title: "Cursor App".into(),
                subtitle: None,
                state: "ready".into(),
                children: Vec::new(),
            }]
        }

        fn select_auth_source(&self, _provider: &str, _account: &str, selection: AuthSelection) {
            *self.0.lock().expect("no test panics holding this") = Some(selection);
        }
    }
    /// A daemon that implements `GetPreferences` but cannot read its own storage.
    #[derive(Debug)]
    struct FailingPreferencesService;

    #[zbus::interface(name = "io.github.zbndev.Tidemark.Daemon1")]
    impl FailingPreferencesService {
        fn list_providers(&self) -> Vec<ProviderDefinition> {
            Vec::new()
        }

        fn get_status(&self) -> Vec<ProviderStatus> {
            Vec::new()
        }

        fn get_preferences(&self) -> fdo::Result<Preferences> {
            Err(fdo::Error::Failed(
                "the preferences in config.toml do not parse".into(),
            ))
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
    fn the_proxy_round_trips_secret_free_browser_auth_calls() {
        zbus::block_on(async {
            let Ok(client_connection) = zbus::Connection::session().await else {
                eprintln!("skipped: no session bus reachable");
                return;
            };
            let name = format!(
                "io.github.zbndev.Tidemark.BrowserAuthTest.p{}",
                process::id()
            );
            let service = BrowserAuthService::default();
            let selected = std::sync::Arc::clone(&service.0);
            let _server = zbus::connection::Builder::session()
                .expect("session bus address")
                .name(name.as_str())
                .expect("valid test bus name")
                .serve_at(ids::OBJECT_PATH, service)
                .expect("valid test object")
                .build()
                .await
                .expect("test service starts");
            let proxy = DaemonProxy::builder(&client_connection)
                .destination(name.as_str())
                .expect("valid destination")
                .path(ids::OBJECT_PATH)
                .expect("valid path")
                .build()
                .await
                .expect("proxy builds");

            let sources = proxy
                .get_auth_sources("cursor", "default")
                .await
                .expect("sources arrive");
            assert_eq!(sources[0].id, "cursor-app");
            assert!(!format!("{sources:?}").contains("session="));
            let selection = AuthSelection {
                mode: "cursor-app".into(),
                candidate: None,
            };
            proxy
                .select_auth_source("cursor", "default", selection.clone())
                .await
                .expect("selection arrives");
            assert_eq!(
                *selected.lock().expect("no test panics holding this"),
                Some(selection)
            );
        });
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
                Some(Update::Connected(
                    _,
                    version,
                    available,
                    preferences,
                    data,
                    definitions,
                    statuses,
                )) => {
                    assert_eq!(version, None);
                    assert_eq!(available, "");
                    assert_eq!(preferences, Preferences::default());
                    assert!(!data.release_check_available);
                    assert!(definitions.is_empty());
                    assert!(statuses.is_empty());
                }
                other => panic!("expected connected status, got {other:?}"),
            }
        });
    }
    #[test]
    fn only_a_missing_get_preferences_is_answered_with_defaults() {
        zbus::block_on(async {
            let Ok(client_connection) = zbus::Connection::session().await else {
                eprintln!("skipped: no session bus is reachable");
                return;
            };

            // A daemon from before GetPreferences existed: the interface answers, the
            // method does not, and compiled-in defaults are the compatibility answer.
            let legacy = format!("io.github.zbndev.Tidemark.LegacyTest.p{}", process::id());
            let _legacy = zbus::connection::Builder::session()
                .expect("session bus address")
                .name(legacy.as_str())
                .expect("valid test bus name")
                .serve_at(ids::OBJECT_PATH, StatusService)
                .expect("valid test object")
                .build()
                .await
                .expect("legacy test service");
            let legacy_destination = zbus::names::OwnedBusName::try_from(legacy.as_str())
                .expect("valid owned test destination");
            let proxy = DaemonProxy::builder(&client_connection)
                .destination(legacy_destination)
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
                Some(Update::Connected(_, _, _, preferences, _, _, _)) => {
                    assert_eq!(preferences, Preferences::default());
                }
                other => panic!("expected compatibility defaults, got {other:?}"),
            }

            // A daemon that has GetPreferences but fails to read: the error must reach
            // the window rather than being replaced by defaults the user never wrote.
            let broken = format!("io.github.zbndev.Tidemark.BrokenTest.p{}", process::id());
            let _broken = zbus::connection::Builder::session()
                .expect("session bus address")
                .name(broken.as_str())
                .expect("valid test bus name")
                .serve_at(ids::OBJECT_PATH, FailingPreferencesService)
                .expect("valid test object")
                .build()
                .await
                .expect("broken test service");
            let broken_destination = zbus::names::OwnedBusName::try_from(broken.as_str())
                .expect("valid owned test destination");
            let proxy = DaemonProxy::builder(&client_connection)
                .destination(broken_destination)
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
                Some(Update::Waiting(reason)) => {
                    assert!(
                        reason.contains("do not parse"),
                        "the read failure should stay visible, got: {reason}"
                    );
                }
                other => panic!("expected the read failure to stay visible, got {other:?}"),
            }
        });
    }
}
