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
use tidemark_core::providers::http::{self, Proxy};
use tidemark_core::providers::keyed::session;
use tidemark_core::providers::{Credential, Provider, ProviderError};
use tidemark_core::secrets::{Kind, SecretError, Secrets};
use tidemark_core::storage::{History, IngestReport};
use tidemark_types::{
    AccountId, AuthCandidate, AuthCandidateState, AuthSelection, CredentialKind, HistoryPoint,
    Preferences, ProviderId, ProviderOption, ProviderState, ProviderStatus, Snapshot, Timestamp,
    WindowKey, WindowStatus,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use crate::notify::{self, Notifier};
use crate::scheduler::{self, Situation};

/// How often the history is thinned. Thinning only touches points older than ninety days,
/// so there is nothing to gain from doing it more often than once a day.
pub const THIN_INTERVAL: Duration = Duration::from_secs(24 * 3600);

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
    /// Put the published accounts in this provider order.
    ///
    /// A sequence rather than a status, because that is what changed: the readings are
    /// exactly as they were, and a client redrawing a grid needs the positions.
    Reordered(Vec<String>),
}

/// One application-wide preference mutation.
#[derive(Debug)]
pub enum Preference {
    ReleaseCheck(bool),
    MinimizeOnClose(bool),
    StartupMode(String),
    HistoryRetention(String),
    RefreshMode(String),
    RefreshMinutes(u32),
    /// The one preference that changes how this process reaches the network, rather than
    /// what it does with what it reaches.
    Proxy {
        mode: String,
        host: String,
        port: u16,
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
    /// Discovers dynamic local authentication sources for one configured account.
    InspectAuthSources {
        /// Stable provider slug.
        provider: String,
        /// Stable account name.
        account: String,
        /// Secret-free candidate report.
        reply: oneshot::Sender<Result<Vec<AuthCandidate>, String>>,
    },
    /// Revalidates and stores one explicit dynamic local authentication source.
    SelectAuthSource {
        /// Stable provider slug.
        provider: String,
        /// Stable account name.
        account: String,
        /// The mode and opaque candidate identity selected by the client.
        selection: AuthSelection,
        /// Completion sent after validation, persistence and publication finish.
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
    /// Reorder the configured providers, in the same queue as topology writes.
    SetOrder {
        /// Every configured provider slug, in the order the user put them in.
        providers: Vec<String>,
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
    /// Persists one application-wide preference.
    SetPreference {
        preference: Preference,
        reply: oneshot::Sender<Result<Preferences, String>>,
    },
    /// Deletes every stored historical reading and notification marker.
    ClearHistory {
        reply: oneshot::Sender<Result<(), String>>,
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
            due: Instant::now(),
        }
    }

    /// An account with no credential at all, whose client is built from its settings.
    ///
    /// For a provider that answers without one — a gateway running on this machine. There
    /// is no stored key to wait for, so the client is built on the first poll and again
    /// whenever the settings change, by the same [`Rebuild`] the credential-owning
    /// providers use. Built lazily rather than at registration so that a base URL the user
    /// has mistyped leaves this one account `Unreachable`, rather than refusing to load
    /// the rest of them.
    pub fn keyless(provider: ProviderId, account: AccountId, rebuild: Rebuild) -> Self {
        let mut status = ProviderStatus::pending(&provider, &account);
        describe(&mut status, CredentialKind::None);
        Self {
            status,
            provider,
            account,
            factory: None,
            rebuild: Some(rebuild),
            client: None,
            failures: 0,
            retry_after: None,
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
    pub(crate) fn rebuildable(&self) -> bool {
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

    /// Publishes the explicit local browser-cookie source selected in config.
    pub fn with_auth_selection(mut self, selection: Option<AuthSelection>) -> Self {
        self.status.auth_selection = selection;
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
    /// How healthy accounts are paced. Held rather than re-read per reschedule, because
    /// every change to it arrives through this loop's own command queue — there is no
    /// window in which the file and the field can disagree.
    refresh: scheduler::RefreshMode,
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
        refresh: scheduler::RefreshMode,
        notifier: Arc<dyn Notifier>,
    ) -> Self {
        Self {
            accounts,
            history,
            secrets,
            updates,
            config_path,
            refresh,
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

    /// Puts the configured providers in the order the user dragged them into.
    ///
    /// The accounts vector is reordered as well as the file, so a live reorder and a
    /// restart produce the same sequence: this vector is what [`Engine::announce`] walks,
    /// and a daemon that persisted one order and kept publishing another would disagree
    /// with itself for the rest of the session.
    ///
    /// Nothing about polling changes. An account keeps its `due` time and its backoff — a
    /// card moving on a grid is not news about a credential.
    pub async fn set_order(&mut self, providers: &[String]) -> Result<(), String> {
        let mut config = Config::at(self.config_path.clone()).map_err(|error| error.to_string())?;
        config
            .set_provider_order(providers)
            .map_err(|error| error.to_string())?;

        self.accounts.sort_by_key(|account| {
            providers
                .iter()
                .position(|slug| slug == account.provider.as_str())
                // An account for a provider the order does not name cannot happen while
                // the file is the only source of both, and sorting it to the end is what
                // the grid does with one anyway.
                .unwrap_or(providers.len())
        });
        let _ = self
            .updates
            .send(Publication::Reordered(providers.to_vec()))
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

    /// Discovers and validates the dynamic local sources one configured account offers.
    pub async fn inspect_auth_sources(
        &mut self,
        provider: &str,
        account: &str,
    ) -> Result<Vec<AuthCandidate>, String> {
        let Some(index) = self.accounts.iter().position(|configured| {
            configured.provider.as_str() == provider && configured.account.as_str() == account
        }) else {
            return Err(format!("account {provider}/{account} is not configured"));
        };
        self.ensure_client(index).await;
        let client = self.accounts[index]
            .client
            .as_ref()
            .ok_or_else(|| format!("{provider} has no client for authentication inspection"))?;
        let sources = client
            .inspect_auth_sources()
            .await
            .map_err(|error| error.to_string())?;
        Ok(browser_mode_report(provider, sources))
    }

    /// Revalidates, then atomically persists one selected dynamic local source.
    pub async fn select_auth_source(
        &mut self,
        provider: &str,
        account: &str,
        selection: AuthSelection,
    ) -> Result<(), String> {
        let Some(index) = self.accounts.iter().position(|configured| {
            configured.provider.as_str() == provider && configured.account.as_str() == account
        }) else {
            return Err(format!("account {provider}/{account} is not configured"));
        };
        let sources = self.inspect_auth_sources(provider, account).await?;
        let Some(selection) = resolvable_auth_selection(&sources, &selection) else {
            return Err("the selected authentication source is not ready".into());
        };

        let mut config = Config::at(self.config_path.clone()).map_err(|error| error.to_string())?;
        config
            .set_auth_selection(provider, &selection)
            .map_err(|error| error.to_string())?;
        let target = &mut self.accounts[index];
        target.status.options = crate::registry::options(provider, &config);
        target.status.auth_selection = crate::registry::browser_auth_selection(provider, &config);
        if target.rebuildable() {
            target.client = None;
        }
        target.failures = 0;
        target.retry_after = None;
        target.due = Instant::now();
        target.status.next_poll_at = Some(Timestamp::now().as_unix());
        let _ = self
            .updates
            .send(Publication::Changed(target.status.clone()))
            .await;
        Ok(())
    }

    /// Persists one application preference in the same serial queue as every other config
    /// mutation. Returning the complete set lets the service publish one coherent view.
    ///
    /// The immediate retention prune is maintenance after the commit, not part of it: a
    /// database that will not prune must not report an already-durable preference as
    /// failed — daily maintenance retries the pruning, and the caller's platform state
    /// (release watch, startup integration) must follow the durable config.
    pub async fn set_preference(&mut self, preference: Preference) -> Result<Preferences, String> {
        let retention_changed = matches!(&preference, Preference::HistoryRetention(_));
        let refresh_changed = matches!(
            &preference,
            Preference::RefreshMode(_) | Preference::RefreshMinutes(_)
        );
        // Captured before the match below, which takes the preference by value.
        let mode_switch = matches!(&preference, Preference::RefreshMode(_));
        // Built before anything is written. A mode with nowhere to reach is a mistake the
        // user can still see on screen and correct; persisted first, it would instead
        // become a stored setting that fails every poll from here on.
        let proxy = match &preference {
            Preference::Proxy { mode, host, port } => Some(Proxy::new(mode, host, *port)?),
            _ => None,
        };
        let mut config = Config::at(self.config_path.clone()).map_err(|error| error.to_string())?;
        match preference {
            Preference::ReleaseCheck(enabled) => config.set_release_check(enabled),
            Preference::MinimizeOnClose(enabled) => config.set_minimize_on_close(enabled),
            Preference::StartupMode(mode) => config.set_startup_mode(&mode),
            Preference::HistoryRetention(retention) => config.set_history_retention(&retention),
            Preference::RefreshMode(mode) => config.set_refresh_mode(&mode),
            Preference::RefreshMinutes(minutes) => config.set_refresh_minutes(minutes),
            Preference::Proxy { mode, host, port } => config.set_proxy(&mode, &host, port),
        }
        .map_err(|error| error.to_string())?;
        let preferences = config.preferences().map_err(|error| error.to_string())?;
        if retention_changed
            && let Err(error) = self.prune_for_retention(&preferences.history_retention)
        {
            tracing::error!(%error, "could not apply history retention");
        }
        if let Some(proxy) = proxy {
            self.adopt_proxy(proxy);
        }
        if refresh_changed {
            self.refresh = scheduler::RefreshMode::configured(&preferences);
            if mode_switch {
                // A mode switch is owed an immediate reading under the new rules — the
                // `adopt_proxy` precedent. Minutes deliberately do not: a spin control
                // must not be able to cause a poll storm, so the new pace applies from
                // each account's next natural poll.
                let now = Instant::now();
                for account in &mut self.accounts {
                    account.due = now;
                }
            }
        }
        Ok(preferences)
    }

    /// Points this process at a new proxy without restarting it.
    ///
    /// Dropping the clients is the whole mechanism: a `reqwest::Client` holds the proxy it
    /// was built with for the life of the client, and its pool holds sockets already
    /// established through the old one. Every account can build its client again — from a
    /// stored key or from its settings — so this costs one keyring read per account on the
    /// next poll and nothing else. **No status is touched**: each card keeps its last good
    /// reading and its state while the new client is built under it, which is the
    /// difference between this and restarting the service.
    ///
    /// `agy` comes along for the ride. Dropping Antigravity's client shuts down the
    /// subprocess, and the next poll spawns a new one — with the proxy in its environment,
    /// because [`Proxy::child_env`] is read at spawn time.
    fn adopt_proxy(&mut self, proxy: Option<Proxy>) {
        if http::proxy() == proxy {
            return;
        }
        http::set_proxy(proxy);
        let now = Instant::now();
        for account in &mut self.accounts {
            if !account.rebuildable() {
                continue;
            }
            account.client = None;
            // The old proxy may be why this account was failing, so its backoff is not
            // evidence about the new one.
            account.failures = 0;
            account.retry_after = None;
            account.due = now;
        }
    }

    fn prune_for_retention(&mut self, retention: &str) -> Result<(), String> {
        let days = match retention {
            Preferences::RETENTION_FOREVER => return Ok(()),
            Preferences::RETENTION_SIX_MONTHS => 183,
            Preferences::RETENTION_ONE_YEAR => 365,
            _ => return Err(format!("unknown history retention {retention:?}")),
        };
        let cutoff = Timestamp::from_unix(Timestamp::now().as_unix() - days * 24 * 3600)
            .map_err(|error| error.to_string())?;
        let removed = self
            .history
            .prune_before(cutoff)
            .map_err(|error| error.to_string())?;
        if removed > 0 {
            tracing::info!(removed, retention, "pruned history beyond retention");
        }
        Ok(())
    }

    fn clear_history(&mut self) -> Result<(), String> {
        self.history.clear().map_err(|error| error.to_string())
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
                    Some(Command::InspectAuthSources { provider, account, reply }) => {
                        let result = self.inspect_auth_sources(&provider, &account).await;
                        let _ = reply.send(result);
                    }
                    Some(Command::SelectAuthSource { provider, account, selection, reply }) => {
                        let result = self.select_auth_source(&provider, &account, selection).await;
                        let _ = reply.send(result);
                    }
                    Some(Command::SetWindowNotify { provider, account, window, enabled, reply }) => {
                        let result = self
                            .set_window_notify(&provider, &account, &window, enabled)
                            .await;
                        let _ = reply.send(result);
                    }
                    Some(Command::SetOrder { providers, reply }) => {
                        let result = self.set_order(&providers).await;
                        let _ = reply.send(result);
                    }
                    Some(Command::CurrentSegment { provider, account, window, reply }) => {
                        let result = self.current_segment(&provider, &account, &window);
                        let _ = reply.send(result);
                    }
                    Some(Command::SetPreference { preference, reply }) => {
                        let result = self.set_preference(preference).await;
                        let _ = reply.send(result);
                    }
                    Some(Command::ClearHistory { reply }) => {
                        let result = self.clear_history();
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

            for decided in notify::decide(window.used_percent, outcome.boundary.is_some(), &already)
            {
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
            return Err(format!(
                "{provider} is not reporting a window called {window}"
            ));
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
            mode: self.refresh,
            worst_used_percent: worst_used(&account.status.windows),
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
    ///
    /// The other login is looked for again for the same reason, and it is the more urgent
    /// half: a refresh is also what the user reaches for straight after running `claude`
    /// in a terminal, and the settings pane that sent them there is showing "Not found"
    /// until somebody looks again. The keyring is *not* re-read here — that answer only
    /// moves when Tidemark itself moves it, and every such path already probes.
    fn mark_due(&mut self, target: Option<&str>) {
        let now = Instant::now();
        for account in &mut self.accounts {
            if !target.is_none_or(|slug| account.provider.as_str() == slug) {
                continue;
            }
            account.due = now;
            if account.factory.is_some() {
                account.client = None;
            }
            let provider = account.provider.as_str().to_owned();
            account.status.external_present = crate::registry::external_present(&provider);
            account.status.auth_source = crate::registry::auth_source(&provider, &account.status);
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

    /// Records, for each account, the two things a credentials dialog asks before anything
    /// has been polled: whether Tidemark itself holds a credential for the account, and
    /// whether the provider's other login — the vendor CLI's file, an `agy` session —
    /// exists on this machine at all. From the two it then settles which credential the
    /// next poll will use.
    ///
    /// Asked rather than inferred, and asked rarely — at startup and whenever a credential
    /// changes — because the answers only move when the user moves them. A locked keyring
    /// leaves the first answer unknown rather than answering "no": the dialog would
    /// otherwise offer to replace a key that is there and simply out of reach. The second
    /// proves existence, not usability — a file that exists may hold an expired token —
    /// and it is asked even of accounts Tidemark holds nothing for, because for them it
    /// is the whole answer.
    pub async fn probe_credentials(&mut self, target: Option<&str>) {
        for index in 0..self.accounts.len() {
            let account = &self.accounts[index];
            if !target.is_none_or(|slug| account.provider.as_str() == slug) {
                continue;
            }
            let provider = account.provider.clone();
            let kind = account.status.credential_kind().and_then(stored_kind);
            let held = match kind {
                Some(kind) => {
                    let name = account.account.clone();
                    match self.secrets.get(kind, &provider, &name).await {
                        Ok(found) => Some(found.is_some()),
                        Err(SecretError::Locked) => None,
                        Err(error) => {
                            tracing::debug!(provider = %provider, %error, "cannot see stored credentials");
                            None
                        }
                    }
                }
                // Nothing of ours to look for: the credential belongs to something else
                // on the machine.
                None => None,
            };
            self.accounts[index].status.has_credential = held;
            self.accounts[index].status.external_present =
                crate::registry::external_present(provider.as_str());
            self.accounts[index].status.auth_source =
                crate::registry::auth_source(provider.as_str(), &self.accounts[index].status);
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
        match Config::at(self.config_path.clone()).and_then(|config| config.preferences()) {
            Ok(preferences) => {
                if let Err(error) = self.prune_for_retention(&preferences.history_retention) {
                    tracing::error!(%error, "could not apply history retention");
                }
            }
            Err(error) => {
                tracing::error!(%error, "could not read history retention during maintenance")
            }
        }
    }

    /// The accounts, so a test can look at what a poll left behind.
    #[cfg(test)]
    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }
}

/// Gives browser-profile candidates the mode body the settings protocol renders.
fn browser_mode_report(provider: &str, sources: Vec<AuthCandidate>) -> Vec<AuthCandidate> {
    if crate::registry::has_browser_session_auth(provider)
        && !sources
            .iter()
            .any(|candidate| candidate.id == session::BROWSER_SOURCE)
    {
        return session::browser_sources(sources);
    }
    sources
}

/// Which schema a credential of this kind is stored under, or `None` where Tidemark stores
/// nothing of its own.
pub fn stored_kind(credential: CredentialKind) -> Option<Kind> {
    match credential {
        CredentialKind::Key => Some(Kind::Key),
        CredentialKind::OAuth => Some(Kind::Token),
        CredentialKind::External | CredentialKind::None => None,
    }
}

/// The exact usable source an inspected choice names.
///
/// A browser parent is a one-click shorthand only. The persisted choice is its first usable
/// profile in inspection order, so a later poll cannot re-scan and choose a different
/// account. Usable is a proven source or a challenged one: an edge challenge refuses the
/// proof rather than the session, so a challenged choice starts working the moment the edge
/// lets the client through.
fn resolvable_auth_selection(
    sources: &[AuthCandidate],
    selection: &AuthSelection,
) -> Option<AuthSelection> {
    let mode = sources
        .iter()
        .find(|candidate| candidate.id == selection.mode)?;
    match selection.candidate.as_deref() {
        None if selectable_state(mode.state()) => Some(selection.clone()),
        None => None,
        Some(candidate) => {
            selectable_descendant(&mode.children, candidate).map(|candidate| AuthSelection {
                mode: selection.mode.clone(),
                candidate: Some(candidate.id.clone()),
            })
        }
    }
}

/// Finds a selected usable candidate and resolves parent shortcuts to their first usable leaf.
fn selectable_descendant<'a>(
    candidates: &'a [AuthCandidate],
    selected: &str,
) -> Option<&'a AuthCandidate> {
    candidates.iter().find_map(|candidate| {
        if candidate.id == selected && selectable_state(candidate.state()) {
            first_selectable_leaf(candidate)
        } else {
            selectable_descendant(&candidate.children, selected)
        }
    })
}

/// The first usable leaf in the daemon's stable discovery order.
fn first_selectable_leaf(candidate: &AuthCandidate) -> Option<&AuthCandidate> {
    if !selectable_state(candidate.state()) {
        return None;
    }
    candidate
        .children
        .iter()
        .find_map(first_selectable_leaf)
        .or(Some(candidate))
}

/// Whether an inspected candidate may be persisted: proven, or blocked only by an edge
/// challenge the provider never saw.
fn selectable_state(state: Option<AuthCandidateState>) -> bool {
    matches!(
        state,
        Some(AuthCandidateState::Ready | AuthCandidateState::Challenged)
    )
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
        | ProviderError::Local(_)
        | ProviderError::Emulated(_)
        | ProviderError::Challenged(_) => ProviderState::Unreachable,
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

/// The most-used window the account last reported, if it reported any.
///
/// Auto paces by the worst window: a red five-hour window is news every minute even
/// beside a blue weekly one.
fn worst_used(windows: &[WindowStatus]) -> Option<f64> {
    windows
        .iter()
        .map(|window| window.used_percent)
        .reduce(f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::{Notice, Notifier, NotifyError};
    use std::sync::Mutex;
    use tidemark_core::providers::BoxFuture;
    use tidemark_types::{AuthCandidate, AuthSelection, Window, WindowKey, WindowLength};

    #[test]
    fn a_missing_cli_credential_has_its_own_published_state() {
        assert_eq!(
            state_for(&ProviderError::NoCredential),
            ProviderState::NoCredential
        );
    }

    #[test]
    fn a_browser_only_provider_wraps_its_profile_report_in_the_browser_mode() {
        // BrowserAuth renders by mode id. Handing it a Firefox root directly makes the
        // selected Browser half look unavailable even when the profile was discovered.
        let report = browser_mode_report(
            "t3chat",
            vec![AuthCandidate {
                id: "firefox".into(),
                title: "Firefox".into(),
                subtitle: None,
                state: "ready".into(),
                children: vec![AuthCandidate {
                    id: "firefox/Default".into(),
                    title: "Default".into(),
                    subtitle: None,
                    state: "ready".into(),
                    children: Vec::new(),
                }],
            }],
        );

        assert_eq!(report[0].id, "browser");
        assert_eq!(report[0].children[0].id, "firefox");
        assert_eq!(
            resolvable_auth_selection(
                &report,
                &AuthSelection {
                    mode: "browser".into(),
                    candidate: Some("firefox".into()),
                },
            ),
            Some(AuthSelection {
                mode: "browser".into(),
                candidate: Some("firefox/Default".into()),
            })
        );
    }

    #[test]
    fn a_challenged_browser_choice_is_persisted_as_its_challenged_profile() {
        // An edge challenge refuses the proof, not the session: the recorded profile
        // starts working the moment the edge lets polls through.
        let report = browser_mode_report(
            "t3chat",
            vec![AuthCandidate {
                id: "firefox".into(),
                title: "Firefox".into(),
                subtitle: None,
                state: "challenged".into(),
                children: vec![AuthCandidate {
                    id: "firefox/Default".into(),
                    title: "Default".into(),
                    subtitle: None,
                    state: "challenged".into(),
                    children: Vec::new(),
                }],
            }],
        );

        assert_eq!(
            resolvable_auth_selection(
                &report,
                &AuthSelection {
                    mode: "browser".into(),
                    candidate: Some("firefox".into()),
                },
            ),
            Some(AuthSelection {
                mode: "browser".into(),
                candidate: Some("firefox/Default".into()),
            })
        );
    }

    #[test]
    fn an_unanswered_browser_choice_is_still_refused() {
        let report = browser_mode_report(
            "t3chat",
            vec![AuthCandidate {
                id: "firefox".into(),
                title: "Firefox".into(),
                subtitle: None,
                state: "unreachable".into(),
                children: Vec::new(),
            }],
        );

        assert_eq!(
            resolvable_auth_selection(
                &report,
                &AuthSelection {
                    mode: "browser".into(),
                    candidate: Some("firefox".into()),
                },
            ),
            None
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

    /// A rebuildable provider whose authentication candidates are fixed test metadata.
    #[derive(Debug)]
    struct AuthFake {
        sources: Vec<AuthCandidate>,
    }

    impl Provider for AuthFake {
        fn id(&self) -> ProviderId {
            ProviderId::new("cursor")
        }

        fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>> {
            Box::pin(async { Err(ProviderError::NoCredential) })
        }

        fn inspect_auth_sources(&self) -> BoxFuture<'_, Result<Vec<AuthCandidate>, ProviderError>> {
            Box::pin(async { Ok(self.sources.clone()) })
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
                subtitle: None,
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
                subtitle: None,
                used_percent: used,
                resets_at: Some(now.saturating_add_seconds(resets_in)),
                length: WindowLength::from_secs(18_000),
            }],
            details: Vec::new(),
        }
    }

    /// Two windows, so a test can put them in different zones.
    fn two_windows(five_hour: f64, weekly: f64, resets_in: i64) -> Snapshot {
        let now = Timestamp::now();
        Snapshot {
            provider: ProviderId::new("fake"),
            account: AccountId::default(),
            captured_at: now,
            windows: vec![
                Window {
                    key: WindowKey::for_length(WindowLength::from_secs(18_000).expect("nonzero")),
                    title: "5 hours".into(),
                    subtitle: None,
                    used_percent: five_hour,
                    resets_at: Some(now.saturating_add_seconds(resets_in)),
                    length: WindowLength::from_secs(18_000),
                },
                Window {
                    key: WindowKey::for_length(WindowLength::from_secs(604_800).expect("nonzero")),
                    title: "7 days".into(),
                    subtitle: None,
                    used_percent: weekly,
                    resets_at: Some(now.saturating_add_seconds(7 * 24 * 3600)),
                    length: WindowLength::from_secs(604_800),
                },
            ],
            details: Vec::new(),
        }
    }

    /// A harness whose engine runs one chosen refresh mode rather than the file's.
    fn with_provider_paced(
        provider: Arc<dyn Provider>,
        refresh: scheduler::RefreshMode,
    ) -> Harness {
        let (tx, rx) = mpsc::channel(64);
        let config_path = std::env::temp_dir().join("tidemark-engine-tests-absent.toml");
        let notices = Arc::new(Recorder::default());
        Harness {
            engine: Engine::new(
                vec![Account::with_client(provider)],
                History::in_memory().expect("an in-memory database opens"),
                unlocked(),
                tx,
                config_path.clone(),
                refresh,
                Arc::clone(&notices) as Arc<dyn Notifier>,
            ),
            updates: rx,
            config_path,
            notices,
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
                    scheduler::RefreshMode::Auto,
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
                    scheduler::RefreshMode::Auto,
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
        // From the file, so a test can seed `[refresh]` and have the loop run it.
        let refresh = Config::at(config.clone())
            .and_then(|config| config.preferences())
            .map(|preferences| scheduler::RefreshMode::configured(&preferences))
            .unwrap_or(scheduler::RefreshMode::Auto);
        Harness {
            engine: Engine::new(
                accounts,
                History::in_memory().expect("an in-memory database opens"),
                unlocked(),
                tx,
                config.clone(),
                refresh,
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
    async fn reordering_persists_the_array_and_moves_the_accounts_with_it() {
        let mut harness = Harness::configured("runtime-order", &["kimi", "zai", "claude"]).await;
        let order = vec!["claude".to_owned(), "kimi".to_owned(), "zai".to_owned()];

        harness.engine.set_order(&order).await.expect("reordered");

        let slugs: Vec<&str> = harness
            .engine
            .accounts()
            .iter()
            .map(|account| account.provider().as_str())
            .collect();
        assert_eq!(
            slugs,
            ["claude", "kimi", "zai"],
            "announce walks this vector, so it has to agree with the file"
        );
        let config = Config::at(harness.config_path.clone()).expect("parses");
        assert_eq!(config.providers().expect("readable"), order);
        let publication = harness.updates.recv().await.expect("announced");
        assert!(matches!(publication, Publication::Reordered(published) if published == order));
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
            scheduler::RefreshMode::Auto,
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
    async fn a_browser_parent_persists_its_first_validated_profile_then_rebuilds() {
        // If the validation moved after the write, an unknown candidate could replace the
        // working Cursor App source with a browser the daemon has never proved usable.
        let path = std::env::temp_dir().join(format!(
            "tidemark-engine-auth-source-{}.toml",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let mut config = Config::at(path.clone()).expect("empty config parses");
        config.add_provider("cursor").expect("Cursor configured");
        config
            .set_auth_selection(
                "cursor",
                &AuthSelection {
                    mode: "cursor-app".into(),
                    candidate: None,
                },
            )
            .expect("Cursor App stored");
        let sources = vec![
            AuthCandidate {
                id: "cursor-app".into(),
                title: "Cursor App".into(),
                subtitle: None,
                state: "ready".into(),
                children: Vec::new(),
            },
            AuthCandidate {
                id: "browser".into(),
                title: "Browser".into(),
                subtitle: None,
                state: "ready".into(),
                children: vec![AuthCandidate {
                    id: "firefox".into(),
                    title: "Firefox".into(),
                    subtitle: None,
                    state: "ready".into(),
                    children: vec![
                        AuthCandidate {
                            id: "firefox/work".into(),
                            title: "Work".into(),
                            subtitle: None,
                            state: "ready".into(),
                            children: Vec::new(),
                        },
                        AuthCandidate {
                            id: "firefox/personal".into(),
                            title: "Personal".into(),
                            subtitle: None,
                            state: "ready".into(),
                            children: Vec::new(),
                        },
                    ],
                }],
            },
        ];
        let source_copy = sources.clone();
        let account = Account::keyless(
            ProviderId::new("cursor"),
            AccountId::default(),
            Box::new(move |_| {
                Ok(Arc::new(AuthFake {
                    sources: source_copy.clone(),
                }) as Arc<dyn Provider>)
            }),
        )
        .with_options(crate::registry::options("cursor", &config))
        .with_auth_selection(crate::registry::browser_auth_selection("cursor", &config));
        let mut harness = harness_with_config(vec![account], path.clone());
        harness.engine.ensure_client(0).await;
        assert!(
            harness.engine.accounts[0].client.is_some(),
            "old client is live"
        );

        assert!(
            harness
                .engine
                .select_auth_source(
                    "cursor",
                    "default",
                    AuthSelection {
                        mode: "browser".into(),
                        candidate: Some("firefox/unknown".into()),
                    },
                )
                .await
                .is_err()
        );
        let untouched = Config::at(path.clone()).expect("config still parses");
        assert_eq!(
            untouched.option("cursor", "auth-source"),
            Some("cursor-app")
        );
        assert!(
            harness.engine.accounts[0].client.is_some(),
            "rejection keeps the client"
        );

        harness
            .engine
            .select_auth_source(
                "cursor",
                "default",
                AuthSelection {
                    mode: "browser".into(),
                    candidate: Some("firefox".into()),
                },
            )
            .await
            .expect("ready source is committed");
        let written = Config::at(path.clone()).expect("config parses");
        assert_eq!(written.option("cursor", "auth-browser"), Some("firefox"));
        assert_eq!(
            written.option("cursor", "auth-profile"),
            Some("work"),
            "a one-click browser choice is pinned to the exact ready profile it proved"
        );
        assert_eq!(
            harness.engine.accounts[0].status.auth_selection,
            Some(AuthSelection {
                mode: "browser".into(),
                candidate: Some("firefox/work".into()),
            })
        );
        assert!(
            harness.engine.accounts[0].client.is_none(),
            "client is rebuilt on poll"
        );
        assert_eq!(
            harness.wait_secs(),
            0,
            "the selected source is due immediately"
        );
        assert!(matches!(
            harness.updates.recv().await,
            Some(Publication::Changed(status)) if status.auth_selection.is_some()
        ));
        let _ = std::fs::remove_file(path);
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
            scheduler::RefreshMode::Auto,
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
            scheduler::RefreshMode::Auto,
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
    async fn the_worst_window_sets_the_auto_pace() {
        let mut harness = with_provider(Fake::new(vec![Ok(two_windows(55.0, 95.0, 4 * 3600))]));
        harness.engine.poll_due(Instant::now()).await;
        assert_eq!(
            harness.wait_secs(),
            scheduler::AUTO_RED.as_secs(),
            "a red weekly window is news every minute beside a blue five-hour one"
        );
    }

    #[tokio::test]
    async fn manual_polls_at_the_chosen_interval_whatever_the_zone() {
        let mut harness = with_provider_paced(
            Fake::new(vec![Ok(snapshot(95.0, 4 * 3600))]),
            scheduler::RefreshMode::Manual(Duration::from_secs(15 * 60)),
        );
        harness.engine.poll_due(Instant::now()).await;
        assert_eq!(
            harness.wait_secs(),
            15 * 60,
            "the user picked a pace; neither the zone nor the reset changes it"
        );
    }

    #[tokio::test]
    async fn switching_the_refresh_mode_polls_every_account_now() {
        let mut harness = with_provider(Fake::new(vec![Ok(snapshot(50.0, 4 * 3600))]));
        harness.engine.poll_due(Instant::now()).await;
        assert_eq!(harness.wait_secs(), scheduler::AUTO_BLUE.as_secs());

        harness
            .engine
            .set_preference(Preference::RefreshMode("manual".into()))
            .await
            .expect("mode stored");
        assert_eq!(
            harness.wait_secs(),
            0,
            "the new pace is owed an immediate reading, not one old interval later"
        );
        let config = Config::at(harness.config_path.clone()).expect("parses");
        assert_eq!(
            config.preferences().expect("readable").refresh_mode,
            "manual"
        );
    }

    #[tokio::test]
    async fn changing_the_manual_minutes_applies_from_the_next_poll() {
        let path = std::env::temp_dir().join(format!(
            "tidemark-engine-refresh-minutes-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "[refresh]\nmode = \"manual\"\nminutes = 10\n").expect("seed");
        let mut harness = harness_with_config(
            vec![Account::with_client(Fake::new(vec![
                Ok(snapshot(50.0, 4 * 3600)),
                Ok(snapshot(50.0, 4 * 3600)),
            ]))],
            path.clone(),
        );
        harness.engine.poll_due(Instant::now()).await;
        assert_eq!(harness.wait_secs(), 10 * 60);

        harness
            .engine
            .set_preference(Preference::RefreshMinutes(2))
            .await
            .expect("minutes stored");
        assert!(
            harness.wait_secs() > 0,
            "a spin control must not be able to cause a poll storm"
        );

        harness.poll_again().await;
        assert_eq!(harness.wait_secs(), 2 * 60);
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn every_account_is_announced_before_anything_is_polled() {
        let mut harness = with_provider(Fake::new(vec![]));
        harness.engine.announce().await;
        let published = harness.published();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].state(), Some(ProviderState::Pending));
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
                scheduler::RefreshMode::Auto,
                Arc::clone(&notices) as Arc<dyn Notifier>,
            ),
            updates: rx,
            config_path: std::env::temp_dir().join("tidemark-engine-retry-absent.toml"),
            notices: Arc::clone(&notices),
        };

        harness.engine.poll_due(Instant::now()).await;
        assert!(
            harness.notices.summaries().is_empty(),
            "the first attempt was refused"
        );
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
                scheduler::RefreshMode::Auto,
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
            vec![Account::with_client(Fake::new(vec![Ok(reading(
                0, 42.0, 3600,
            ))]))],
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
    #[tokio::test]
    async fn application_preferences_are_serialized_through_the_engine() {
        let mut harness = Harness::empty("application-preferences").await;

        harness
            .engine
            .set_preference(Preference::ReleaseCheck(false))
            .await
            .expect("release check changed");
        harness
            .engine
            .set_preference(Preference::MinimizeOnClose(false))
            .await
            .expect("close behavior changed");
        harness
            .engine
            .set_preference(Preference::StartupMode(Preferences::STARTUP_DAEMON.into()))
            .await
            .expect("startup mode changed");
        let preferences = harness
            .engine
            .set_preference(Preference::HistoryRetention(
                Preferences::RETENTION_SIX_MONTHS.into(),
            ))
            .await
            .expect("history retention changed");

        assert_eq!(
            preferences,
            Preferences {
                release_check: false,
                minimize_on_close: false,
                startup_mode: Preferences::STARTUP_DAEMON.into(),
                history_retention: Preferences::RETENTION_SIX_MONTHS.into(),
                proxy_mode: Preferences::PROXY_OFF.into(),
                proxy_host: String::new(),
                proxy_port: 0,
                refresh_mode: Preferences::REFRESH_AUTO.into(),
                refresh_minutes: 5,
            }
        );
        assert_eq!(
            Config::at(harness.config_path)
                .expect("reloaded")
                .preferences()
                .expect("readable"),
            preferences
        );
    }

    /// The proxy is the one preference that changes this process rather than a file, so
    /// this checks all three things that has to mean: the value is in force, the clients
    /// built against the old one are gone, and no card lost its reading over it.
    #[tokio::test]
    async fn a_proxy_change_is_adopted_without_restarting_anything() {
        let mut harness = harness_with_config(
            vec![
                Account::with_client(Fake::new(vec![Ok(reading(0, 42.0, 3600))])).with_rebuild(
                    Box::new(|_| {
                        Ok(Fake::new(vec![Ok(reading(0, 42.0, 3600))]) as Arc<dyn Provider>)
                    }),
                ),
            ],
            std::env::temp_dir().join(format!("tidemark-engine-proxy-{}.toml", std::process::id())),
        );
        harness.engine.poll_due(Instant::now()).await;
        let published = harness.engine.accounts()[0].status.clone();
        assert!(harness.engine.accounts()[0].client.is_some());

        let preferences = harness
            .engine
            .set_preference(Preference::Proxy {
                mode: Preferences::PROXY_SOCKS5.into(),
                host: "127.0.0.1".into(),
                port: 1080,
            })
            .await
            .expect("proxy set");

        assert_eq!(preferences.proxy_mode, Preferences::PROXY_SOCKS5);
        assert_eq!(
            http::proxy().expect("in force").url(),
            "socks5h://127.0.0.1:1080"
        );
        assert!(
            harness.engine.accounts()[0].client.is_none(),
            "a client built against the old proxy must not survive the change"
        );
        assert_eq!(
            harness.engine.accounts()[0].status,
            published,
            "the reading on the card is not news about a proxy"
        );

        // Half a proxy is refused before it is written, so the one in force still stands.
        let refused = harness
            .engine
            .set_preference(Preference::Proxy {
                mode: Preferences::PROXY_HTTP.into(),
                host: String::new(),
                port: 8080,
            })
            .await;
        assert!(refused.is_err());
        assert_eq!(
            Config::at(harness.config_path.clone())
                .expect("reloaded")
                .preferences()
                .expect("readable")
                .proxy_mode,
            Preferences::PROXY_SOCKS5
        );

        // Back to none, so nothing else in this test binary inherits it.
        harness
            .engine
            .set_preference(Preference::Proxy {
                mode: Preferences::PROXY_OFF.into(),
                host: String::new(),
                port: 0,
            })
            .await
            .expect("proxy cleared");
        assert_eq!(http::proxy(), None);
    }
    #[tokio::test]
    async fn daily_maintenance_applies_the_configured_history_retention() {
        let mut harness = Harness::empty("retention-maintenance").await;
        let mut config = Config::at(harness.config_path.clone()).expect("config opens");
        config
            .set_history_retention(Preferences::RETENTION_SIX_MONTHS)
            .expect("retention configured");
        let mut old = snapshot(42.0, 3600);
        old.captured_at =
            Timestamp::from_unix(Timestamp::now().as_unix() - 200 * 24 * 3600).expect("valid");
        harness.engine.history.ingest(&old).expect("history seeded");

        harness.engine.thin_if_due();

        assert_eq!(harness.engine.history.point_count().expect("counted"), 0);
    }
    #[tokio::test]
    async fn a_committed_preference_is_not_failed_by_a_retention_prune() {
        let dir =
            std::env::temp_dir().join(format!("tidemark-engine-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let history_path = dir.join("history.db");
        let config_path = dir.join("config.toml");
        Config::at(config_path.clone()).expect("config opens");
        let (updates, _queue) = mpsc::channel(64);
        let notices = Arc::new(Recorder::default());
        let mut engine = Engine::new(
            Vec::new(),
            History::open(&history_path).expect("file-backed history opens"),
            unlocked(),
            updates,
            config_path.clone(),
            scheduler::RefreshMode::Auto,
            Arc::clone(&notices) as Arc<dyn Notifier>,
        );

        // A second connection holds the database's one write lock, so the immediate
        // prune that follows a preference commit cannot write.
        let competing = rusqlite::Connection::open(&history_path).expect("second connection");
        competing
            .execute_batch("BEGIN IMMEDIATE")
            .expect("write lock held");

        let preferences = engine
            .set_preference(Preference::HistoryRetention(
                Preferences::RETENTION_SIX_MONTHS.into(),
            ))
            .await
            .expect("the commit itself succeeded");

        competing
            .execute_batch("ROLLBACK")
            .expect("write lock freed");
        assert!(preferences.minimize_on_close);
        assert_eq!(
            preferences.history_retention,
            Preferences::RETENTION_SIX_MONTHS
        );
        assert_eq!(
            Config::at(config_path.clone())
                .expect("config rereads")
                .preferences()
                .expect("readable"),
            preferences
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
