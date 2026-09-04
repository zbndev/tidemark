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
#[cfg(windows)]
mod lifecycle;
mod notify;
mod peer;
mod registry;
mod scheduler;
mod service;
mod startup;
#[cfg(feature = "update-check")]
mod update;

use std::error::Error;
use std::sync::Arc;

use tidemark_core::config::Config;
use tidemark_core::debug;
use tidemark_core::paths;
use tidemark_core::providers::http::{self, Proxy};
use tidemark_core::secrets::Secrets;
use tidemark_core::storage::History;
use tidemark_types::{ProviderStatus, ids};
use tokio::sync::{mpsc, watch};

#[cfg(unix)]
use zbus::object_server::SignalEmitter;

use crate::engine::{Command, Engine, Publication};
use crate::peer::{Announcement, PeerHub};
use crate::service::{Daemon, Published, PublishedUpdate};

/// Commands from D-Bus clients. Small: a burst of refreshes is a user hammering a button,
/// and the loop collapses them into one poll anyway.
const COMMAND_QUEUE: usize = 16;

/// Finished statuses waiting to be published. One per account per poll.
const UPDATE_QUEUE: usize = 64;

#[cfg(feature = "update-check")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleaseWait {
    Ready,
    Disabled,
    Closed,
}

#[cfg(feature = "update-check")]
async fn wait_for_release_check(
    enabled: &mut watch::Receiver<bool>,
    delay: std::time::Duration,
) -> ReleaseWait {
    loop {
        if !*enabled.borrow_and_update() {
            if enabled.changed().await.is_err() {
                return ReleaseWait::Closed;
            }
            continue;
        }
        return tokio::select! {
            _ = tokio::time::sleep(delay) => ReleaseWait::Ready,
            changed = enabled.changed() => {
                if changed.is_err() {
                    ReleaseWait::Closed
                } else {
                    ReleaseWait::Disabled
                }
            }
        };
    }
}
/// Applies one finished check, whatever its outcome. Whether the result may still be
/// published is decided inside [`PublishedUpdate::publish`], against the same lock a
/// disable takes — never against an enabled flag copied before an await.
#[cfg(feature = "update-check")]
async fn publish_result(
    published: &PublishedUpdate,
    result: Result<Option<String>, update::CheckError>,
) -> Result<Option<String>, update::CheckError> {
    Ok(published.publish(result?).await)
}

/// Spawns the watcher that turns the platform's "stop now" notice into
/// [`Command::Shutdown`], so the engine stops cleanly instead of being killed mid-poll.
/// The streams are registered before the spawn: a daemon that cannot arrange to hear
/// its own stop signal is a startup failure, never a runtime surprise.
#[cfg(unix)]
fn shutdown_signals(
    commands: mpsc::Sender<Command>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    use tokio::signal::unix::{SignalKind, signal};

    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    Ok(tokio::spawn(async move {
        let reason = tokio::select! {
            _ = term.recv() => "SIGTERM",
            _ = interrupt.recv() => "SIGINT",
        };
        tracing::info!(reason, "tidemarkd stopping");
        let _ = commands.send(Command::Shutdown).await;
    }))
}

/// The same contract on Windows: there is no SIGTERM for a console application, the OS
/// asks to be let go with console control events. tokio's signal feature — already
/// linked — registers the one `SetConsoleCtrlHandler` a process may install and hands
/// back every event a daemon can receive as streams, waited on exactly like the Unix
/// arm's signal pair.
///
/// The `windows` crate is deliberately not used for this: its `SetConsoleCtrlHandler`
/// is an `unsafe fn` taking an `unsafe extern "system" fn` handler, and this workspace
/// forbids `unsafe`. tokio covers the same events through a safe API, so tidemarkd
/// needs no `[target.'cfg(windows)'.dependencies]` entry here.
#[cfg(windows)]
fn shutdown_signals(
    commands: mpsc::Sender<Command>,
) -> std::io::Result<tokio::task::JoinHandle<()>> {
    let mut term = tokio::signal::windows::ctrl_break()?;
    let mut interrupt = tokio::signal::windows::ctrl_c()?;
    let mut close = tokio::signal::windows::ctrl_close()?;
    let mut logoff = tokio::signal::windows::ctrl_logoff()?;
    let mut system = tokio::signal::windows::ctrl_shutdown()?;
    Ok(tokio::spawn(async move {
        let reason = tokio::select! {
            _ = term.recv() => "Ctrl+Break",
            _ = interrupt.recv() => "Ctrl+C",
            _ = close.recv() => "Ctrl+Close",
            _ = logoff.recv() => "Ctrl+Logoff",
            _ = system.recv() => "Ctrl+Shutdown",
        };
        tracing::info!(reason, "tidemarkd stopping");
        let _ = commands.send(Command::Shutdown).await;
    }))
}

