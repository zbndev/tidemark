//! The poll loop: credentials in, snapshots out, history on the way past.
//!
//! One task owns every account and the database, which is why nothing here is behind a
//! mutex. Fetches run concurrently — there will be five providers and the slowest measured
//! one takes 2.7 s — but ingest and publication happen back in the owning task, in order.
//!
//! Finished statuses leave through a channel rather than being written to D-Bus here. That
//! is what keeps this module testable without a bus: the tests below run the real loop with
//! a fake provider and a fake keyring and read the same updates the daemon publishes.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tidemark_core::config::Config;
use tidemark_core::providers::{Credential, Provider, ProviderError};
use tidemark_core::secrets::{Kind, SecretError, Secrets};
use tidemark_core::storage::{History, IngestReport};
use tidemark_types::{
    AccountId, CredentialKind, HistoryPoint, ProviderId, ProviderOption, ProviderState,
    ProviderStatus, Snapshot, Timestamp, WindowKey, WindowStatus,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use crate::notify::{self, Notifier};
use crate::scheduler::{self, Situation};

/// How often the history is thinned. Thinning only touches points older than ninety days,
/// so there is nothing to gain from doing it more often than once a day.
pub const THIN_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// Consumption must move by more than this to count as movement. Percentages that arrive
/// as ratios multiplied out wobble in the last decimal place; a wobble is not a session.
const CHANGE_EPSILON: f64 = 0.01;

/// A change the publisher task must apply to its shared view of the accounts.
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "the published status wire shape stays owned and unboxed at this five-account scale"
)]
pub enum Publication {
    /// Add or replace one account status.
    Changed(ProviderStatus),
    /// Remove the exact account from the published topology.
    Removed {
        /// Stable provider slug.
        provider: String,
        /// Stable account name.
        account: String,
    },
}

/// What the D-Bus interface asks the loop to do.
#[derive(Debug)]
pub enum Command {
    /// Poll now: one provider by slug, or every account when `None`.
    Refresh(Option<String>),
    /// A stored credential or a setting changed. Re-reads the settings file, drops the
    /// built client so the new credential is picked up, and polls now.
    ///
    /// Separate from [`Command::Refresh`] because it is the only thing that must survive a
    /// provider currently backing off: a user who has just pasted a key is owed an answer
    /// now, not in fifty minutes.
    Reload {
        /// Provider slug, or `None` for every account.
        provider: Option<String>,
    },
    /// Add one provider's default account and report the persisted topology result.
    AddProvider {
        /// Stable provider slug.
        provider: String,
        /// Completion sent after persistence and in-memory mutation finish.
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Remove one exact provider/account pair and report the persisted topology result.
    RemoveProvider {
        /// Stable provider slug.
        provider: String,
        /// Stable account name.
        account: String,
        /// Completion sent after persistence and in-memory mutation finish.
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Validate and persist one provider setting in the same queue as topology writes.
    SetOption {
        /// Stable provider slug.
        provider: String,
        /// Stable account name.
        account: String,
        /// Provider-declared option name.
        name: String,
        /// One of the option's declared values.
        value: String,
        /// Completion sent after persistence and in-memory state agree.
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Switch one window's notifications on or off, in the same queue as topology writes.
    SetWindowNotify {
        /// Stable provider slug.
        provider: String,
        /// Stable account name.
        account: String,
        /// The window key, as published in the account's status.
        window: String,
        /// Whether the user wants to hear about it.
        enabled: bool,
        /// Completion sent after persistence and in-memory state agree.
        reply: oneshot::Sender<Result<(), String>>,
    },
    /// Reads the stored points in one account window's open segment.
    CurrentSegment {
        /// Stable provider slug.
        provider: String,
        /// Stable account name.
        account: String,
        /// The window identity published to clients.
        window: String,
        /// Completion with oldest-first points, or a named account/storage error.
        reply: oneshot::Sender<Result<Vec<HistoryPoint>, String>>,
    },
    /// Stop the loop.
    Shutdown,
}

/// Builds a provider client once a credential is in hand.
///
/// A closure rather than a trait because that is all it is: the five providers are
/// constructed five different ways, and only the daemon knows which one it is holding.
pub type Factory = Box<
    dyn Fn(Credential, &BTreeMap<String, String>) -> Result<Arc<dyn Provider>, ProviderError>
        + Send
        + Sync,
>;

/// Rebuilds a provider client that finds its own credential, from its settings alone.
///
/// The counterpart of [`Factory`] for a provider registered with [`Account::with_client`].
/// Such a provider has no stored key for the engine to hand it, but it can still have
/// settings — and a setting that never reached the built client would take effect only on
/// the next daemon restart.
pub type Rebuild = Box<
    dyn Fn(&BTreeMap<String, String>) -> Result<Arc<dyn Provider>, ProviderError> + Send + Sync,
>;

/// One account the daemon watches.
pub struct Account {
    provider: ProviderId,
    account: AccountId,
    factory: Option<Factory>,
    rebuild: Option<Rebuild>,
    client: Option<Arc<dyn Provider>>,
    status: ProviderStatus,
    failures: u32,
    retry_after: Option<Duration>,
    last_change_at: Option<Timestamp>,
    due: Instant,
}

/// Everything about an account that is fixed at registration, kept together so both
/// constructors fill it the same way.
fn describe(status: &mut ProviderStatus, kind: CredentialKind) {
    status.credential = Some(kind.as_wire().to_owned());
}

impl std::fmt::Debug for Account {
    /// Written by hand because [`Factory`] is a boxed closure and cannot derive one — and
    /// because the client behind it holds a credential, which must never reach a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Account")
            .field("provider", &self.provider)
            .field("account", &self.account)
            .field("state", &self.status.state)
            .field("has_client", &self.client.is_some())
            .field("failures", &self.failures)
            .finish_non_exhaustive()
    }
}

impl Account {
    /// An account whose client is built from a stored key the first time it is polled.
    pub fn new(provider: ProviderId, account: AccountId, factory: Factory) -> Self {
        let mut status = ProviderStatus::pending(&provider, &account);
        describe(&mut status, CredentialKind::Key);
        Self {
            status,
            provider,
            account,
            factory: Some(factory),
            rebuild: None,
            client: None,
            failures: 0,
            retry_after: None,
            last_change_at: None,
            due: Instant::now(),
        }
    }

    /// An account with its client already in hand.
    ///
    /// This is the shape for providers that own credential discovery themselves, such as
    /// Claude's CLI file and Antigravity's future `agy` session.
    pub fn with_client(client: Arc<dyn Provider>) -> Self {
        let (provider, account) = (client.id(), client.account());
        let mut status = ProviderStatus::pending(&provider, &account);
        describe(&mut status, CredentialKind::External);
        Self {
            status,
            provider,
            account,
            factory: None,
            rebuild: None,
            client: Some(client),
            failures: 0,
            retry_after: None,
            last_change_at: None,
            due: Instant::now(),
        }
    }

    /// How to build this account's client again when its settings change.
    ///
    /// Only for [`Account::with_client`] accounts: one built by a [`Factory`] is already
    /// rebuilt from its stored key whenever the engine drops it.
    pub fn with_rebuild(mut self, rebuild: Rebuild) -> Self {
        self.rebuild = Some(rebuild);
        self
    }

    /// Whether dropping this account's client is safe, because it can be built again.
    fn rebuildable(&self) -> bool {
        self.factory.is_some() || self.rebuild.is_some()
    }

    /// Says how this account is authenticated, and therefore what a credentials dialog
    /// should offer for it.
    pub fn with_credential(mut self, kind: CredentialKind) -> Self {
        self.status.credential = Some(kind.as_wire().to_owned());
        self
    }

    /// One sentence on where the credential comes from.
    pub fn with_hint(mut self, hint: &str) -> Self {
        self.status.credential_hint = Some(hint.to_owned());
        self
    }

    /// Replaces the published settings of this provider.
    pub fn with_options(mut self, options: Vec<ProviderOption>) -> Self {
        self.status.options = options;
        self
    }

    /// Replaces the set of windows this account notifies about.
    pub fn with_notify(mut self, windows: Vec<String>) -> Self {
        self.status.notify = windows;
        self
    }

    /// Which provider this account belongs to.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read only by engine and registry tests")
    )]
    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }

    /// The settings as a plain map, which is what a [`Factory`] is handed.
    fn option_values(&self) -> BTreeMap<String, String> {
        self.status
            .options
            .iter()
            .map(|option| (option.name.clone(), option.value.clone()))
            .collect()
    }

    /// The status as last published. Read by the registry's tests, which check that every
    /// account describes its own credentials before anything has polled it.
    pub fn status(&self) -> &ProviderStatus {
        &self.status
    }

    fn set_state(&mut self, state: ProviderState, message: Option<String>) {
        self.status.set_state(state, message);
    }

    fn state(&self) -> ProviderState {
        self.status.state().unwrap_or(ProviderState::Pending)
    }
}

