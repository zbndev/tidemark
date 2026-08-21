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
//!
//! # Credentials
//!
//! The daemon holds the credentials, so the daemon is what changes them: `SetKey`,
//! `SignOut`, `SetOption`, and the two halves of a login. Nothing about that is specific to
//! the GUI — a `busctl` line does the same thing, and so would a `tidemark login` — which
//! is the point of putting it here rather than reachable only through a dialog.
//!
//! A login is two calls rather than one because the work splits across two processes.
//! `BeginLogin` takes the callback port, builds the authorize URL and hands it back
//! *without* waiting; the caller opens it however its platform opens URLs; `AwaitLogin`
//! then blocks until the browser has come back and the tokens are stored. The daemon does
//! not open the browser itself: it is a background service that may have been started
//! before the session had a display, and asking it to guess how to reach one is how a
//! login silently fails on a machine with an unusual desktop.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tidemark_core::oauth::Login;
use tidemark_core::providers::Credential;
use tidemark_core::secrets::{Kind, SecretError, Secrets};
use tidemark_types::{
    AccountId, CredentialKind, HistoryPoint, ProviderDefinition, ProviderId, ProviderStatus,
};
use tokio::sync::{Mutex, RwLock, mpsc, oneshot};
use tokio::task::{AbortHandle, JoinHandle};
use zbus::object_server::SignalEmitter;
use zbus::{fdo, interface};

use crate::engine::{Command, stored_kind};
use crate::registry;

type AccountKey = (String, String);
type AccountMutation = Arc<Mutex<()>>;

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

    /// Removes one exact account while preserving the order of every survivor.
    pub async fn remove(&self, provider: &str, account: &str) -> Option<ProviderStatus> {
        let mut statuses = self.0.write().await;
        let index = statuses
            .iter()
            .position(|held| held.provider == provider && held.account == account)?;
        Some(statuses.remove(index))
    }

    /// Everything currently known.
    pub async fn all(&self) -> Vec<ProviderStatus> {
        self.0.read().await.clone()
    }

    /// One account, by the pair that identifies it.
    pub async fn find(&self, provider: &str, account: &str) -> Option<ProviderStatus> {
        self.0
            .read()
            .await
            .iter()
            .find(|held| held.provider == provider && held.account == account)
            .cloned()
    }
}

/// A login that has been started and not yet finished.
///
/// The task owns the listener and the exchange; this is the handle `AwaitLogin` takes to
/// wait on it and `CancelLogin` uses to let the port go.
#[derive(Debug)]
struct Pending {
    task: Option<JoinHandle<Result<(), String>>>,
    abort: AbortHandle,
    identity: Arc<()>,
}

impl Pending {
    fn new(task: JoinHandle<Result<(), String>>) -> Self {
        Self {
            abort: task.abort_handle(),
            task: Some(task),
            identity: Arc::new(()),
        }
    }
}

/// The object served at `/io/github/zbndev/Tidemark`.
#[derive(Debug)]
pub struct Daemon {
    statuses: Published,
    catalog: Vec<ProviderDefinition>,
    configured: RwLock<HashSet<AccountKey>>,
    commands: mpsc::Sender<Command>,
    secrets: Arc<dyn Secrets>,
    logins: Mutex<HashMap<AccountKey, Pending>>,
    mutations: Mutex<HashMap<AccountKey, AccountMutation>>,
}

impl Daemon {
    /// Wires the interface to the published state, the poll loop and the keyring.
    pub fn new(
        statuses: Published,
        catalog: Vec<ProviderDefinition>,
        configured: Vec<AccountKey>,
        commands: mpsc::Sender<Command>,
        secrets: Arc<dyn Secrets>,
    ) -> Self {
        Self {
            statuses,
            catalog,
            configured: RwLock::new(configured.into_iter().collect()),
            commands,
            secrets,
            logins: Mutex::new(HashMap::new()),
            mutations: Mutex::new(HashMap::new()),
        }
    }

    /// The account named by a call, or the error a client gets for naming one that is not
    /// there — a typo must be visible rather than silently doing nothing.
    async fn account(&self, provider: &str, account: &str) -> fdo::Result<ProviderStatus> {
        if !self
            .configured
            .read()
            .await
            .contains(&key(provider, account))
        {
            return Err(fdo::Error::InvalidArgs(format!(
                "no account {account} is configured for {provider}"
            )));
        }
        if let Some(status) = self.statuses.find(provider, account).await {
            return Ok(status);
        }

        let definition = self
            .catalog
            .iter()
            .find(|definition| definition.provider == provider)
            .ok_or_else(|| {
                fdo::Error::InvalidArgs(format!(
                    "provider {provider} is not supported by this build"
                ))
            })?;
        let mut status =
            ProviderStatus::pending(&ProviderId::new(provider), &AccountId::new(account));
        status.credential = Some(definition.credential.clone());
        status.credential_hint = Some(definition.credential_hint.clone());
        status.options = definition.options.clone();
        Ok(status)
    }

    /// Whether loaded topology contains any account for this provider.
    async fn configured_provider(&self, provider: &str) -> bool {
        self.configured
            .read()
            .await
            .iter()
            .any(|(configured, _)| configured == provider)
    }

    /// Tells the poll loop that a credential or a setting changed.
    async fn reload(&self, provider: &str) -> fdo::Result<()> {
        self.commands
            .send(Command::Reload {
                provider: Some(provider.to_owned()),
            })
            .await
            .map_err(|_| fdo::Error::Failed("the poll loop has stopped".into()))
    }