/// Where finished announcements go: the session-bus emitter on Linux, byte-for-byte the
/// old path, or the p2p peer hub on Windows, which fans out to every connected client.
#[derive(Clone)]
enum Announcer {
    #[cfg(unix)]
    Session(SignalEmitter<'static>),
    #[cfg(windows)]
    Peers(Arc<PeerHub>),
}

impl Announcer {
    async fn provider_changed(&self, status: ProviderStatus) {
        match self {
            #[cfg(unix)]
            Self::Session(emitter) => {
                if let Err(error) = Daemon::provider_changed(emitter, status).await {
                    tracing::warn!(%error, "could not announce a change");
                }
            }
            #[cfg(windows)]
            Self::Peers(hub) => {
                hub.publish(Announcement::ProviderChanged(status)).await;
            }
        }
    }

    async fn provider_removed(&self, provider: String, account: String) {
        match self {
            #[cfg(unix)]
            Self::Session(emitter) => {
                if let Err(error) = Daemon::provider_removed(emitter, &provider, &account).await {
                    tracing::warn!(%error, "could not announce a removal");
                }
            }
            #[cfg(windows)]
            Self::Peers(hub) => {
                hub.publish(Announcement::ProviderRemoved { provider, account })
                    .await;
            }
        }
    }

    async fn order_changed(&self, providers: Vec<String>) {
        match self {
            #[cfg(unix)]
            Self::Session(emitter) => {
                if let Err(error) = Daemon::order_changed(emitter, providers).await {
                    tracing::warn!(%error, "could not announce a reorder");
                }
            }
            #[cfg(windows)]
            Self::Peers(hub) => {
                hub.publish(Announcement::OrderChanged(providers)).await;
            }
        }
    }

    #[cfg(feature = "update-check")]
    async fn update_changed(&self, version: &str) {
        match self {
            #[cfg(unix)]
            Self::Session(emitter) => {
                if let Err(error) = Daemon::update_changed(emitter, version).await {
                    tracing::warn!(%error, "could not announce update availability");
                }
            }
            #[cfg(windows)]
            Self::Peers(hub) => {
                hub.publish(Announcement::UpdateChanged(version.to_owned()))
                    .await;
            }
        }
    }
}

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
    // The single-instance lock comes before any shared state is opened: the history
    // database is the first of it, and a second daemon must never get as far as
    // opening it. Windows has no D-Bus name to race for, so this mutex is the whole
    // arbitration; a duplicate exits 0 quietly because a running daemon is exactly
    // the outcome a duplicate was started to provide.
    #[cfg(windows)]
    let _singleton = match lifecycle::Singleton::acquire() {
        Ok(Some(singleton)) => Some(singleton),
        Ok(None) => {
            tracing::info!("another tidemarkd already runs for this user; this one is not needed");
            return Ok(());
        }
        Err(error) => {
            return Err(format!("could not take the single-instance mutex: {error}").into());
        }
    };

    let history_path = paths::history_path()?;
    let history = History::open(&history_path)?;
    let config_path = paths::config_path()?;
    // A settings file that does not parse is fatal at startup and only at startup. Coming
    // up with the defaults instead would look like the user's edit had been thrown away,
    // and the first thing this daemon would do with the wrong region is report a rejected
    // key. Once running, a later bad edit is reported and the previous settings stand.
    let config = Config::at(config_path.clone())?;
    let preferences = config.preferences()?;
    // Read here and only here: a switch that took effect mid-poll would leave a log whose
    // gaps mean nothing, so turning it on is a restart. Before the first client is built,
    // for the same reason the proxy is — the very first poll is often the interesting one.
    if config.debug_raw_responses()? {
        let log = debug::enable(&paths::data_dir()?)?;
        tracing::warn!(
            log = %log.display(),
            "[debug] raw_responses is on: every provider response is being written to disk verbatim"
        );
    }
    // Before the first client is built, because `registry::accounts` builds several of
    // them: a `reqwest::Client` holds its proxy for life, and one built a line too early
    // would keep talking around the proxy until the daemon was restarted.
    http::set_proxy(Proxy::configured(&preferences).map_err(Box::<dyn Error>::from)?);
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
    let published_update = PublishedUpdate::default();
    let release_check_enabled = preferences.release_check && cfg!(feature = "update-check");
    // The checker's sleep/wake control and the publishable state start from the same
    // durable preference.
    published_update.set_enabled(release_check_enabled).await;
    let (release_checks, release_check_changes) = watch::channel(release_check_enabled);
    #[cfg(feature = "update-check")]
    let mut release_check_changes = release_check_changes;
    #[cfg(not(feature = "update-check"))]
    let _release_check_changes = release_check_changes;