/// The poll loop.
#[derive(Debug)]
pub struct Engine {
    accounts: Vec<Account>,
    history: History,
    secrets: Arc<dyn Secrets>,
    updates: mpsc::Sender<Publication>,
    /// Where the settings are read from on a reload. Held rather than resolved from the
    /// environment each time, so the loop can be run over a settings file of a test's own.
    config_path: std::path::PathBuf,
    last_thin: Option<Instant>,
    notifier: Arc<dyn Notifier>,
}

impl Engine {
    /// Assembles the loop. Nothing is polled until [`Engine::run`].
    pub fn new(
        accounts: Vec<Account>,
        history: History,
        secrets: Arc<dyn Secrets>,
        updates: mpsc::Sender<Publication>,
        config_path: std::path::PathBuf,
        notifier: Arc<dyn Notifier>,
    ) -> Self {
        Self {
            accounts,
            history,
            secrets,
            updates,
            config_path,
            last_thin: None,
            notifier,
        }
    }

    /// Publishes every account as pending.
    ///
    /// Called before the first poll so that a client connecting in the first second of the
    /// daemon's life gets the list of accounts with a state on each, rather than an empty
    /// array it cannot tell apart from "nothing is configured".
    pub async fn announce(&self) {
        for account in &self.accounts {
            let _ = self
                .updates
                .send(Publication::Changed(account.status.clone()))
                .await;
        }
    }

    /// Adds the default account for a compiled-in provider without restarting the loop.
    pub async fn add_provider(&mut self, provider: &str) -> Result<(), String> {
        let mut config = Config::at(self.config_path.clone()).map_err(|error| error.to_string())?;
        if self.accounts.iter().any(|account| {
            account.provider.as_str() == provider && account.account == AccountId::default()
        }) {
            return Ok(());
        }

        let Some(mut account) = crate::registry::account(provider, &self.secrets, &config)
            .map_err(|error| error.to_string())?
        else {
            return Err(format!(
                "provider {provider} is not supported by this build"
            ));
        };
        config
            .add_provider(provider)
            .map_err(|error| error.to_string())?;

        account.due = Instant::now();
        self.accounts.push(account);
        self.probe_credentials(Some(provider)).await;
        let status = self
            .accounts
            .last()
            .expect("the account was just pushed")
            .status
            .clone();
        let _ = self.updates.send(Publication::Changed(status)).await;
        Ok(())
    }

    /// Removes an exact account from future polling without touching its history.
    pub async fn remove_provider(&mut self, provider: &str, account: &str) -> Result<(), String> {
        let Some(index) = self.accounts.iter().position(|configured| {
            configured.provider.as_str() == provider && configured.account.as_str() == account
        }) else {
            return Err(format!("account {provider}/{account} is not configured"));
        };

        let mut config = Config::at(self.config_path.clone()).map_err(|error| error.to_string())?;
        config
            .remove_provider(provider)
            .map_err(|error| error.to_string())?;

        let removed = self.accounts.remove(index);
        let _ = self
            .updates
            .send(Publication::Removed {
                provider: removed.provider.as_str().to_owned(),
                account: removed.account.as_str().to_owned(),
            })
            .await;
        Ok(())
    }

    /// Changes one provider option as a serialized configuration transaction.
    pub async fn set_option(
        &mut self,
        provider: &str,
        account: &str,
        name: &str,
        value: &str,
    ) -> Result<(), String> {
        let Some(index) = self.accounts.iter().position(|configured| {
            configured.provider.as_str() == provider && configured.account.as_str() == account
        }) else {
            return Err(format!("account {provider}/{account} is not configured"));
        };
        let option = self.accounts[index]
            .status
            .options
            .iter()
            .find(|option| option.name == name)
            .ok_or_else(|| format!("{provider} has no setting called {name}"))?;
        if !option.choices.iter().any(|choice| choice.value == value) {
            return Err(format!("{value} is not one of the values {name} can take"));
        }

        let mut config = Config::at(self.config_path.clone()).map_err(|error| error.to_string())?;
        config
            .set_option(provider, name, value)
            .map_err(|error| error.to_string())?;
        self.accounts[index].status.options = crate::registry::options(provider, &config);
        if self.accounts[index].rebuildable() {
            self.accounts[index].client = None;
        }
        self.accounts[index].failures = 0;
        self.accounts[index].retry_after = None;
        self.accounts[index].due = Instant::now();
        self.probe_credentials(Some(provider)).await;
        Ok(())
    }

    /// Reads the active history segment for one configured account/window pair.
    ///
    /// The engine owns the database, so even this short read goes through its command queue
    /// rather than giving an interface client a second SQLite connection.
    fn current_segment(
        &self,
        provider: &str,
        account: &str,
        window: &str,
    ) -> Result<Vec<HistoryPoint>, String> {
        if !self.accounts.iter().any(|configured| {
            configured.provider.as_str() == provider && configured.account.as_str() == account
        }) {
            return Err(format!("account {provider}/{account} is not configured"));
        }

        self.history
            .current_points(provider, account, &WindowKey::named(window))
            .map(|points| {
                points
                    .into_iter()
                    .map(|point| HistoryPoint {
                        captured_at: point.captured_at.as_unix(),
                        used_percent: point.used_percent,
                    })
                    .collect()
            })
            .map_err(|error| error.to_string())
    }

    /// Runs until a [`Command::Shutdown`] arrives or the command channel closes.
    pub async fn run(&mut self, commands: &mut mpsc::Receiver<Command>) {
        loop {
            let next_due = self
                .accounts
                .iter()
                .map(|account| account.due)
                .min()
                .unwrap_or_else(|| Instant::now() + scheduler::BASELINE);

            tokio::select! {
                _ = tokio::time::sleep_until(next_due.into()) => {}
                command = commands.recv() => match command {
                    Some(Command::Refresh(target)) => self.mark_due(target.as_deref()),
                    Some(Command::Reload { provider }) => self.reload(provider.as_deref()).await,
                    Some(Command::AddProvider { provider, reply }) => {
                        let result = self.add_provider(&provider).await;
                        let _ = reply.send(result);
                    }
                    Some(Command::RemoveProvider { provider, account, reply }) => {
                        let result = self.remove_provider(&provider, &account).await;
                        let _ = reply.send(result);
                    }
                    Some(Command::SetOption { provider, account, name, value, reply }) => {
                        let result = self.set_option(&provider, &account, &name, &value).await;
                        let _ = reply.send(result);
                    }
                    Some(Command::SetWindowNotify { provider, account, window, enabled, reply }) => {
                        let result = self
                            .set_window_notify(&provider, &account, &window, enabled)
                            .await;
                        let _ = reply.send(result);
                    }
                    Some(Command::CurrentSegment { provider, account, window, reply }) => {
                        let result = self.current_segment(&provider, &account, &window);
                        let _ = reply.send(result);
                    }
                    Some(Command::Shutdown) | None => return,
                },
            }

            self.thin_if_due();
            self.poll_due(Instant::now()).await;
        }
    }

