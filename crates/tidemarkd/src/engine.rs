//! The poll loop: credentials in, snapshots out, history on the way past.
//!
//! One task owns every account and the database, which is why nothing here is behind a
//! mutex. Fetches run concurrently — there will be five providers and the slowest measured
//! one takes 2.7 s — but ingest and publication happen back in the owning task, in order.
//!
//! Finished statuses leave through a channel rather than being written to D-Bus here. That
//! is what keeps this module testable without a bus: the tests below run the real loop with
//! a fake provider and a fake keyring and read the same updates the daemon publishes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tidemark_core::providers::{Credential, Provider, ProviderError};
use tidemark_core::secrets::{KeySource, SecretError};
use tidemark_core::storage::History;
use tidemark_types::{
    AccountId, ProviderId, ProviderState, ProviderStatus, Snapshot, Timestamp, WindowStatus,
};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use crate::scheduler::{self, Situation};

/// How often the history is thinned. Thinning only touches points older than ninety days,
/// so there is nothing to gain from doing it more often than once a day.
pub const THIN_INTERVAL: Duration = Duration::from_secs(24 * 3600);

/// Consumption must move by more than this to count as movement. Percentages that arrive
/// as ratios multiplied out wobble in the last decimal place; a wobble is not a session.
const CHANGE_EPSILON: f64 = 0.01;

/// What the D-Bus interface asks the loop to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Poll now: one provider by slug, or every account when `None`.
    Refresh(Option<String>),
    /// Stop the loop.
    Shutdown,
}

/// Builds a provider client once a credential is in hand.
///
/// A closure rather than a trait because that is all it is: the five providers are
/// constructed five different ways, and only the daemon knows which one it is holding.
pub type Factory =
    Box<dyn Fn(Credential) -> Result<Arc<dyn Provider>, ProviderError> + Send + Sync>;

/// One account the daemon watches.
pub struct Account {
    provider: ProviderId,
    account: AccountId,
    factory: Option<Factory>,
    client: Option<Arc<dyn Provider>>,
    status: ProviderStatus,
    failures: u32,
    retry_after: Option<Duration>,
    last_change_at: Option<Timestamp>,
    due: Instant,
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
        Self {
            status: ProviderStatus::pending(&provider, &account),
            provider,
            account,
            factory: Some(factory),
            client: None,
            failures: 0,
            retry_after: None,
            last_change_at: None,
            due: Instant::now(),
        }
    }

    /// An account with its client already in hand.
    ///
    /// Test-only for now. It is also the shape a provider needing no stored key will take
    /// — Antigravity holds its own session in the `agy` CLI — so the step that adds one
    /// removes this gate rather than writing it again.
    #[cfg(test)]
    pub fn with_client(client: Arc<dyn Provider>) -> Self {
        let (provider, account) = (client.id(), client.account());
        Self {
            status: ProviderStatus::pending(&provider, &account),
            provider,
            account,
            factory: None,
            client: Some(client),
            failures: 0,
            retry_after: None,
            last_change_at: None,
            due: Instant::now(),
        }
    }

    /// The status as last published.
    #[cfg(test)]
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
    keys: Arc<dyn KeySource>,
    updates: mpsc::Sender<ProviderStatus>,
    last_thin: Option<Instant>,
}

impl Engine {
    /// Assembles the loop. Nothing is polled until [`Engine::run`].
    pub fn new(
        accounts: Vec<Account>,
        history: History,
        keys: Arc<dyn KeySource>,
        updates: mpsc::Sender<ProviderStatus>,
    ) -> Self {
        Self {
            accounts,
            history,
            keys,
            updates,
            last_thin: None,
        }
    }

    /// Publishes every account as pending.
    ///
    /// Called before the first poll so that a client connecting in the first second of the
    /// daemon's life gets the list of accounts with a state on each, rather than an empty
    /// array it cannot tell apart from "nothing is configured".
    pub async fn announce(&self) {
        for account in &self.accounts {
            let _ = self.updates.send(account.status.clone()).await;
        }
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
        if self.accounts[index].factory.is_none() {
            // No factory and no client: nothing could ever poll this account. Only
            // reachable if a caller built one that way.
            self.accounts[index].set_state(
                ProviderState::NoCredential,
                Some("no way to build a client for this account".into()),
            );
            return;
        }

        let keys = Arc::clone(&self.keys);
        let provider = self.accounts[index].provider.clone();
        let account = self.accounts[index].account.clone();
        let found = keys.provider_key(&provider, &account).await;

        // Resolved before the account is borrowed mutably, because building the client
        // reads the factory off the very account the outcome is written back to.
        let loaded = match found {
            Ok(Some(credential)) => {
                let factory = self.accounts[index]
                    .factory
                    .as_ref()
                    .expect("checked just above");
                match factory(credential) {
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
                self.record(index, &snapshot);
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
                if state == ProviderState::CredentialRejected {
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
    fn record(&mut self, index: usize, snapshot: &Snapshot) {
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
        let _ = self.updates.send(account.status.clone()).await;
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
        ProviderError::Credential { .. } => ProviderState::CredentialRejected,
        ProviderError::RateLimited { .. } => ProviderState::RateLimited,
        ProviderError::Malformed(_) => ProviderState::Malformed,
        ProviderError::Client(_) | ProviderError::Transport(_) | ProviderError::Http { .. } => {
            ProviderState::Unreachable
        }
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
    use std::sync::Mutex;
    use tidemark_core::providers::BoxFuture;
    use tidemark_types::{Window, WindowKey, WindowLength};

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

    impl KeySource for Keyring {
        fn provider_key<'a>(
            &'a self,
            _provider: &'a ProviderId,
            _account: &'a AccountId,
        ) -> BoxFuture<'a, Result<Option<Credential>, SecretError>> {
            let answer = (self.0)();
            Box::pin(async move { answer })
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
        updates: mpsc::Receiver<ProviderStatus>,
    }

    impl Harness {
        fn new(accounts: Vec<Account>, keys: Arc<dyn KeySource>) -> Self {
            let (tx, rx) = mpsc::channel(64);
            Self {
                engine: Engine::new(
                    accounts,
                    History::in_memory().expect("an in-memory database opens"),
                    keys,
                    tx,
                ),
                updates: rx,
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
            while let Ok(status) = self.updates.try_recv() {
                drained.push(status);
            }
            drained
        }
    }

    fn unlocked() -> Arc<dyn KeySource> {
        Arc::new(Keyring(|| Ok(Some(Credential::new("sk-test")))))
    }

    fn with_provider(provider: Arc<dyn Provider>) -> Harness {
        Harness::new(vec![Account::with_client(provider)], unlocked())
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
                Box::new(|_| Ok(Fake::new(vec![Ok(snapshot(1.0, 3600))]) as Arc<dyn Provider>)),
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
                Box::new(|_| Ok(Fake::new(vec![]) as Arc<dyn Provider>)),
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
                Box::new(|_| {
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
}