    #[cfg(unix)]
    let connection = zbus::connection::Builder::session()?
        .name(ids::DAEMON_BUS_NAME)?
        .serve_at(
            ids::OBJECT_PATH,
            Daemon::new(
                published.clone(),
                published_update.clone(),
                catalog,
                configured,
                commands.clone(),
                Arc::clone(&secrets),
            )
            .with_preferences(
                config_path.clone(),
                history_path.clone(),
                Arc::new(startup::System),
                release_checks.clone(),
                cfg!(feature = "update-check"),
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

    #[cfg(unix)]
    let announcer = Announcer::Session(SignalEmitter::new(&connection, ids::OBJECT_PATH)?);

    // Windows has no session bus to serve the interface on: the p2p endpoint is the
    // service. The hub is the fan-out point for everything the daemon announces, and
    // every accepted peer gets its own connection serving this same shared state.
    #[cfg(windows)]
    let (announcer, accept_task) = {
        let hub = Arc::new(PeerHub::default());
        let daemon = Daemon::new(
            published.clone(),
            published_update.clone(),
            catalog,
            configured,
            commands.clone(),
            Arc::clone(&secrets),
        )
        .with_preferences(
            config_path.clone(),
            history_path.clone(),
            Arc::new(startup::System),
            release_checks.clone(),
            cfg!(feature = "update-check"),
        )
        .with_hub(Arc::clone(&hub));
        let accept_task = peer::listen(daemon, Arc::clone(&hub)).await?;
        (Announcer::Peers(hub), accept_task)
    };

    // Publishing is one task so that the shared state is always written before the signal
    // that announces it: a client woken by `ProviderChanged` and calling `GetStatus`
    // straight away must never see the older value.
    let publisher = tokio::spawn({
        let published = published.clone();
        let announcer = announcer.clone();
        async move {
            while let Some(publication) = update_queue.recv().await {
                match publication {
                    Publication::Changed(status) => {
                        published.upsert(status.clone()).await;
                        announcer.provider_changed(status).await;
                    }
                    Publication::Removed { provider, account } => {
                        let _ = published.remove(&provider, &account).await;
                        announcer.provider_removed(provider, account).await;
                    }
                    Publication::Reordered(accounts) => {
                        published.reorder(&accounts).await;
                        let mut providers = Vec::new();
                        for (provider, _) in &accounts {
                            if !providers.iter().any(|known| known == provider) {
                                providers.push(provider.clone());
                            }
                        }
                        announcer.order_changed(providers).await;
                    }
                }
            }
        }
    });

    #[cfg(feature = "update-check")]
    let release_checker = match update::Checker::production() {
        Ok(checker) => {
            let published_update = published_update.clone();
            let announcer = announcer.clone();
            Some(tokio::spawn(async move {
                let mut delay = update::INITIAL_DELAY;
                loop {
                    match wait_for_release_check(&mut release_check_changes, delay).await {
                        ReleaseWait::Ready => {}
                        ReleaseWait::Disabled => continue,
                        ReleaseWait::Closed => return,
                    }
                    let result = checker.check().await;
                    match publish_result(&published_update, result).await {
                        Ok(Some(version)) => {
                            announcer.update_changed(&version).await;
                        }
                        Ok(None) => {}
                        Err(error) => tracing::info!(%error, "release check failed"),
                    }
                    delay = update::INTERVAL;
                }
            }))
        }
        Err(error) => {
            tracing::info!(%error, "release checker is unavailable");
            None
        }
    };
    #[cfg(not(feature = "update-check"))]
    let release_checker: Option<tokio::task::JoinHandle<()>> = None;

    let signals = shutdown_signals(commands.clone())?;

    // The daemon's own connection carries the notifications on Linux: it is already
    // open, and org.freedesktop.Notifications is on the same bus as everything else here.
    // Windows has no session bus, and the toast transport (todo 16) needs none.
    #[cfg(unix)]
    let notifier = Arc::new(notify::Desktop::new(connection.clone()));
    #[cfg(windows)]
    let notifier = Arc::new(notify::Desktop::new());
    let mut engine = Engine::new(
        accounts,
        history,
        secrets,
        updates,
        config_path,
        scheduler::RefreshMode::configured(&preferences),
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
    if let Some(release_checker) = release_checker {
        release_checker.abort();
    }
    signals.abort();
    #[cfg(windows)]
    accept_task.abort();
    Ok(())
}

#[cfg(all(test, feature = "update-check"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_failed_release_check_preserves_the_previous_update() {
        let published = PublishedUpdate::default();
        published.set_enabled(true).await;
        assert_eq!(
            published.publish(Some("0.2.0".into())).await,
            Some("0.2.0".into())
        );

        let result = publish_result(&published, Err(update::CheckError::Version)).await;

        assert!(result.is_err());
        assert_eq!(published.get().await, "0.2.0");
    }

    #[tokio::test]
    async fn only_a_changed_release_result_needs_a_signal() {
        let published = PublishedUpdate::default();
        published.set_enabled(true).await;
        assert_eq!(
            publish_result(&published, Ok(Some("0.3.0".into())))
                .await
                .unwrap(),
            Some("0.3.0".into())
        );
        assert_eq!(
            publish_result(&published, Ok(Some("0.3.0".into())))
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            publish_result(&published, Ok(None)).await.unwrap(),
            Some(String::new())
        );
    }
    #[tokio::test]
    async fn disabling_release_checks_interrupts_the_wait() {
        let (enabled, mut changes) = tokio::sync::watch::channel(true);
        let waiting = tokio::spawn(async move {
            wait_for_release_check(&mut changes, std::time::Duration::from_secs(30)).await
        });
        tokio::task::yield_now().await;

        enabled.send_replace(false);

        tokio::time::timeout(std::time::Duration::from_millis(100), waiting)
            .await
            .expect("disable wakes the wait")
            .expect("task did not panic");
    }
    #[tokio::test]
    async fn a_result_from_before_a_disable_stays_unpublished() {
        let published = PublishedUpdate::default();
        published.set_enabled(true).await;
        assert_eq!(
            published.publish(Some("0.2.0".into())).await,
            Some("0.2.0".into())
        );

        // The daemon disables checks: the flag and the known release are cleared in one
        // write, and a check that started earlier and finishes afterwards is dropped
        // under that same lock rather than through an enabled flag it copied beforehand.
        assert!(published.set_enabled(false).await);

        assert_eq!(
            publish_result(&published, Ok(Some("0.3.0".into())))
                .await
                .expect("the check itself succeeded"),
            None
        );
        assert_eq!(published.get().await, "");
    }
}

/// The signal wiring has no feature gate of its own; this module keeps it pinned
/// separately so it runs in every configuration the daemon can be built in.
#[cfg(all(test, unix))]
mod signal_tests {
    use super::*;

    /// Pins the shutdown wiring end to end: once this process is told to terminate,
    /// `Command::Shutdown` must reach the engine's command queue. The signal is real —
    /// `kill(1)` stands in for the signal-raising API std does not have, the
    /// alternative being a libc dependency just for this test — and it travels the
    /// helper's whole path: registration, stream, select, channel send.
    #[tokio::test]
    async fn a_termination_signal_reaches_the_command_queue() {
        let (commands, mut queue) = mpsc::channel(COMMAND_QUEUE);
        shutdown_signals(commands).expect("the SIGTERM and SIGINT streams register");

        let delivered = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(std::process::id().to_string())
            .status()
            .expect("kill(1) is on the test host");
        assert!(
            delivered.success(),
            "the signal was delivered to this process"
        );

        let command = tokio::time::timeout(std::time::Duration::from_secs(5), queue.recv())
            .await
            .expect("the watcher wakes on its own")
            .expect("the watcher keeps the queue open until it is aborted");
        assert!(matches!(command, Command::Shutdown));
    }
}