    /// Brings every account whose next poll has come due up to date.
    pub async fn poll_due(&mut self, now: Instant) {
        let due: Vec<usize> = self
            .accounts
            .iter()
            .enumerate()
            .filter(|(_, account)| account.due <= now)
            .map(|(index, _)| index)
            .collect();

        // Credentials first, and sequentially: each is one local D-Bus round trip, and a
        // provider whose key is missing must reach its state without a network attempt.
        for &index in &due {
            self.ensure_client(index).await;
        }

        let mut fetches = JoinSet::new();
        for &index in &due {
            if let Some(client) = self.accounts[index].client.clone() {
                fetches.spawn(async move { (index, client.fetch().await) });
            }
        }

        while let Some(joined) = fetches.join_next().await {
            match joined {
                Ok((index, result)) => self.apply(index, result).await,
                Err(error) => tracing::error!(%error, "a fetch task did not finish"),
            }
        }

        // Accounts with no client got their state from `ensure_client` and never reached a
        // fetch; they still need rescheduling, or they would come due forever.
        for &index in &due {
            if self.accounts[index].client.is_none() {
                self.reschedule(index).await;
            }
        }
    }

    /// Loads the credential and builds the client, unless one is already in hand.
    async fn ensure_client(&mut self, index: usize) {
        if self.accounts[index].client.is_some() {
            return;
        }
        if let Some(rebuild) = self.accounts[index].rebuild.as_ref() {
            // Owns its credential discovery: there is no stored key to read, so the
            // settings are the whole of what the replacement is built from.
            let options = self.accounts[index].option_values();
            match rebuild(&options) {
                Ok(client) => self.accounts[index].client = Some(client),
                Err(error) => {
                    self.accounts[index].failures = self.accounts[index].failures.saturating_add(1);
                    self.accounts[index]
                        .set_state(ProviderState::Unreachable, Some(error.to_string()));
                }
            }
            return;
        }
        if self.accounts[index].factory.is_none() {
            // No factory and no client: nothing could ever poll this account. Only
            // reachable if a caller built one that way.
            self.accounts[index].set_state(
                ProviderState::NoCredential,
                Some("no way to build a client for this account".into()),
            );
            return;
        }

        let secrets = Arc::clone(&self.secrets);
        let provider = self.accounts[index].provider.clone();
        let account = self.accounts[index].account.clone();
        let found = secrets.get(Kind::Key, &provider, &account).await;

        // Resolved before the account is borrowed mutably, because building the client
        // reads the factory off the very account the outcome is written back to.
        let loaded = match found {
            Ok(Some(credential)) => {
                let options = self.accounts[index].option_values();
                let factory = self.accounts[index]
                    .factory
                    .as_ref()
                    .expect("checked just above");
                match factory(credential, &options) {
                    Ok(client) => {
                        tracing::debug!(provider = %provider, "credential loaded");
                        Loaded::Client(client)
                    }
                    Err(error) => {
                        Loaded::State(ProviderState::Unreachable, Some(error.to_string()))
                    }
                }
            }
            Ok(None) => Loaded::State(ProviderState::NoCredential, None),
            // Not a failure. The user has not logged in yet, or has not unlocked the
            // keyring; the daemon waits, and says so, rather than reporting a problem the
            // user did not cause. See `tidemark_core::secrets`.
            Err(SecretError::Locked) => Loaded::State(ProviderState::WaitingForKeyring, None),
            Err(error @ SecretError::NotUtf8) => {
                Loaded::State(ProviderState::NoCredential, Some(error.to_string()))
            }
            Err(error @ SecretError::Dbus(_)) => {
                Loaded::State(ProviderState::KeyringUnavailable, Some(error.to_string()))
            }
        };

        let target = &mut self.accounts[index];
        match loaded {
            Loaded::Client(client) => target.client = Some(client),
            Loaded::State(state, message) => {
                if state == ProviderState::Unreachable {
                    target.failures = target.failures.saturating_add(1);
                }
                target.set_state(state, message);
            }
        }
    }

    /// Files one fetch result and publishes the account.
    async fn apply(&mut self, index: usize, result: Result<Snapshot, ProviderError>) {
        match result {
            Ok(snapshot) => {
                self.record(index, &snapshot).await;
                let account = &mut self.accounts[index];
                account.failures = 0;
                account.retry_after = None;
                account.status.set_reading(&snapshot);
            }
            Err(error) => {
                let state = state_for(&error);
                tracing::warn!(
                    provider = %self.accounts[index].provider,
                    state = %state,
                    %error,
                    "poll failed"
                );
                let account = &mut self.accounts[index];
                account.failures = account.failures.saturating_add(1);
                account.retry_after = error.retry_after();
                account.set_state(state, Some(error.to_string()));
                if state == ProviderState::CredentialRejected && account.factory.is_some() {
                    // Re-read the key next time round: the user's fix is to store a new
                    // one, and nothing else would tell us they had.
                    account.client = None;
                }
            }
        }
        self.reschedule(index).await;
    }

    /// Writes a reading to history and notes whether consumption moved.
    ///
    /// A database failure is logged and swallowed on purpose: history is what the forecast
    /// is built from, but a daemon that stopped publishing numbers because it could not
    /// write them down would turn a recoverable disk problem into a blank interface.
    async fn record(&mut self, index: usize, snapshot: &Snapshot) {
        let moved = consumption_moved(&self.accounts[index].status.windows, snapshot);

        match self.history.ingest(snapshot) {
            Ok(report) => {
                for outcome in report.segments_opened() {
                    tracing::info!(
                        provider = %snapshot.provider,
                        window = %outcome.key,
                        segment = outcome.segment,
                        boundary = ?outcome.boundary,
                        "window rolled over"
                    );
                }
                let stored = report.windows.iter().filter(|w| w.stored).count();
                tracing::debug!(
                    provider = %snapshot.provider,
                    windows = report.windows.len(),
                    stored,
                    stale = report.stale.len(),
                    "reading filed"
                );
                self.raise_notices(index, snapshot, &report).await;
            }
            Err(error) => tracing::error!(
                provider = %snapshot.provider,
                %error,
                "could not write to the history database"
            ),
        }

        let account = &mut self.accounts[index];
        if moved || account.last_change_at.is_none() {
            account.last_change_at = Some(snapshot.captured_at);
        }
    }

    /// Tells the user what this reading changed, for the windows they asked to hear about.
    ///
    /// Runs after the reading is filed, because the segment a notification deduplicates
    /// against is what filing it decided. A reading the history refused is not notified
    /// about at all: without the report there is no segment to key the dedup on, and a
    /// warning that cannot be deduplicated is a warning every five minutes.
    async fn raise_notices(&mut self, index: usize, snapshot: &Snapshot, report: &IngestReport) {
        let opted = self.accounts[index].status.notify.clone();
        if opted.is_empty() {
            return;
        }
        let provider = snapshot.provider.as_str().to_owned();
        let account = snapshot.account.as_str().to_owned();

        for outcome in &report.windows {
            if !opted.iter().any(|key| key == outcome.key.as_str()) {
                continue;
            }
            let Some(window) = snapshot
                .windows
                .iter()
                .find(|window| window.key == outcome.key)
            else {
                continue;
            };

            let mut already = Vec::new();
            for kind in notify::Kind::ALL {
                match self.history.notice_sent(
                    &provider,
                    &account,
                    &outcome.key,
                    outcome.segment,
                    kind.as_str(),
                ) {
                    Ok(true) => already.push(kind),
                    Ok(false) => {}
                    // Without the dedup table there is nothing to bound repetition, and a
                    // notification every poll forever would be worse than a missed one.
                    Err(error) => {
                        tracing::error!(%error, "cannot read which notifications went out");
                        return;
                    }
                }
            }

            for decided in notify::decide(
                window.used_percent,
                outcome.boundary.is_some(),
                &already,
            ) {
                let notice = notify::compose(&provider, window, decided.kind, snapshot.captured_at);
                if let Err(error) = self.notifier.send(&notice).await {
                    // Nothing is recorded, so the next poll says it again. The daemon can
                    // easily outlive the session it started in.
                    tracing::debug!(provider = %provider, %error, "notification not delivered");
                    break;
                }
                tracing::info!(provider = %provider, window = %outcome.key, kind = decided.kind.as_str(), "notified");
                for kind in decided.settles {
                    if let Err(error) = self.history.record_notice(
                        &provider,
                        &account,
                        &outcome.key,
                        outcome.segment,
                        kind.as_str(),
                        snapshot.captured_at,
                    ) {
                        tracing::error!(%error, "could not record a delivered notification");
                    }
                }
            }
        }
    }

