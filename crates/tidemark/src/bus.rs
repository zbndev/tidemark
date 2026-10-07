//! The daemon connection: the GTK client's `bus.rs` with Slint's event loop in place of
//! GLib's. The same proxy, the same signals; zbus's `async-io` backend drives the
//! connection on its own thread, so these futures run on the UI thread.
//!
//! On Linux the daemon is on the session bus and its name is watched, so a restart is
//! noticed and re-read. On Windows there is no bus: the daemon serves a zbus p2p endpoint
//! over AF_UNIX, the client brings it up when nothing is serving it (`reconnect`), and the
//! connection ending is the whole story of the daemon going away.

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
        /// The daemon's own version, for the About dialog's troubleshooting page.
        version: Option<String>,
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
    /// Another launch asked this window to come forward.
    Activate,
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
                    #[cfg(unix)]
                    let unreachable = format!("Cannot reach the session bus: {error}");
                    #[cfg(windows)]
                    let unreachable = format!("Cannot reach the daemon: {error}");
                    on(Update::Waiting(unreachable));
                }
            }
            async_io::Timer::after(RETRY).await;
        }
    });
    if let Err(error) = spawned {
        tracing::error!(%error, "the event loop refused the daemon watcher");
    }
}

/// Asks the running window to come forward, for a second launch on its way out (Windows).
/// Errors are the caller's to log; a missing daemon just means no window to raise.
#[cfg(windows)]
pub async fn request_activation() -> zbus::Result<()> {
    let connection = reconnect::connect(&|_| {}).await?;
    let proxy = DaemonProxy::new(&connection).await?;
    proxy.request_activate().await
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
    #[cfg(windows)]
    use async_io::Timer;

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
        let mut timeout = std::pin::pin!(Timer::after(READINESS_TIMEOUT));
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
            Timer::after(schedule.next_delay()).await;
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

enum Event {
    #[cfg(unix)]
    Owner(Option<Option<zbus::names::UniqueName<'static>>>),
    Changed(Option<tidemark_ipc::ProviderChanged>),
    Removed(Option<tidemark_ipc::ProviderRemoved>),
    Reordered(Option<tidemark_ipc::OrderChanged>),
    Available(Option<tidemark_ipc::UpdateChanged>),
    Preferences(Option<tidemark_ipc::PreferencesChanged>),
    Data(Option<tidemark_ipc::DataChanged>),
    Plugins(Option<tidemark_ipc::PluginsChanged>),
    Activate(Option<tidemark_ipc::ActivateRequested>),
}

async fn serve(on: &impl Fn(Update)) -> zbus::Result<()> {
    #[cfg(unix)]
    let connection = zbus::Connection::session().await?;
    #[cfg(windows)]
    let connection = reconnect::connect(on).await?;
    let proxy = DaemonProxy::new(&connection).await?;

    // Subscribed before the first `GetStatus`, so that a poll finishing between the two is
    // delivered as a signal rather than missed by both.
    #[cfg(unix)]
    let mut owner = pin!(proxy.inner().receive_owner_changed().await?);
    let mut changes = pin!(proxy.receive_provider_changed().await?);
    let mut removals = pin!(proxy.receive_provider_removed().await?);
    let mut orders = pin!(proxy.receive_order_changed().await?);
    let mut updates = pin!(proxy.receive_update_changed().await?);
    let mut preference_changes = pin!(proxy.receive_preferences_changed().await?);
    let mut data_changes = pin!(proxy.receive_data_changed().await?);
    let mut plugin_changes = pin!(proxy.receive_plugins_changed().await?);
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
            if let Poll::Ready(plugins) = plugin_changes.as_mut().poll_next(context) {
                return Poll::Ready(Event::Plugins(plugins));
            }
            if let Poll::Ready(activation) = activations.as_mut().poll_next(context) {
                return Poll::Ready(Event::Activate(activation));
            }
            Poll::Pending
        })
        .await;

        match event {
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
            Event::Plugins(Some(_)) => match proxy.list_providers().await {
                Ok(definitions) => on(Update::Catalog(definitions)),
                Err(error) => {
                    tracing::warn!(%error, "the catalog could not be re-read after PluginsChanged");
                }
            },
            Event::Activate(Some(_)) => on(Update::Activate),
            #[cfg(unix)]
            Event::Owner(None) => return Ok(()),
            Event::Changed(None)
            | Event::Removed(None)
            | Event::Reordered(None)
            | Event::Available(None)
            | Event::Preferences(None)
            | Event::Data(None)
            | Event::Plugins(None)
            | Event::Activate(None) => return Ok(()),
        }
    }
}

/// Reads the whole picture. Optional calls an older daemon lacks fall back to defaults.
async fn load(proxy: &DaemonProxy<'static>, on: &impl Fn(Update)) {
    let definitions = proxy.list_providers().await;
    let statuses = proxy.get_status().await;
    let available = proxy.get_update().await.unwrap_or_default();
    let version = proxy.version().await.ok();
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
            version,
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