    /// Sends a configuration mutation and waits until the poll loop has persisted it.
    async fn config_request(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<(), String>>) -> Command,
    ) -> fdo::Result<()> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(make(reply))
            .await
            .map_err(|_| fdo::Error::Failed("the poll loop has stopped".into()))?;
        answer
            .await
            .map_err(|_| fdo::Error::Failed("the poll loop dropped the request".into()))?
            .map_err(fdo::Error::Failed)
    }

    /// The lock serializing credential and topology mutations for one account.
    async fn mutation(&self, provider: &str, account: &str) -> AccountMutation {
        Arc::clone(
            self.mutations
                .lock()
                .await
                .entry(key(provider, account))
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Removes and aborts a pending login, if one exists.
    async fn cancel_pending_login(&self, provider: &str, account: &str) {
        if let Some(mut pending) = self.logins.lock().await.remove(&key(provider, account)) {
            pending.abort.abort();
            if let Some(task) = pending.task.take() {
                let _ = task.await;
            } else {
                while !pending.abort.is_finished() {
                    tokio::task::yield_now().await;
                }
            }
            tracing::info!(provider, account, "login cancelled");
        }
    }
}

/// The D-Bus error a keyring failure becomes.
///
/// A locked keyring is the one that matters: it is not the caller's mistake and not a
/// permanent failure, so it says what to do rather than reporting a fault.
fn keyring_error(error: SecretError) -> fdo::Error {
    match error {
        SecretError::Locked => {
            fdo::Error::Failed("the keyring is locked; unlock it and try again".into())
        }
        other => fdo::Error::Failed(other.to_string()),
    }
}

/// Commits the completed OAuth document under the same account mutation guard as sign-out
/// and removal. Discovery and token exchange happen before this short critical section.
async fn commit_login(
    secrets: &dyn Secrets,
    mutation: &AccountMutation,
    provider: &str,
    account: &str,
    document: &Credential,
) -> Result<(), SecretError> {
    let _guard = mutation.lock().await;
    secrets
        .set(
            Kind::Token,
            &ProviderId::new(provider),
            &AccountId::new(account),
            document,
        )
        .await
}

#[interface(name = "io.github.zbndev.Tidemark.Daemon1")]
impl Daemon {
    /// Every provider this build knows how to configure.
    async fn list_providers(&self) -> Vec<ProviderDefinition> {
        self.catalog.clone()
    }

    /// Adds a compiled-in provider's default account and waits for it to be persisted.
    async fn add_provider(&self, provider: &str) -> fdo::Result<()> {
        let mutation = self.mutation(provider, AccountId::default().as_str()).await;
        let _guard = mutation.lock().await;
        if !self
            .catalog
            .iter()
            .any(|definition| definition.provider == provider)
        {
            return Err(fdo::Error::InvalidArgs(format!(
                "provider {provider} is not supported by this build"
            )));
        }
        self.config_request(|reply| Command::AddProvider {
            provider: provider.to_owned(),
            reply,
        })
        .await?;
        self.configured
            .write()
            .await
            .insert(key(provider, AccountId::default().as_str()));
        Ok(())
    }

    /// Removes one configured account after clearing every credential Tidemark owns.
    async fn remove_provider(&self, provider: &str, account: &str) -> fdo::Result<()> {
        let mutation = self.mutation(provider, account).await;
        let _guard = mutation.lock().await;
        self.account(provider, account).await?;
        self.cancel_pending_login(provider, account).await;

        let provider_id = ProviderId::new(provider);
        let account_id = AccountId::new(account);
        self.secrets
            .delete(Kind::Key, &provider_id, &account_id)
            .await
            .map_err(keyring_error)?;
        self.secrets
            .delete(Kind::Token, &provider_id, &account_id)
            .await
            .map_err(keyring_error)?;

        self.config_request(|reply| Command::RemoveProvider {
            provider: provider.to_owned(),
            account: account.to_owned(),
            reply,
        })
        .await?;
        self.configured
            .write()
            .await
            .remove(&key(provider, account));
        Ok(())
    }

    /// Every account the daemon watches, with its current state and last good reading.
    ///
    /// Never empty while accounts are configured: they are published as `pending` before
    /// the first poll, so a client can tell "nothing configured" from "nothing yet".
    async fn get_status(&self) -> Vec<ProviderStatus> {
        self.statuses.all().await
    }

    /// Stored points in the open segment of one window.
    ///
    /// The service only validates and relays this request. `Engine` owns the SQLite
    /// connection, so the query shares its serial command queue with writes and polling.
    async fn current_segment(
        &self,
        provider: &str,
        account: &str,
        window: &str,
    ) -> fdo::Result<Vec<HistoryPoint>> {
        self.account(provider, account).await?;
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(Command::CurrentSegment {
                provider: provider.to_owned(),
                account: account.to_owned(),
                window: window.to_owned(),
                reply,
            })
            .await
            .map_err(|_| fdo::Error::Failed("the poll loop has stopped".into()))?;
        answer
            .await
            .map_err(|_| fdo::Error::Failed("the poll loop dropped the request".into()))?
            .map_err(fdo::Error::Failed)
    }

    /// Polls now: one provider by slug, or every account when given an empty string.
    ///
    /// The credential is read again as part of this, so it is also the call the settings
    /// dialog makes after storing a new key.
    async fn refresh(&self, provider: &str) -> fdo::Result<()> {
        let target = (!provider.is_empty()).then(|| provider.to_owned());
        if let Some(slug) = target.as_deref()
            && !self.configured_provider(slug).await
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

    /// Stores an API key for an account, and polls it straight away.
    ///
    /// The key is not validated here beyond being non-blank: the only authority on whether
    /// a key works is the provider, and the poll this triggers is what asks it. A bad key
    /// arrives back as `credential-rejected`, which is a state the interface already draws.
    async fn set_key(&self, provider: &str, account: &str, key: &str) -> fdo::Result<()> {
        let mutation = self.mutation(provider, account).await;
        let _guard = mutation.lock().await;
        let status = self.account(provider, account).await?;
        if status.credential_kind() != Some(CredentialKind::Key) {
            return Err(fdo::Error::InvalidArgs(format!(
                "{provider} does not take an API key"
            )));
        }
        let key = key.trim();
        if key.is_empty() {
            return Err(fdo::Error::InvalidArgs(
                "an empty key is not a key; use SignOut to remove one".into(),
            ));
        }
        self.secrets
            .set(
                Kind::Key,
                &ProviderId::new(provider),
                &AccountId::new(account),
                &Credential::new(key),
            )
            .await
            .map_err(keyring_error)?;
        tracing::info!(provider, account, "stored an API key");
        self.reload(provider).await
    }

    /// Removes whatever credential Tidemark holds for an account.
    ///
    /// For an OAuth provider this hands the account back to the vendor CLI's own login if
    /// there is one, which is why it is "sign out of Tidemark" rather than "sign out": the
    /// card may well go on working, from the credential it was reading before.
    async fn sign_out(&self, provider: &str, account: &str) -> fdo::Result<()> {
        let mutation = self.mutation(provider, account).await;
        let _guard = mutation.lock().await;
        let status = self.account(provider, account).await?;
        let Some(kind) = status.credential_kind().and_then(stored_kind) else {
            return Err(fdo::Error::InvalidArgs(format!(
                "Tidemark holds no credential for {provider}"
            )));
        };
        self.secrets
            .delete(kind, &ProviderId::new(provider), &AccountId::new(account))
            .await
            .map_err(keyring_error)?;
        tracing::info!(provider, account, "removed the stored credential");
        self.reload(provider).await
    }

    /// Starts a login and returns the URL to open in a browser.
    ///
    /// Returns as soon as the callback port is held and the URL exists. Nothing has been
    /// waited for yet: call `AwaitLogin` for that. A login already in progress for this
    /// account is replaced, so a user who abandoned a browser tab is not locked out of
    /// starting again.
    async fn begin_login(&self, provider: &str, account: &str) -> fdo::Result<String> {
        let mutation = self.mutation(provider, account).await;
        let _guard = mutation.lock().await;
        let status = self.account(provider, account).await?;
        if status.credential_kind() != Some(CredentialKind::OAuth) {
            return Err(fdo::Error::InvalidArgs(format!(
                "{provider} does not sign in through Tidemark"
            )));
        }
        let client = registry::oauth_client(provider).ok_or_else(|| {
            fdo::Error::Failed(format!("this build has no OAuth client for {provider}"))
        })?;

        self.cancel_pending_login(provider, account).await;

        let login = Login::begin(client)
            .await
            .map_err(|error| fdo::Error::Failed(error.to_string()))?;
        let url = login.url().to_owned();

        let secrets = Arc::clone(&self.secrets);
        let commands = self.commands.clone();
        let provider_name = provider.to_owned();
        let account_name = account.to_owned();
        let credential_mutation = Arc::clone(&mutation);
        let task = tokio::spawn(async move {
            let http = tidemark_core::oauth::client().map_err(|error| error.to_string())?;
            let response = login
                .finish(&http)
                .await
                .map_err(|error| error.to_string())?;
            let now_ms = tidemark_types::Timestamp::now()
                .as_unix()
                .saturating_mul(1_000);
            let document = registry::login_document(&provider_name, &response, now_ms)
                .await
                .map_err(|error| error.to_string())?;
            commit_login(
                secrets.as_ref(),
                &credential_mutation,
                &provider_name,
                &account_name,
                &Credential::new(document.to_string()),
            )
            .await
            .map_err(|error| error.to_string())?;
            tracing::info!(provider = %provider_name, "signed in");
            let _ = commands
                .send(Command::Reload {
                    provider: Some(provider_name),
                })
                .await;
            Ok(())
        });
        self.logins
            .lock()
            .await
            .insert(key(provider, account), Pending::new(task));
        Ok(url)
    }

    /// Waits for a login started by `BeginLogin` to finish.
    ///
    /// Long-running by design — up to the browser timeout — and there is no D-Bus timeout
    /// on our side to shorten it. Returns when the tokens are stored, or an error saying
    /// what went wrong instead.
    async fn await_login(&self, provider: &str, account: &str) -> fdo::Result<()> {
        let (task, identity) = {
            let mut logins = self.logins.lock().await;
            let pending = logins
                .get_mut(&key(provider, account))
                .ok_or_else(|| fdo::Error::Failed("no login is in progress".into()))?;
            let task = pending
                .task
                .take()
                .ok_or_else(|| fdo::Error::Failed("the login is already being awaited".into()))?;
            (task, Arc::clone(&pending.identity))
        };
        let result = match task.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(reason)) => Err(fdo::Error::Failed(reason)),
            Err(error) if error.is_cancelled() => {
                Err(fdo::Error::Failed("the login was cancelled".into()))
            }
            Err(error) => Err(fdo::Error::Failed(error.to_string())),
        };
        let login_key = key(provider, account);
        let mut logins = self.logins.lock().await;
        if logins
            .get(&login_key)
            .is_some_and(|pending| Arc::ptr_eq(&pending.identity, &identity))
        {
            logins.remove(&login_key);
        }
        result
    }

    /// Abandons a login in progress and gives the callback port back.
    ///
    /// Not an error when there is nothing to cancel: a client that closed its dialog should
    /// not have to know whether the login had already finished.
    async fn cancel_login(&self, provider: &str, account: &str) -> fdo::Result<()> {
        let mutation = self.mutation(provider, account).await;
        let _guard = mutation.lock().await;
        self.cancel_pending_login(provider, account).await;
        Ok(())
    }

    /// Changes one of a provider's settings and polls it again.
    ///
    /// The value is checked against the choices the account publishes, so a client cannot
    /// write something into `config.toml` that no build knows how to read.
    async fn set_option(
        &self,
        provider: &str,
        account: &str,
        name: &str,
        value: &str,
    ) -> fdo::Result<()> {
        let mutation = self.mutation(provider, account).await;
        let _guard = mutation.lock().await;
        let status = self.account(provider, account).await?;
        let option = status
            .options
            .iter()
            .find(|option| option.name == name)
            .ok_or_else(|| {
                fdo::Error::InvalidArgs(format!("{provider} has no setting called {name}"))
            })?;
        if !option.choices.iter().any(|choice| choice.value == value) {
            return Err(fdo::Error::InvalidArgs(format!(
                "{value} is not one of the values {name} can take"
            )));
        }

        self.config_request(|reply| Command::SetOption {
            provider: provider.to_owned(),
            account: account.to_owned(),
            name: name.to_owned(),
            value: value.to_owned(),
            reply,
        })
        .await?;
        tracing::info!(provider, name, value, "setting changed");
        Ok(())
    }

    /// Switches one window's notifications on or off.
    ///
    /// Per window rather than per provider, and off by default: the five providers report
    /// fifteen windows between them, and a warning about every one of them is a warning
    /// about none. The engine validates the window against what the account currently
    /// reports and owns the write, so this is the same serialized configuration
    /// transaction as `SetOption`.
    async fn set_window_notify(
        &self,
        provider: &str,
        account: &str,
        window: &str,
        enabled: bool,
    ) -> fdo::Result<()> {
        let mutation = self.mutation(provider, account).await;
        let _guard = mutation.lock().await;
        self.account(provider, account).await?;

        self.config_request(|reply| Command::SetWindowNotify {
            provider: provider.to_owned(),
            account: account.to_owned(),
            window: window.to_owned(),
            enabled,
            reply,
        })
        .await?;
        tracing::info!(provider, window, enabled, "notification switch changed");
        Ok(())
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

    /// One configured account was removed after its owned credentials were deleted.
    #[zbus(signal)]
    pub async fn provider_removed(
        emitter: &SignalEmitter<'_>,
        provider: &str,
        account: &str,
    ) -> zbus::Result<()>;
}

/// The pair an in-progress login is filed under.
fn key(provider: &str, account: &str) -> AccountKey {
    (provider.to_owned(), account.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::{AccountId, ProviderDefinition, ProviderId, ids};

    fn status(provider: &str) -> ProviderStatus {
        ProviderStatus::pending(&ProviderId::new(provider), &AccountId::default())
    }

    /// A keyring that remembers what it was told, so the credential calls can be checked
    /// without a Secret Service and without a bus.
    #[derive(Debug, Default)]
    struct FakeSecrets(std::sync::Mutex<HashMap<(String, String, String), String>>);

    impl FakeSecrets {
        fn held(&self) -> Vec<(String, String, String, String)> {
            let map = self.0.lock().expect("no test panics holding this");
            let mut entries: Vec<_> = map
                .iter()
                .map(|((kind, provider, account), secret)| {
                    (
                        kind.clone(),
                        provider.clone(),
                        account.clone(),
                        secret.clone(),
                    )
                })
                .collect();
            entries.sort();
            entries
        }
    }

    fn slot(kind: Kind, provider: &ProviderId, account: &AccountId) -> (String, String, String) {
        let kind = match kind {
            Kind::Key => "key",
            Kind::Token => "token",
        };
        (
            kind.to_owned(),
            provider.as_str().to_owned(),
            account.as_str().to_owned(),
        )
    }

    impl Secrets for FakeSecrets {
        fn get<'a>(
            &'a self,
            kind: Kind,
            provider: &'a ProviderId,
            account: &'a AccountId,
        ) -> tidemark_core::providers::BoxFuture<'a, Result<Option<Credential>, SecretError>>
        {
            let held = self
                .0
                .lock()
                .expect("no test panics holding this")
                .get(&slot(kind, provider, account))
                .cloned()
                .map(Credential::new);
            Box::pin(async move { Ok(held) })
        }

        fn set<'a>(
            &'a self,
            kind: Kind,
            provider: &'a ProviderId,
            account: &'a AccountId,
            secret: &'a Credential,
        ) -> tidemark_core::providers::BoxFuture<'a, Result<(), SecretError>> {
            self.0
                .lock()
                .expect("no test panics holding this")
                .insert(slot(kind, provider, account), secret.expose().to_owned());
            Box::pin(async { Ok(()) })
        }

        fn compare_and_set<'a>(
            &'a self,
            kind: Kind,
            provider: &'a ProviderId,
            account: &'a AccountId,
            expected: &'a Credential,
            replacement: &'a Credential,
        ) -> tidemark_core::providers::BoxFuture<'a, Result<bool, SecretError>> {
            let mut held = self.0.lock().expect("no test panics holding this");
            let slot = slot(kind, provider, account);
            let matches = held.get(&slot).map(String::as_str) == Some(expected.expose());
            if matches {
                held.insert(slot, replacement.expose().to_owned());
            }
            Box::pin(async move { Ok(matches) })
        }

        fn delete<'a>(
            &'a self,
            kind: Kind,
            provider: &'a ProviderId,
            account: &'a AccountId,
        ) -> tidemark_core::providers::BoxFuture<'a, Result<(), SecretError>> {
            self.0
                .lock()
                .expect("no test panics holding this")
                .remove(&slot(kind, provider, account));
            Box::pin(async { Ok(()) })
        }
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

    #[tokio::test]
    async fn removing_a_published_account_does_not_reorder_the_rest() {
        let published = Published::default();
        published.upsert(status("zai")).await;
        published.upsert(status("kimi")).await;

        assert!(published.remove("zai", "default").await.is_some());
        assert_eq!(published.all().await[0].provider, "kimi");
    }

    fn key_account(provider: &str) -> ProviderStatus {
        let mut status = status(provider);
        status.credential = Some(CredentialKind::Key.as_wire().to_owned());
        status
    }

    fn oauth_account(provider: &str) -> ProviderStatus {
        let mut status = status(provider);
        status.credential = Some(CredentialKind::OAuth.as_wire().to_owned());
        status
    }

    fn catalog() -> Vec<ProviderDefinition> {
        vec![ProviderDefinition {
            provider: "zai".into(),
            title: "Z.ai".into(),
            credential: "key".into(),
            credential_hint: "Z.ai dashboard → API keys.".into(),
            external_fallback: None,
            options: Vec::new(),
        }]
    }

    /// A daemon over a fixed set of accounts, with the channel its commands land on.
    async fn daemon_over(
        accounts: Vec<ProviderStatus>,
    ) -> (Daemon, Arc<FakeSecrets>, mpsc::Receiver<Command>) {
        daemon_over_catalog(accounts, Vec::new()).await
    }

    async fn daemon_over_catalog(
        accounts: Vec<ProviderStatus>,
        catalog: Vec<ProviderDefinition>,
    ) -> (Daemon, Arc<FakeSecrets>, mpsc::Receiver<Command>) {
        let configured = accounts
            .iter()
            .map(|account| (account.provider.clone(), account.account.clone()))
            .collect();
        let published = Published::default();
        for account in accounts {
            published.upsert(account).await;
        }
        let secrets = Arc::new(FakeSecrets::default());
        let (tx, rx) = mpsc::channel(8);
        (
            Daemon::new(
                published,
                catalog,
                configured,
                tx,
                Arc::clone(&secrets) as Arc<dyn Secrets>,
            ),
            secrets,
            rx,
        )
    }

    struct StaleRefresh {
        loaded: oneshot::Receiver<()>,
        release: oneshot::Sender<()>,
        task: tokio::task::JoinHandle<bool>,
    }

    fn stale_refresh_barrier(secrets: Arc<FakeSecrets>, provider: &'static str) -> StaleRefresh {
        let (loaded_tx, loaded) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let provider = ProviderId::new(provider);
            let account = AccountId::default();
            let expected = secrets
                .get(Kind::Token, &provider, &account)
                .await
                .expect("refresh reads the keyring")
                .expect("expired token seeded");
            loaded_tx.send(()).expect("test waits at refresh barrier");
            release_rx.await.expect("refresh released");
            secrets
                .compare_and_set(
                    Kind::Token,
                    &provider,
                    &account,
                    &expected,
                    &Credential::new("stale-refresh"),
                )
                .await
                .expect("refresh persistence reaches keyring")
        });
        StaleRefresh {
            loaded,
            release,
            task,
        }
    }

    async fn seed_token(secrets: &FakeSecrets, provider: &str, document: &str) {
        secrets
            .set(
                Kind::Token,
                &ProviderId::new(provider),
                &AccountId::default(),
                &Credential::new(document),
            )
            .await
            .expect("token seeded");
    }

    #[tokio::test]
    async fn the_daemon_lists_the_catalog_even_with_no_statuses() {
        let definition = ProviderDefinition {
            provider: "zai".into(),
            title: "Z.ai".into(),
            credential: "key".into(),
            credential_hint: "Z.ai dashboard → API keys.".into(),
            external_fallback: None,
            options: Vec::new(),
        };
        let (daemon, _secrets, _commands) =
            daemon_over_catalog(Vec::new(), vec![definition.clone()]).await;

        assert_eq!(daemon.list_providers().await, vec![definition]);
    }

    #[tokio::test]
    async fn a_configured_account_validates_before_first_status_publication() {
        let published = Published::default();
        let secrets = Arc::new(FakeSecrets::default());
        let (commands, mut command_queue) = mpsc::channel(4);
        let daemon = Daemon::new(
            published,
            catalog(),
            vec![("zai".into(), "default".into())],
            commands,
            Arc::clone(&secrets) as Arc<dyn Secrets>,
        );

        daemon
            .refresh("zai")
            .await
            .expect("loaded identity refreshes before publication");
        assert!(matches!(
            command_queue.try_recv().expect("the loop was told"),
            Command::Refresh(Some(provider)) if provider == "zai"
        ));

        daemon
            .set_key("zai", "default", "startup-key")
            .await
            .expect("loaded configuration is authoritative before publication");

        assert_eq!(
            secrets.held(),
            vec![(
                "key".into(),
                "zai".into(),
                "default".into(),
                "startup-key".into()
            )]
        );
        assert!(matches!(
            command_queue.try_recv().expect("the loop was told"),
            Command::Reload { provider: Some(provider) } if provider == "zai"
        ));
    }

    #[tokio::test]
    async fn current_segment_is_serialized_through_the_engine() {
        let (daemon, _secrets, mut commands) = daemon_over(vec![key_account("zai")]).await;
        let responder = tokio::spawn(async move {
            let Command::CurrentSegment {
                provider,
                account,
                window,
                reply,
            } = commands.recv().await.expect("command")
            else {
                panic!("unexpected command");
            };
            assert_eq!(
                (provider.as_str(), account.as_str(), window.as_str()),
                ("zai", "default", "w18000")
            );
            reply
                .send(Ok(vec![tidemark_types::HistoryPoint {
                    captured_at: 1_785_700_000,
                    used_percent: 42.0,
                }]))
                .expect("caller waits");
        });

        let points = daemon
            .current_segment("zai", "default", "w18000")
            .await
            .expect("configured account reads history");
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].used_percent, 42.0);
        responder.await.expect("responder finishes");
    }

    #[tokio::test]
    async fn current_segment_for_an_unconfigured_account_does_not_reach_the_engine() {
        let (daemon, _secrets, mut commands) = daemon_over(Vec::new()).await;
        let error = daemon
            .current_segment("zai", "default", "w18000")
            .await
            .expect_err("unknown account is rejected");
        assert!(matches!(error, fdo::Error::InvalidArgs(_)));
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn adding_waits_for_the_engine_result() {
        let (daemon, _secrets, mut commands) = daemon_over_catalog(Vec::new(), catalog()).await;
        let responder = tokio::spawn(async move {
            match commands.recv().await.expect("command") {
                Command::AddProvider { provider, reply } => {
                    assert_eq!(provider, "zai");
                    reply.send(Ok(())).expect("caller waits");
                }
                command => panic!("unexpected command: {command:?}"),
            }
        });

        daemon.add_provider("zai").await.expect("added");
        responder.await.expect("responder finished");
    }

    #[tokio::test]
    async fn adding_an_unknown_provider_is_an_invalid_argument() {
        let (daemon, _secrets, mut commands) = daemon_over_catalog(Vec::new(), catalog()).await;

        let error = daemon
            .add_provider("not-a-provider")
            .await
            .expect_err("unknown providers are rejected before the engine");

        assert!(matches!(error, fdo::Error::InvalidArgs(_)));
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn an_engine_add_failure_is_returned_to_the_caller() {
        let (daemon, _secrets, mut commands) = daemon_over_catalog(Vec::new(), catalog()).await;
        let responder = tokio::spawn(async move {
            let Command::AddProvider { reply, .. } = commands.recv().await.expect("command") else {
                panic!("unexpected command");
            };
            reply
                .send(Err("config is read-only".into()))
                .expect("caller waits");
        });

        let error = daemon
            .add_provider("zai")
            .await
            .expect_err("engine failure crosses D-Bus boundary");

        assert!(matches!(error, fdo::Error::Failed(reason) if reason == "config is read-only"));
        responder.await.expect("responder finished");
    }

    #[tokio::test]
    async fn removing_deletes_both_owned_secret_kinds_before_the_engine_request() {
        let (daemon, secrets, mut commands) =
            daemon_over_catalog(vec![key_account("zai")], catalog()).await;
        for kind in [Kind::Key, Kind::Token] {
            secrets
                .set(
                    kind,
                    &ProviderId::new("zai"),
                    &AccountId::default(),
                    &Credential::new("owned"),
                )
                .await
                .expect("seeded");
        }
        let observed = Arc::clone(&secrets);
        let responder = tokio::spawn(async move {
            match commands.recv().await.expect("command") {
                Command::RemoveProvider {
                    provider,
                    account,
                    reply,
                } => {
                    assert_eq!((provider.as_str(), account.as_str()), ("zai", "default"));
                    assert!(
                        observed.held().is_empty(),
                        "both secrets are gone before topology changes"
                    );
                    reply.send(Ok(())).expect("caller waits");
                }
                command => panic!("unexpected command: {command:?}"),
            }
        });

        daemon
            .remove_provider("zai", "default")
            .await
            .expect("removed");

        assert!(secrets.held().is_empty());
        responder.await.expect("responder finished");
    }

    #[tokio::test]
    async fn a_credential_mutation_cannot_overlap_successful_removal() {
        let (daemon, secrets, mut commands) =
            daemon_over_catalog(vec![key_account("zai")], catalog()).await;
        let daemon = Arc::new(daemon);
        let (remove_seen, removal_reached_engine) = tokio::sync::oneshot::channel();
        let (finish_removal, release_engine_reply) = tokio::sync::oneshot::channel();
        let responder = tokio::spawn(async move {
            let Command::RemoveProvider { reply, .. } = commands.recv().await.expect("command")
            else {
                panic!("unexpected command");
            };
            remove_seen.send(()).expect("test is waiting");
            let _ = release_engine_reply.await;
            reply.send(Ok(())).expect("caller waits");
            commands
        });
        let removing = tokio::spawn({
            let daemon = Arc::clone(&daemon);
            async move { daemon.remove_provider("zai", "default").await }
        });
        removal_reached_engine
            .await
            .expect("removal reached the engine after deleting secrets");

        let setting = tokio::spawn({
            let daemon = Arc::clone(&daemon);
            async move { daemon.set_key("zai", "default", "too-late").await }
        });
        for _ in 0..10 {
            if setting.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(
            !setting.is_finished(),
            "credential mutation must wait for removal to finish"
        );

        finish_removal.send(()).expect("responder waits");
        removing
            .await
            .expect("removal task did not panic")
            .expect("engine accepted removal");
        let error = setting
            .await
            .expect("credential task did not panic")
            .expect_err("the account is no longer configured");
        assert!(matches!(error, fdo::Error::InvalidArgs(_)));
        assert!(secrets.held().is_empty());
        let mut commands = responder.await.expect("responder finished");
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn stale_refresh_cannot_undo_the_real_sign_out_mutation() {
        const PROVIDER: &str = "antigravity";
        let (daemon, secrets, mut commands) = daemon_over(vec![oauth_account(PROVIDER)]).await;
        seed_token(&secrets, PROVIDER, "expired-login").await;
        let StaleRefresh {
            loaded,
            release,
            task,
        } = stale_refresh_barrier(Arc::clone(&secrets), PROVIDER);
        loaded.await.expect("refresh loaded the expired login");

        daemon
            .sign_out(PROVIDER, "default")
            .await
            .expect("sign-out completes while refresh is remote");
        release.send(()).expect("release stale refresh");

        assert!(!task.await.expect("refresh task did not panic"));
        assert!(secrets.held().is_empty());
        assert!(matches!(
            commands.recv().await.expect("sign-out wakes the poll loop"),
            Command::Reload { provider: Some(provider) } if provider == PROVIDER
        ));
    }

    #[tokio::test]
    async fn stale_refresh_cannot_restore_a_token_after_real_provider_removal() {
        const PROVIDER: &str = "antigravity";
        let (daemon, secrets, mut commands) = daemon_over(vec![oauth_account(PROVIDER)]).await;
        seed_token(&secrets, PROVIDER, "expired-login").await;
        let StaleRefresh {
            loaded,
            release,
            task,
        } = stale_refresh_barrier(Arc::clone(&secrets), PROVIDER);
        loaded.await.expect("refresh loaded the expired login");
        let responder = tokio::spawn(async move {
            let Command::RemoveProvider { reply, .. } =
                commands.recv().await.expect("removal reaches engine")
            else {
                panic!("unexpected command");
            };
            reply.send(Ok(())).expect("caller waits for removal");
        });

        daemon
            .remove_provider(PROVIDER, "default")
            .await
            .expect("provider removed while refresh is remote");
        assert!(matches!(
            daemon.account(PROVIDER, "default").await,
            Err(fdo::Error::InvalidArgs(_))
        ));
        release.send(()).expect("release stale refresh");

        assert!(!task.await.expect("refresh task did not panic"));
        assert!(
            secrets.held().is_empty(),
            "unconfigured provider has no token"
        );
        responder.await.expect("responder did not panic");
    }

    #[tokio::test]
    async fn stale_refresh_cannot_overwrite_the_real_login_commit() {
        const PROVIDER: &str = "antigravity";
        let (daemon, secrets, _commands) = daemon_over(vec![oauth_account(PROVIDER)]).await;
        seed_token(&secrets, PROVIDER, "expired-login").await;
        let StaleRefresh {
            loaded,
            release,
            task,
        } = stale_refresh_barrier(Arc::clone(&secrets), PROVIDER);
        loaded.await.expect("refresh loaded the expired login");

        let mutation = daemon.mutation(PROVIDER, "default").await;
        commit_login(
            secrets.as_ref(),
            &mutation,
            PROVIDER,
            "default",
            &Credential::new("new-login"),
        )
        .await
        .expect("new login committed while refresh is remote");
        release.send(()).expect("release stale refresh");

        assert!(!task.await.expect("refresh task did not panic"));
        let stored = secrets
            .get(
                Kind::Token,
                &ProviderId::new(PROVIDER),
                &AccountId::default(),
            )
            .await
            .expect("keyring read")
            .expect("new login remains stored");
        assert_eq!(stored.expose(), "new-login");
    }

    #[derive(Debug, Default)]
    struct FailingDeleteSecrets;

    impl Secrets for FailingDeleteSecrets {
        fn get<'a>(
            &'a self,
            _kind: Kind,
            _provider: &'a ProviderId,
            _account: &'a AccountId,
        ) -> tidemark_core::providers::BoxFuture<'a, Result<Option<Credential>, SecretError>>
        {
            Box::pin(async { Ok(None) })
        }

        fn set<'a>(
            &'a self,
            _kind: Kind,
            _provider: &'a ProviderId,
            _account: &'a AccountId,
            _secret: &'a Credential,
        ) -> tidemark_core::providers::BoxFuture<'a, Result<(), SecretError>> {
            Box::pin(async { Ok(()) })
        }

        fn compare_and_set<'a>(
            &'a self,
            _kind: Kind,
            _provider: &'a ProviderId,
            _account: &'a AccountId,
            _expected: &'a Credential,
            _replacement: &'a Credential,
        ) -> tidemark_core::providers::BoxFuture<'a, Result<bool, SecretError>> {
            Box::pin(async { Ok(false) })
        }

        fn delete<'a>(
            &'a self,
            _kind: Kind,
            _provider: &'a ProviderId,
            _account: &'a AccountId,
        ) -> tidemark_core::providers::BoxFuture<'a, Result<(), SecretError>> {
            Box::pin(async { Err(SecretError::Locked) })
        }
    }

    #[tokio::test]
    async fn a_secret_delete_failure_leaves_the_provider_configured() {
        let published = Published::default();
        published.upsert(key_account("zai")).await;
        let (commands, mut command_queue) = mpsc::channel(4);
        let daemon = Daemon::new(
            published,
            catalog(),
            vec![("zai".into(), "default".into())],
            commands,
            Arc::new(FailingDeleteSecrets),
        );

        assert!(daemon.remove_provider("zai", "default").await.is_err());
        assert!(matches!(
            command_queue.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn removing_an_unconfigured_account_is_an_invalid_argument() {
        let (daemon, _secrets, mut commands) = daemon_over_catalog(Vec::new(), catalog()).await;

        let error = daemon
            .remove_provider("zai", "default")
            .await
            .expect_err("the catalog alone is not a configured account");

        assert!(matches!(error, fdo::Error::InvalidArgs(_)));
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn removing_an_account_cancels_its_pending_login() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let (daemon, _secrets, mut commands) =
            daemon_over_catalog(vec![oauth_account("zai")], catalog()).await;
        let dropped = Arc::new(AtomicBool::new(false));
        let task_dropped = Arc::clone(&dropped);
        let (started, ready) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _guard = Dropped(task_dropped);
            started.send(()).expect("test is waiting");
            std::future::pending::<()>().await;
            Ok(())
        });
        ready.await.expect("pending task started");
        daemon
            .logins
            .lock()
            .await
            .insert(key("zai", "default"), Pending::new(task));
        let responder = tokio::spawn(async move {
            let Command::RemoveProvider { reply, .. } = commands.recv().await.expect("command")
            else {
                panic!("unexpected command");
            };
            reply.send(Ok(())).expect("caller waits");
        });

        daemon
            .remove_provider("zai", "default")
            .await
            .expect("removed");
        for _ in 0..10 {
            if dropped.load(Ordering::SeqCst) {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert!(dropped.load(Ordering::SeqCst), "the login task was aborted");
        assert!(daemon.logins.lock().await.is_empty());
        responder.await.expect("responder finished");
    }

    #[tokio::test]
    async fn a_stored_key_lands_in_the_keyring_and_wakes_the_poll_loop() {
        let (daemon, secrets, mut commands) = daemon_over(vec![key_account("zai")]).await;

        daemon
            .set_key("zai", "default", "  sk-pasted-with-whitespace  ")
            .await
            .expect("a key is stored");

        assert_eq!(
            secrets.held(),
            vec![(
                "key".to_owned(),
                "zai".to_owned(),
                "default".to_owned(),
                // Trimmed: a key copied out of a web page arrives with a newline on it,
                // and a bearer token with trailing whitespace is a 401 nobody can explain.
                "sk-pasted-with-whitespace".to_owned()
            )]
        );
        assert!(matches!(
            commands.try_recv().expect("the loop was told"),
            Command::Reload { provider: Some(provider) } if provider == "zai"
        ));
    }

    #[tokio::test]
    async fn an_empty_key_is_refused_rather_than_stored_as_one() {
        let (daemon, secrets, _commands) = daemon_over(vec![key_account("zai")]).await;
        assert!(daemon.set_key("zai", "default", "   ").await.is_err());
        assert!(secrets.held().is_empty());
    }

    #[tokio::test]
    async fn a_key_is_not_offered_to_a_provider_that_signs_in() {
        let (daemon, secrets, _commands) = daemon_over(vec![oauth_account("claude")]).await;
        assert!(daemon.set_key("claude", "default", "sk-1").await.is_err());
        assert!(daemon.set_key("zai", "default", "sk-1").await.is_err());
        assert!(secrets.held().is_empty());
    }

    #[tokio::test]
    async fn signing_out_removes_the_credential_of_the_right_kind() {
        let (daemon, secrets, mut commands) =
            daemon_over(vec![key_account("zai"), oauth_account("claude")]).await;
        daemon
            .set_key("zai", "default", "sk-1")
            .await
            .expect("stored");
        let _ = commands.try_recv();
        secrets
            .set(
                Kind::Token,
                &ProviderId::new("claude"),
                &AccountId::default(),
                &Credential::new("{}"),
            )
            .await
            .expect("stored");

        daemon
            .sign_out("claude", "default")
            .await
            .expect("signed out");

        assert_eq!(
            secrets.held(),
            vec![(
                "key".to_owned(),
                "zai".to_owned(),
                "default".to_owned(),
                "sk-1".to_owned()
            )],
            "signing out of one account must not take another account's key with it"
        );
        assert!(matches!(
            commands.try_recv().expect("the loop was told"),
            Command::Reload { provider: Some(provider) } if provider == "claude"
        ));
    }

    #[tokio::test]
    async fn an_account_whose_credential_is_not_ours_has_nothing_to_sign_out_of() {
        let mut external = status("antigravity");
        external.credential = Some(CredentialKind::External.as_wire().to_owned());
        let (daemon, _secrets, _commands) = daemon_over(vec![external]).await;
        assert!(daemon.sign_out("antigravity", "default").await.is_err());
    }

    #[tokio::test]
    async fn a_setting_is_checked_against_the_choices_the_account_published() {
        let mut zai = key_account("zai");
        zai.options = vec![tidemark_types::ProviderOption {
            name: "region".into(),
            title: "Region".into(),
            description: None,
            value: "global".into(),
            choices: vec![tidemark_types::OptionChoice {
                value: "global".into(),
                title: "Global".into(),
            }],
        }];
        let (daemon, _secrets, _commands) = daemon_over(vec![zai]).await;

        assert!(
            daemon
                .set_option("zai", "default", "region", "mars")
                .await
                .is_err(),
            "a value no build knows how to read must not reach config.toml"
        );
        assert!(
            daemon
                .set_option("zai", "default", "colour", "green")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_valid_setting_is_serialized_through_the_engine() {
        let mut zai = key_account("zai");
        zai.options = vec![tidemark_types::ProviderOption {
            name: "region".into(),
            title: "Region".into(),
            description: None,
            value: "global".into(),
            choices: vec![
                tidemark_types::OptionChoice {
                    value: "global".into(),
                    title: "Global".into(),
                },
                tidemark_types::OptionChoice {
                    value: "bigmodel-cn".into(),
                    title: "Mainland China".into(),
                },
            ],
        }];
        let (daemon, _secrets, mut commands) = daemon_over(vec![zai]).await;
        let daemon = Arc::new(daemon);
        let setting = tokio::spawn({
            let daemon = Arc::clone(&daemon);
            async move {
                daemon
                    .set_option("zai", "default", "region", "bigmodel-cn")
                    .await
            }
        });

        let Command::SetOption {
            provider,
            account,
            name,
            value,
            reply,
        } = commands.recv().await.expect("setting reaches engine")
        else {
            panic!("unexpected command");
        };
        assert_eq!(
            (provider, account, name, value),
            (
                "zai".to_owned(),
                "default".to_owned(),
                "region".to_owned(),
                "bigmodel-cn".to_owned()
            )
        );
        assert!(!setting.is_finished(), "D-Bus waits for persistence");
        reply.send(Ok(())).expect("caller waits for reply");
        setting
            .await
            .expect("setting task did not panic")
            .expect("engine accepted setting");
    }

    #[tokio::test]
    async fn awaiting_a_login_nobody_started_says_so() {
        let (daemon, _secrets, _commands) = daemon_over(vec![oauth_account("claude")]).await;
        assert!(daemon.await_login("claude", "default").await.is_err());
        // Cancelling one is not an error, so a dialog closing does not have to know.
        assert!(daemon.cancel_login("claude", "default").await.is_ok());
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
        let daemon = Daemon::new(
            published,
            Vec::new(),
            vec![("zai".into(), "default".into())],
            tx,
            Arc::new(FakeSecrets::default()),
        );

        assert!(
            daemon.refresh("codex").await.is_err(),
            "a typo must be visible"
        );
        assert!(daemon.refresh("zai").await.is_ok());
        assert!(matches!(
            rx.try_recv().expect("the loop was told"),
            Command::Refresh(Some(provider)) if provider == "zai"
        ));

        assert!(
            daemon.refresh("").await.is_ok(),
            "an empty slug means everything"
        );
        assert!(matches!(
            rx.try_recv().expect("the loop was told"),
            Command::Refresh(None)
        ));
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

        let Ok(server) = serve(Daemon::new(
            published,
            Vec::new(),
            vec![("zai".into(), "default".into())],
            commands,
            Arc::new(FakeSecrets::default()),
        ))
        .await
        else {
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
        assert!(
            matches!(
                command_queue.recv().await,
                Some(Command::Refresh(Some(provider))) if provider == "zai"
            ),
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

        let rule = MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .interface(ids::DAEMON_INTERFACE)
            .expect("a valid interface name")
            .member("ProviderRemoved")
            .expect("a valid member name")
            .build();
        let removals = MessageStream::for_match_rule(rule, &client, Some(4))
            .await
            .expect("the bus accepts the match rule");
        let mut removals = pin!(removals);

        Daemon::provider_removed(&emitter, "zai", "default")
            .await
            .expect("the removal signal goes out");
        let next = std::future::poll_fn(|cx| removals.as_mut().poll_next(cx));
        let received = tokio::time::timeout(Duration::from_secs(5), next)
            .await
            .expect("the signal arrives")
            .expect("the stream is alive")
            .expect("the message is well formed");
        let carried: (String, String) = received
            .body()
            .deserialize()
            .expect("a client parses the provider/account pair");
        assert_eq!(carried, ("zai".into(), "default".into()));
    }

    #[tokio::test]
    async fn a_notification_switch_is_serialized_through_the_engine() {
        let mut zai = key_account("zai");
        zai.windows = vec![tidemark_types::WindowStatus {
            key: "w18000".into(),
            title: "5 hours".into(),
            subtitle: None,
            used_percent: 42.0,
            resets_at: None,
            length_secs: Some(18_000),
        }];
        let (daemon, _secrets, mut commands) = daemon_over(vec![zai]).await;
        let daemon = Arc::new(daemon);
        let switching = tokio::spawn({
            let daemon = Arc::clone(&daemon);
            async move {
                daemon
                    .set_window_notify("zai", "default", "w18000", true)
                    .await
            }
        });

        let Command::SetWindowNotify {
            provider,
            account,
            window,
            enabled,
            reply,
        } = commands.recv().await.expect("switch reaches engine")
        else {
            panic!("unexpected command");
        };
        assert_eq!(
            (provider, account, window, enabled),
            (
                "zai".to_owned(),
                "default".to_owned(),
                "w18000".to_owned(),
                true
            )
        );
        assert!(!switching.is_finished(), "D-Bus waits for persistence");
        reply.send(Ok(())).expect("caller waits for reply");
        switching
            .await
            .expect("switch task did not panic")
            .expect("engine accepted the switch");
    }

    #[tokio::test]
    async fn a_notification_switch_for_an_unconfigured_account_is_an_error() {
        let (daemon, _secrets, _commands) = daemon_over(vec![key_account("zai")]).await;
        assert!(
            daemon
                .set_window_notify("codex", "default", "w18000", true)
                .await
                .is_err()
        );
    }
}