    /// Switches one window's notifications on or off as a serialized configuration change.
    ///
    /// Switching **on** is checked against the windows the account currently reports, so a
    /// typo over D-Bus is an error rather than a line in the settings file that never does
    /// anything. Switching **off** is not: a provider that has temporarily stopped
    /// reporting a window must not also trap the user into hearing about it.
    pub async fn set_window_notify(
        &mut self,
        provider: &str,
        account: &str,
        window: &str,
        enabled: bool,
    ) -> Result<(), String> {
        let Some(index) = self.accounts.iter().position(|configured| {
            configured.provider.as_str() == provider && configured.account.as_str() == account
        }) else {
            return Err(format!("account {provider}/{account} is not configured"));
        };
        if enabled
            && !self.accounts[index]
                .status
                .windows
                .iter()
                .any(|published| published.key == window)
        {
            return Err(format!("{provider} is not reporting a window called {window}"));
        }

        let mut config = Config::at(self.config_path.clone()).map_err(|error| error.to_string())?;
        config
            .set_window_notify(provider, window, enabled)
            .map_err(|error| error.to_string())?;
        self.accounts[index].status.notify = config
            .notify_windows(provider)
            .map_err(|error| error.to_string())?;
        let _ = self
            .updates
            .send(Publication::Changed(self.accounts[index].status.clone()))
            .await;
        Ok(())
    }

    /// Chooses the next poll time and publishes the account.
    async fn reschedule(&mut self, index: usize) {
        let now = Timestamp::now();
        let account = &self.accounts[index];
        let situation = Situation {
            state: account.state(),
            failures: account.failures,
            retry_after: account.retry_after,
            seconds_to_next_reset: soonest_reset(&account.status.windows, now),
            seconds_since_change: account
                .last_change_at
                .map(|changed| changed.seconds_until(now)),
        };
        let interval = scheduler::next_interval(&situation);

        let account = &mut self.accounts[index];
        account.due = Instant::now() + interval;
        account.status.next_poll_at = Some(
            now.saturating_add_seconds(interval.as_secs() as i64)
                .as_unix(),
        );

        tracing::debug!(
            provider = %account.provider,
            state = %situation.state,
            seconds = interval.as_secs(),
            "next poll scheduled"
        );
        let _ = self
            .updates
            .send(Publication::Changed(account.status.clone()))
            .await;
    }

    /// Brings accounts forward so the next turn of the loop polls them.
    ///
    /// The client is dropped as well, so the credential is read again. A manual refresh is
    /// what the user reaches for straight after saving a key, and it would be a poor
    /// interface that answered it with the old one.
    fn mark_due(&mut self, target: Option<&str>) {
        let now = Instant::now();
        for account in &mut self.accounts {
            if target.is_none_or(|slug| account.provider.as_str() == slug) {
                account.due = now;
                if account.factory.is_some() {
                    account.client = None;
                }
            }
        }
    }

    /// Acts on a credential or setting having changed.
    ///
    /// The settings file is read again rather than remembered, so the published options
    /// and the client that is about to be built agree with what is on disk. The backoff is
    /// cleared with the client: the failures that earned it were the old credential's.
    pub async fn reload(&mut self, target: Option<&str>) {
        let config = match Config::at(self.config_path.clone()) {
            Ok(config) => Some(config),
            Err(error) => {
                // A file the user has broken is left alone and reported. The accounts keep
                // the settings they were built with rather than silently reverting to the
                // defaults, which would look like the edit had been undone.
                tracing::warn!(%error, "could not read the settings file");
                None
            }
        };
        for account in &mut self.accounts {
            if !target.is_none_or(|slug| account.provider.as_str() == slug) {
                continue;
            }
            if let Some(config) = &config {
                account.status.options =
                    crate::registry::options(account.provider.as_str(), config);
                account.status.notify = crate::registry::notify(account.provider.as_str(), config);
            }
            if account.rebuildable() {
                account.client = None;
            }
            account.failures = 0;
            account.retry_after = None;
            account.due = Instant::now();
        }
        self.probe_credentials(target).await;
    }

    /// Records, for each account, whether Tidemark itself holds a credential for it.
    ///
    /// Asked rather than inferred, and asked rarely — at startup and whenever a credential
    /// changes — because the answer only moves when the user moves it. A locked keyring
    /// leaves the answer unknown rather than answering "no": the dialog would otherwise
    /// offer to replace a key that is there and simply out of reach.
    pub async fn probe_credentials(&mut self, target: Option<&str>) {
        for index in 0..self.accounts.len() {
            let account = &self.accounts[index];
            if !target.is_none_or(|slug| account.provider.as_str() == slug) {
                continue;
            }
            let Some(kind) = account.status.credential_kind().and_then(stored_kind) else {
                // Nothing of ours to look for: the credential belongs to something else on
                // the machine.
                self.accounts[index].status.has_credential = None;
                continue;
            };
            let provider = account.provider.clone();
            let name = account.account.clone();
            let found = self.secrets.get(kind, &provider, &name).await;
            let held = match found {
                Ok(found) => Some(found.is_some()),
                Err(SecretError::Locked) => None,
                Err(error) => {
                    tracing::debug!(provider = %provider, %error, "cannot see stored credentials");
                    None
                }
            };
            self.accounts[index].status.has_credential = held;
        }
    }

    fn thin_if_due(&mut self) {
        if self
            .last_thin
            .is_some_and(|last| last.elapsed() < THIN_INTERVAL)
        {
            return;
        }
        self.last_thin = Some(Instant::now());
        match self.history.thin(Timestamp::now()) {
            Ok(0) => {}
            Ok(removed) => tracing::info!(removed, "thinned history older than ninety days"),
            Err(error) => tracing::error!(%error, "could not thin the history"),
        }
    }

    /// The accounts, so a test can look at what a poll left behind.
    #[cfg(test)]
    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }
}

/// Which schema a credential of this kind is stored under, or `None` where Tidemark stores
/// nothing of its own.
pub fn stored_kind(credential: CredentialKind) -> Option<Kind> {
    match credential {
        CredentialKind::Key => Some(Kind::Key),
        CredentialKind::OAuth => Some(Kind::Token),
        CredentialKind::External => None,
    }
}

/// What asking the keyring for a credential produced.
enum Loaded {
    /// A client, ready to poll.
    Client(Arc<dyn Provider>),
    /// No client, and the state that explains why.
    State(ProviderState, Option<String>),
}

/// Which state a failed fetch leaves the account in.
fn state_for(error: &ProviderError) -> ProviderState {
    match error {
        ProviderError::NoCredential => ProviderState::NoCredential,
        ProviderError::KeyringLocked => ProviderState::WaitingForKeyring,
        ProviderError::KeyringUnavailable(_) => ProviderState::KeyringUnavailable,
        ProviderError::Credential { .. } => ProviderState::CredentialRejected,
        ProviderError::RateLimited { .. } => ProviderState::RateLimited,
        ProviderError::Malformed(_) => ProviderState::Malformed,
        ProviderError::Client(_)
        | ProviderError::Transport(_)
        | ProviderError::Http { .. }
        | ProviderError::Local(_) => ProviderState::Unreachable,
    }
}

/// Seconds until the soonest reset among the published windows, if any window said.
fn soonest_reset(windows: &[WindowStatus], now: Timestamp) -> Option<i64> {
    windows
        .iter()
        .filter_map(|window| window.resets_at)
        .map(|resets_at| resets_at - now.as_unix())
        .min()
}

/// Whether any window's consumption differs from what was last published.
///
/// A window that only appears in one of the two is movement: a quota pool that turned up
/// or went away is exactly the kind of change that should wake the polling back up.
fn consumption_moved(published: &[WindowStatus], snapshot: &Snapshot) -> bool {
    if published.len() != snapshot.windows.len() {
        return true;
    }
    snapshot.windows.iter().any(|window| {
        published
            .iter()
            .find(|previous| previous.key == window.key.as_str())
            .is_none_or(|previous| {
                (previous.used_percent - window.used_percent).abs() > CHANGE_EPSILON
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::{Notice, NotifyError, Notifier};
    use std::sync::Mutex;
    use tidemark_core::providers::BoxFuture;
    use tidemark_types::{Window, WindowKey, WindowLength};

    #[test]
    fn a_missing_cli_credential_has_its_own_published_state() {
        assert_eq!(
            state_for(&ProviderError::NoCredential),
            ProviderState::NoCredential
        );
    }

    /// A provider that answers whatever the test tells it to, without a network.
    #[derive(Debug)]
    struct Fake {
        id: ProviderId,
        answers: Mutex<Vec<Result<Snapshot, ProviderError>>>,
    }

    impl Fake {
        fn new(answers: Vec<Result<Snapshot, ProviderError>>) -> Arc<Self> {
            Arc::new(Self {
                id: ProviderId::new("fake"),
                answers: Mutex::new(answers.into_iter().rev().collect()),
            })
        }
    }

    impl Provider for Fake {
        fn id(&self) -> ProviderId {
            self.id.clone()
        }

        fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
            let answer = self
                .answers
                .lock()
                .expect("no test panics while holding this")
                .pop()
                .unwrap_or(Err(ProviderError::Http { status: 503 }));
            Box::pin(async move { answer })
        }
    }

    /// A keyring that says exactly one thing, including the thing a real one cannot be
    /// made to say on a developer's machine without throwing unlock prompts at every
    /// other application on the desktop.
    #[derive(Debug)]
    struct Keyring(fn() -> Result<Option<Credential>, SecretError>);

    impl Secrets for Keyring {
        fn get<'a>(
            &'a self,
            _kind: Kind,
            _provider: &'a ProviderId,
            _account: &'a AccountId,
        ) -> BoxFuture<'a, Result<Option<Credential>, SecretError>> {
            let answer = (self.0)();
            Box::pin(async move { answer })
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

    /// Consecutive readings, five minutes apart. A reading whose timestamp does not move
    /// is filed as a repeat of the one before it and never reaches the notifier, so tests
    /// that poll twice have to say which poll is which.
    fn reading(step: i64, used: f64, resets_in: i64) -> Snapshot {
        let captured_at = Timestamp::now().saturating_add_seconds(step * 300 - 3600);
        Snapshot {
            captured_at,
            windows: vec![Window {
                key: WindowKey::for_length(WindowLength::from_secs(18_000).expect("nonzero")),
                title: "5 hours".into(),
                used_percent: used,
                resets_at: Some(captured_at.saturating_add_seconds(resets_in)),
                length: WindowLength::from_secs(18_000),
            }],
            ..snapshot(used, resets_in)
        }
    }

    fn snapshot(used: f64, resets_in: i64) -> Snapshot {
        let now = Timestamp::now();
        Snapshot {
            provider: ProviderId::new("fake"),
            account: AccountId::default(),
            captured_at: now,
            windows: vec![Window {
                key: WindowKey::for_length(WindowLength::from_secs(18_000).expect("nonzero")),
                title: "5 hours".into(),
                used_percent: used,
                resets_at: Some(now.saturating_add_seconds(resets_in)),
                length: WindowLength::from_secs(18_000),
            }],
            details: Vec::new(),
        }
    }

    struct Harness {
        engine: Engine,
        updates: mpsc::Receiver<Publication>,
        config_path: std::path::PathBuf,
        notices: Arc<Recorder>,
    }

    /// A notification server that keeps what it was handed, and can be told to refuse.
    #[derive(Debug, Default)]
    struct Recorder {
        sent: Mutex<Vec<Notice>>,
        refusals: Mutex<usize>,
    }

    impl Recorder {
        fn refusing(times: usize) -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                refusals: Mutex::new(times),
            })
        }

        fn summaries(&self) -> Vec<String> {
            self.sent
                .lock()
                .expect("no test panics while holding this")
                .iter()
                .map(|notice| notice.summary.clone())
                .collect()
        }
    }

    impl Notifier for Recorder {
        fn send(&self, notice: &Notice) -> BoxFuture<'_, Result<(), NotifyError>> {
            let mut refusals = self.refusals.lock().expect("no test panics here");
            if *refusals > 0 {
                *refusals -= 1;
                return Box::pin(async { Err(NotifyError::Unreachable) });
            }
            self.sent
                .lock()
                .expect("no test panics here")
                .push(notice.clone());
            Box::pin(async { Ok(()) })
        }
    }

    impl Harness {
        fn new(accounts: Vec<Account>, secrets: Arc<dyn Secrets>) -> Self {
            let (tx, rx) = mpsc::channel(64);
            let config_path = std::env::temp_dir().join("tidemark-engine-tests-absent.toml");
            let notices = Arc::new(Recorder::default());
            Self {
                engine: Engine::new(
                    accounts,
                    History::in_memory().expect("an in-memory database opens"),
                    secrets,
                    tx,
                    config_path.clone(),
                    Arc::clone(&notices) as Arc<dyn Notifier>,
                ),
                updates: rx,
                config_path,
                notices,
            }
        }

        async fn empty(name: &str) -> Self {
            Self::configured(name, &[]).await
        }

        async fn configured(name: &str, providers: &[&str]) -> Self {
            let config_path = std::env::temp_dir().join(format!(
                "tidemark-engine-{name}-{}.toml",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&config_path);
            let mut config = Config::at(config_path.clone()).expect("empty config parses");
            for provider in providers {
                config.add_provider(provider).expect("provider configured");
            }
            let secrets = unlocked();
            let accounts = crate::registry::accounts(&secrets, &config).expect("accounts build");
            let (tx, rx) = mpsc::channel(64);
            let notices = Arc::new(Recorder::default());
            Self {
                engine: Engine::new(
                    accounts,
                    History::in_memory().expect("an in-memory database opens"),
                    secrets,
                    tx,
                    config_path.clone(),
                    Arc::clone(&notices) as Arc<dyn Notifier>,
                ),
                updates: rx,
                config_path,
                notices,
            }
        }

        /// Seconds the only account will wait before its next poll, rounded — the
        /// scheduler's answer as the loop actually stored it.
        fn wait_secs(&self) -> u64 {
            self.engine.accounts()[0]
                .due
                .saturating_duration_since(Instant::now())
                .as_secs_f64()
                .round() as u64
        }

        /// Polls, having first brought the account forward the way `Refresh` does.
        async fn poll_again(&mut self) {
            self.engine.mark_due(None);
            self.engine.poll_due(Instant::now()).await;
        }

        fn published(&mut self) -> Vec<ProviderStatus> {
            let mut drained = Vec::new();
            while let Ok(publication) = self.updates.try_recv() {
                if let Publication::Changed(status) = publication {
                    drained.push(status);
                }
            }
            drained
        }
    }

    fn unlocked() -> Arc<dyn Secrets> {
        Arc::new(Keyring(|| Ok(Some(Credential::new("sk-test")))))
    }

    /// The engine over a settings file of the test's own, so a reload reads that and not
    /// whatever the developer running the suite happens to have configured.
    fn harness_with_config(accounts: Vec<Account>, config: std::path::PathBuf) -> Harness {
        let (tx, rx) = mpsc::channel(64);
        let notices = Arc::new(Recorder::default());
        Harness {
            engine: Engine::new(
                accounts,
                History::in_memory().expect("an in-memory database opens"),
                unlocked(),
                tx,
                config.clone(),
                Arc::clone(&notices) as Arc<dyn Notifier>,
            ),
            updates: rx,
            config_path: config,
            notices,
        }
    }

    #[tokio::test]
    async fn adding_a_provider_persists_announces_and_makes_it_due_now() {
        let mut harness = Harness::empty("runtime-add").await;
        harness.engine.add_provider("kimi").await.expect("added");

        assert_eq!(harness.engine.accounts().len(), 1);
        assert_eq!(harness.engine.accounts()[0].provider().as_str(), "kimi");
        let publication = harness.updates.recv().await.expect("announced");
        assert!(matches!(publication, Publication::Changed(status) if status.provider == "kimi"));
        let config = Config::at(harness.config_path.clone()).expect("parses");
        assert_eq!(config.providers().expect("readable"), ["kimi"]);
    }

    #[tokio::test]
    async fn removing_a_provider_stops_polling_and_keeps_history() {
        let mut harness = Harness::configured("runtime-remove", &["kimi"]).await;
        let before = harness.engine.history.point_count().expect("count");
        harness
            .engine
            .remove_provider("kimi", "default")
            .await
            .expect("removed");

        assert!(harness.engine.accounts().is_empty());
        assert_eq!(harness.engine.history.point_count().expect("count"), before);
        assert!(matches!(
            harness.updates.recv().await,
            Some(Publication::Removed { provider, account })
                if provider == "kimi" && account == "default"
        ));
        let config = Config::at(harness.config_path.clone()).expect("parses");
        assert!(config.providers().expect("readable").is_empty());
    }

    #[tokio::test]
    async fn changing_antigravity_usage_source_rebuilds_its_client() {
        // Antigravity owns its credential discovery, so it is registered with a client
        // rather than a factory — and a setting that only reached the client on the next
        // daemon restart would look like the choice had not been taken.
        let config_path = std::env::temp_dir().join(format!(
            "tidemark-engine-antigravity-source-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&config_path);
        let mut config = Config::at(config_path.clone()).expect("empty config parses");
        config.add_provider("antigravity").expect("configured");
        let secrets: Arc<dyn Secrets> = Arc::new(Keyring(|| Ok(None)));
        let accounts = crate::registry::accounts(&secrets, &config).expect("accounts build");
        let (updates, _published) = mpsc::channel(64);
        let mut engine = Engine::new(
            accounts,
            History::in_memory().expect("an in-memory database opens"),
            secrets,
            updates,
            config_path.clone(),
            Arc::new(Recorder::default()) as Arc<dyn Notifier>,
        );
        assert!(
            engine.accounts[0].client.is_some(),
            "registered with a client"
        );

        engine
            .set_option("antigravity", "default", "source", "cli")
            .await
            .expect("the usage source is settable");

        assert!(
            engine.accounts[0].client.is_none(),
            "the old client is dropped so the new setting is built into its replacement"
        );
        engine.ensure_client(0).await;
        assert!(
            engine.accounts[0].client.is_some(),
            "and a replacement is built without a stored key to read"
        );

        let _ = std::fs::remove_file(&config_path);
    }

    #[tokio::test]
    async fn concurrent_option_and_topology_commands_preserve_both_mutations() {
        let config_path = std::env::temp_dir().join(format!(
            "tidemark-engine-serialized-config-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&config_path);
        let mut config = Config::at(config_path.clone()).expect("empty config parses");
        config.add_provider("zai").expect("Z.ai configured");
        config
            .set_option("zai", "region", "global")
            .expect("initial option written");
        let secrets: Arc<dyn Secrets> = Arc::new(Keyring(|| Ok(None)));
        let accounts = crate::registry::accounts(&secrets, &config).expect("accounts build");
        let (updates, _published) = mpsc::channel(64);
        let mut engine = Engine::new(
            accounts,
            History::in_memory().expect("an in-memory database opens"),
            secrets,
            updates,
            config_path.clone(),
            Arc::new(Recorder::default()) as Arc<dyn Notifier>,
        );
        let (commands, mut command_queue) = mpsc::channel(4);
        let running = tokio::spawn(async move {
            engine.run(&mut command_queue).await;
            engine
        });
        let start = Arc::new(tokio::sync::Barrier::new(3));

        let setting = tokio::spawn({
            let commands = commands.clone();
            let start = Arc::clone(&start);
            async move {
                let (reply, answer) = oneshot::channel();
                start.wait().await;
                commands
                    .send(Command::SetOption {
                        provider: "zai".into(),
                        account: "default".into(),
                        name: "region".into(),
                        value: "bigmodel-cn".into(),
                        reply,
                    })
                    .await
                    .expect("engine is running");
                answer.await.expect("engine replied")
            }
        });
        let adding = tokio::spawn({
            let commands = commands.clone();
            let start = Arc::clone(&start);
            async move {
                let (reply, answer) = oneshot::channel();
                start.wait().await;
                commands
                    .send(Command::AddProvider {
                        provider: "kimi".into(),
                        reply,
                    })
                    .await
                    .expect("engine is running");
                answer.await.expect("engine replied")
            }
        });
        start.wait().await;
        let (setting, adding) = tokio::join!(setting, adding);
        setting
            .expect("setting task did not panic")
            .expect("setting persisted");
        adding
            .expect("add task did not panic")
            .expect("provider persisted");

        commands
            .send(Command::Shutdown)
            .await
            .expect("engine is running");
        let engine = running.await.expect("engine task did not panic");
        let written = Config::at(config_path.clone()).expect("written config parses");
        assert_eq!(written.providers().expect("readable"), ["zai", "kimi"]);
        assert_eq!(written.option("zai", "region"), Some("bigmodel-cn"));
        let zai = engine
            .accounts()
            .iter()
            .find(|account| account.provider().as_str() == "zai")
            .expect("Z.ai remains configured");
        assert_eq!(
            zai.status()
                .options
                .iter()
                .find(|option| option.name == "region")
                .map(|option| option.value.as_str()),
            Some("bigmodel-cn")
        );
        assert!(
            engine
                .accounts()
                .iter()
                .any(|account| account.provider().as_str() == "kimi")
        );
        let _ = std::fs::remove_file(config_path);
    }

    #[tokio::test]
    async fn reload_keeps_a_self_loading_provider_client_alive() {
        let mut harness = with_provider(Fake::new(vec![Ok(snapshot(7.0, 3_600))]));
        harness.engine.reload(None).await;
        harness.engine.poll_due(Instant::now()).await;

        let published = harness.published();
        assert_eq!(
            published.last().and_then(ProviderStatus::state),
            Some(ProviderState::Ok)
        );
    }

    #[tokio::test]
    async fn a_changed_setting_rebuilds_the_client_with_it() {
        // The whole reason a settings change drops the client rather than only polling
        // again: Z.ai's region decides which host the client was built to talk to, and a
        // poll with the old client would answer the new setting with the old answer.
        let seen: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
        let recorder = Arc::clone(&seen);
        let account = Account::new(
            ProviderId::new("zai"),
            AccountId::default(),
            Box::new(move |_credential, options| {
                recorder
                    .lock()
                    .expect("no test panics holding this")
                    .push(options.get("region").cloned().unwrap_or_default());
                Ok(Fake::new(vec![Ok(snapshot(1.0, 3600))]) as Arc<dyn Provider>)
            }),
        )
        .with_credential(CredentialKind::Key)
        .with_options(vec![ProviderOption {
            name: "region".into(),
            title: "Region".into(),
            description: None,
            value: "global".into(),
            choices: Vec::new(),
        }]);

        let path = std::env::temp_dir().join(format!(
            "tidemark-engine-reload-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut harness = harness_with_config(vec![account], path.clone());
        harness.engine.poll_due(Instant::now()).await;
        assert_eq!(
            seen.lock().expect("no panic").as_slice(),
            ["global"],
            "the first client is built from the published setting"
        );

        std::fs::write(&path, "[provider.zai]\nregion = \"bigmodel-cn\"\n").expect("seed");
        harness.engine.reload(Some("zai")).await;
        harness.engine.poll_due(Instant::now()).await;
        assert_eq!(
            seen.lock().expect("no panic").as_slice(),
            ["global", "bigmodel-cn"],
            "the second client is built from what the file now says"
        );
        assert_eq!(
            harness.engine.accounts()[0].status().options[0].value,
            "bigmodel-cn",
            "and the published setting agrees with the file"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn a_reload_clears_the_backoff_the_old_credential_earned() {
        let mut harness = with_provider(Fake::new(vec![
            Err(ProviderError::Credential { status: 401 }),
            Ok(snapshot(3.0, 3600)),
        ]));
        harness.engine.poll_due(Instant::now()).await;
        assert!(harness.wait_secs() > 60, "a rejection backs off");

        harness.engine.reload(None).await;
        assert_eq!(
            harness.wait_secs(),
            0,
            "a user who has just fixed the credential is owed an answer now"
        );
    }

    fn with_provider(provider: Arc<dyn Provider>) -> Harness {
        Harness::new(vec![Account::with_client(provider)], unlocked())
    }

    #[tokio::test]
    async fn current_segment_command_returns_only_the_open_segment() {
        let (updates, _published) = mpsc::channel(8);
        let mut engine = Engine::new(
            vec![Account::with_client(Fake::new(vec![
                Ok(reading(0, 80.0, 3_600)),
                Ok(reading(1, 2.0, 18_000)),
            ]))],
            History::in_memory().expect("history opens"),
            unlocked(),
            updates,
            std::env::temp_dir().join("tidemark-engine-current-segment.toml"),
            Arc::new(Recorder::default()) as Arc<dyn Notifier>,
        );
        engine.poll_due(Instant::now()).await;
        engine.mark_due(None);
        engine.poll_due(Instant::now()).await;

        let (commands, mut queue) = mpsc::channel(4);
        let running = tokio::spawn(async move {
            engine.run(&mut queue).await;
        });
        let (reply, answer) = oneshot::channel();
        commands
            .send(Command::CurrentSegment {
                provider: "fake".into(),
                account: "default".into(),
                window: "w18000".into(),
                reply,
            })
            .await
            .expect("engine is running");

        let points = answer
            .await
            .expect("engine replied")
            .expect("account exists");
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].used_percent, 2.0);

        commands
            .send(Command::Shutdown)
            .await
            .expect("engine is running");
        running.await.expect("engine task did not panic");
    }

    #[tokio::test]
    async fn a_reading_is_published_and_filed() {
        let mut harness = with_provider(Fake::new(vec![Ok(snapshot(42.0, 4 * 3600))]));
        harness.engine.poll_due(Instant::now()).await;

        let published = harness.published();
        let status = published.last().expect("the poll published something");
        assert_eq!(status.state(), Some(ProviderState::Ok));
        assert_eq!(status.windows.len(), 1);
        assert!((status.windows[0].used_percent - 42.0).abs() < f64::EPSILON);
        assert!(
            status.next_poll_at.is_some(),
            "a client can say when the next poll is"
        );
        assert_eq!(
            harness.engine.history.point_count().expect("countable"),
            1,
            "the reading reached the history database"
        );
    }

    #[tokio::test]
    async fn a_failed_poll_keeps_the_numbers_and_changes_the_state() {
        let mut harness = with_provider(Fake::new(vec![
            Ok(snapshot(42.0, 4 * 3600)),
            Err(ProviderError::Http { status: 502 }),
        ]));
        harness.engine.poll_due(Instant::now()).await;
        harness.poll_again().await;

        let status = harness.published().pop().expect("published twice");
        assert_eq!(status.state(), Some(ProviderState::Unreachable));
        assert_eq!(
            status.windows.len(),
            1,
            "the last good reading stays on screen behind the state chip"
        );
        assert!(status.message.is_some(), "and says what went wrong");
    }

    #[tokio::test]
    async fn repeated_failures_back_off() {
        let mut harness = with_provider(Fake::new(vec![
            Err(ProviderError::Http { status: 503 }),
            Err(ProviderError::Http { status: 503 }),
        ]));
        harness.engine.poll_due(Instant::now()).await;
        assert_eq!(harness.wait_secs(), scheduler::BASELINE.as_secs());
        harness.poll_again().await;
        assert_eq!(
            harness.wait_secs(),
            scheduler::BASELINE.as_secs() * 2,
            "the second failure waits longer than the first"
        );
    }

    #[tokio::test]
    async fn a_rate_limit_waits_as_long_as_the_provider_asked() {
        let mut harness = with_provider(Fake::new(vec![Err(ProviderError::RateLimited {
            retry_after: Some(2400),
        })]));
        harness.engine.poll_due(Instant::now()).await;

        let status = harness.published().pop().expect("published");
        assert_eq!(status.state(), Some(ProviderState::RateLimited));
        assert_eq!(harness.wait_secs(), 2400);
    }

    #[tokio::test]
    async fn a_locked_keyring_is_a_state_and_not_a_failure() {
        let mut harness = Harness::new(
            vec![Account::new(
                ProviderId::new("fake"),
                AccountId::default(),
                Box::new(|_, _| Ok(Fake::new(vec![Ok(snapshot(1.0, 3600))]) as Arc<dyn Provider>)),
            )],
            Arc::new(Keyring(|| Err(SecretError::Locked))),
        );
        harness.engine.poll_due(Instant::now()).await;

        let status = harness.published().pop().expect("published");
        assert_eq!(status.state(), Some(ProviderState::WaitingForKeyring));
        assert_eq!(
            harness.wait_secs(),
            scheduler::KEYRING_RETRY.as_secs(),
            "the daemon keeps asking, because the user is about to log in"
        );
        assert_eq!(
            harness.engine.history.point_count().expect("countable"),
            0,
            "nothing was fetched, so nothing was filed"
        );
    }

    #[tokio::test]
    async fn no_key_stored_is_told_apart_from_a_locked_keyring() {
        let mut harness = Harness::new(
            vec![Account::new(
                ProviderId::new("fake"),
                AccountId::default(),
                Box::new(|_, _| Ok(Fake::new(vec![]) as Arc<dyn Provider>)),
            )],
            Arc::new(Keyring(|| Ok(None))),
        );
        harness.engine.poll_due(Instant::now()).await;

        let status = harness.published().pop().expect("published");
        assert_eq!(status.state(), Some(ProviderState::NoCredential));
        assert!(status.message.is_none(), "the state says it all");
    }

    #[tokio::test]
    async fn a_rejected_key_is_read_again_on_the_next_attempt() {
        let mut harness = Harness::new(
            vec![Account::new(
                ProviderId::new("fake"),
                AccountId::default(),
                Box::new(|_, _| {
                    Ok(Fake::new(vec![
                        Err(ProviderError::Credential { status: 401 }),
                        Ok(snapshot(3.0, 3600)),
                    ]) as Arc<dyn Provider>)
                }),
            )],
            unlocked(),
        );
        harness.engine.poll_due(Instant::now()).await;
        assert_eq!(
            harness.engine.accounts()[0].status().state(),
            Some(ProviderState::CredentialRejected)
        );
        assert!(
            harness.engine.accounts()[0].client.is_none(),
            "the client is dropped so a newly stored key is picked up"
        );
    }

    #[tokio::test]
    async fn a_file_backed_client_survives_a_rejection_and_can_recover() {
        let mut harness = with_provider(Fake::new(vec![
            Err(ProviderError::Credential { status: 401 }),
            Ok(snapshot(3.0, 3600)),
        ]));

        harness.engine.poll_due(Instant::now()).await;
        assert_eq!(
            harness.engine.accounts()[0].status().state(),
            Some(ProviderState::CredentialRejected)
        );
        assert!(
            harness.engine.accounts()[0].client.is_some(),
            "a file-backed provider rereads its own credential and must stay registered"
        );

        harness.poll_again().await;
        assert_eq!(
            harness.engine.accounts()[0].status().state(),
            Some(ProviderState::Ok)
        );
    }

    #[tokio::test]
    async fn a_refresh_brings_the_account_forward() {
        let mut harness = with_provider(Fake::new(vec![
            Ok(snapshot(1.0, 4 * 3600)),
            Ok(snapshot(2.0, 4 * 3600)),
        ]));
        harness.engine.poll_due(Instant::now()).await;
        assert!(harness.engine.accounts()[0].due > Instant::now());

        harness.poll_again().await;
        let status = harness.published().pop().expect("published");
        assert!((status.windows[0].used_percent - 2.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn a_refresh_for_another_provider_leaves_this_one_alone() {
        let mut harness = with_provider(Fake::new(vec![Ok(snapshot(1.0, 4 * 3600))]));
        harness.engine.poll_due(Instant::now()).await;
        let due = harness.engine.accounts()[0].due;

        harness.engine.mark_due(Some("kimi"));
        assert_eq!(harness.engine.accounts()[0].due, due);
    }

    #[tokio::test]
    async fn an_account_polled_close_to_a_reset_comes_back_within_the_minute() {
        let mut harness = with_provider(Fake::new(vec![Ok(snapshot(90.0, 5 * 60))]));
        harness.engine.poll_due(Instant::now()).await;
        assert_eq!(harness.wait_secs(), scheduler::NEAR_RESET.as_secs());
    }

    #[tokio::test]
    async fn every_account_is_announced_before_anything_is_polled() {
        let mut harness = with_provider(Fake::new(vec![]));
        harness.engine.announce().await;
        let published = harness.published();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].state(), Some(ProviderState::Pending));
    }

    #[test]
    fn movement_is_measured_against_what_was_published_not_what_was_stored() {
        let published = vec![WindowStatus::from_window(&snapshot(10.0, 3600).windows[0])];
        assert!(!consumption_moved(&published, &snapshot(10.0, 3600)));
        assert!(consumption_moved(&published, &snapshot(10.5, 3600)));
    }

    #[test]
    fn a_window_appearing_or_disappearing_counts_as_movement() {
        let published = vec![WindowStatus::from_window(&snapshot(10.0, 3600).windows[0])];
        let mut gone = snapshot(10.0, 3600);
        gone.windows.clear();
        assert!(consumption_moved(&published, &gone));
        assert!(consumption_moved(&[], &snapshot(10.0, 3600)));
    }

    /// The opt-in is the whole point of the switch: fifteen windows announcing themselves
    /// at eighty percent would be fifteen interruptions nobody asked for.
    #[tokio::test]
    async fn a_window_nobody_opted_into_is_never_notified_about() {
        let mut harness = with_provider(Fake::new(vec![Ok(snapshot(96.0, 3600))]));
        harness.engine.poll_due(Instant::now()).await;
        assert!(harness.notices.summaries().is_empty());
    }

    fn notifying(provider: Arc<dyn Provider>) -> Harness {
        Harness::new(
            vec![Account::with_client(provider).with_notify(vec!["w18000".to_owned()])],
            unlocked(),
        )
    }

    #[tokio::test]
    async fn an_opted_in_window_warns_once_and_not_again() {
        let mut harness = notifying(Fake::new(vec![
            Ok(reading(0, 85.0, 3600)),
            Ok(reading(1, 86.0, 3600)),
        ]));
        harness.engine.poll_due(Instant::now()).await;
        harness.poll_again().await;

        let summaries = harness.notices.summaries();
        assert_eq!(summaries.len(), 1, "{summaries:?}");
        assert!(summaries[0].starts_with("85% used"), "{summaries:?}");
    }

    #[tokio::test]
    async fn crossing_the_second_threshold_warns_again() {
        let mut harness = notifying(Fake::new(vec![
            Ok(reading(0, 85.0, 3600)),
            Ok(reading(1, 96.0, 3600)),
        ]));
        harness.engine.poll_due(Instant::now()).await;
        harness.poll_again().await;

        let summaries = harness.notices.summaries();
        assert_eq!(summaries.len(), 2, "{summaries:?}");
        assert!(summaries[1].starts_with("96% used"), "{summaries:?}");
    }

    /// A desktop that is not listening yet — the daemon may well have started before the
    /// session did. Recording the row before delivery would lose the warning for good.
    #[tokio::test]
    async fn a_warning_the_desktop_refused_is_tried_again() {
        let notices = Recorder::refusing(1);
        let (tx, rx) = mpsc::channel(64);
        let mut harness = Harness {
            engine: Engine::new(
                vec![
                    Account::with_client(Fake::new(vec![
                        Ok(reading(0, 85.0, 3600)),
                        Ok(reading(1, 85.0, 3600)),
                        Ok(reading(2, 85.0, 3600)),
                    ]))
                    .with_notify(vec!["w18000".to_owned()]),
                ],
                History::in_memory().expect("an in-memory database opens"),
                unlocked(),
                tx,
                std::env::temp_dir().join("tidemark-engine-retry-absent.toml"),
                Arc::clone(&notices) as Arc<dyn Notifier>,
            ),
            updates: rx,
            config_path: std::env::temp_dir().join("tidemark-engine-retry-absent.toml"),
            notices: Arc::clone(&notices),
        };

        harness.engine.poll_due(Instant::now()).await;
        assert!(harness.notices.summaries().is_empty(), "the first attempt was refused");
        harness.poll_again().await;
        assert_eq!(harness.notices.summaries().len(), 1, "the retry landed");
        harness.poll_again().await;
        assert_eq!(
            harness.notices.summaries().len(),
            1,
            "and once it has landed it is not repeated"
        );
    }

    #[tokio::test]
    async fn a_rollover_of_an_opted_in_window_is_announced() {
        let mut harness = notifying(Fake::new(vec![
            Ok(reading(0, 96.0, 60)),
            Ok(reading(1, 1.0, 5 * 3600)),
        ]));
        harness.engine.poll_due(Instant::now()).await;
        harness.poll_again().await;

        let summaries = harness.notices.summaries();
        assert_eq!(summaries.len(), 2, "{summaries:?}");
        assert!(summaries[1].starts_with("Limit reset"), "{summaries:?}");
    }

    /// The acceptance criterion of the step: the dedup outlives the process holding it.
    #[tokio::test]
    async fn a_restart_does_not_repeat_a_warning_already_delivered() {
        let directory =
            std::env::temp_dir().join(format!("tidemark-engine-restart-{}", std::process::id()));
        let path = directory.join("history.db");
        let _ = std::fs::remove_dir_all(&directory);
        let config = std::env::temp_dir().join("tidemark-engine-restart-absent.toml");

        let notices = Arc::new(Recorder::default());
        for _ in 0..2 {
            let (tx, _rx) = mpsc::channel(64);
            let mut engine = Engine::new(
                vec![
                    Account::with_client(Fake::new(vec![Ok(snapshot(85.0, 3600))]))
                        .with_notify(vec!["w18000".to_owned()]),
                ],
                History::open(&path).expect("history opens"),
                unlocked(),
                tx,
                config.clone(),
                Arc::clone(&notices) as Arc<dyn Notifier>,
            );
            engine.poll_due(Instant::now()).await;
        }

        assert_eq!(
            notices.summaries().len(),
            1,
            "the second daemon read the row the first one wrote"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[tokio::test]
    async fn switching_a_window_on_persists_it_and_publishes_it() {
        let config_path = std::env::temp_dir().join(format!(
            "tidemark-engine-notify-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&config_path);
        let mut harness = harness_with_config(
            vec![Account::with_client(Fake::new(vec![Ok(reading(0, 42.0, 3600))]))],
            config_path.clone(),
        );
        harness.engine.poll_due(Instant::now()).await;

        harness
            .engine
            .set_window_notify("fake", "default", "w18000", true)
            .await
            .expect("switched on");

        assert_eq!(
            Config::at(config_path)
                .expect("reloaded")
                .notify_windows("fake")
                .expect("read"),
            vec!["w18000".to_owned()],
            "the switch outlives the daemon"
        );
        assert_eq!(
            harness.engine.accounts()[0].status().notify,
            vec!["w18000".to_owned()],
            "and the clients are told without waiting for a poll"
        );
    }

    /// A typo over `busctl` is an error rather than a line in the settings file that never
    /// does anything.
    #[tokio::test]
    async fn switching_on_a_window_the_account_does_not_report_is_refused() {
        let mut harness = with_provider(Fake::new(vec![Ok(reading(0, 42.0, 3600))]));
        harness.engine.poll_due(Instant::now()).await;
        assert!(
            harness
                .engine
                .set_window_notify("fake", "default", "w604800", true)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn switching_a_window_on_for_an_account_nobody_configured_is_refused() {
        let mut harness = Harness::empty("notify-unknown").await;
        assert!(
            harness
                .engine
                .set_window_notify("zai", "default", "w18000", true)
                .await
                .is_err()
        );
    }
}
