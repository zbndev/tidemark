//! The daemon's own connection to the Secret Service.
//!
//! Connecting is deferred and retried rather than done once at startup, because the daemon
//! is ordered after `graphical-session.target` and that says nothing about whether
//! `org.freedesktop.secrets` is up yet, let alone unlocked. A daemon that connected once,
//! failed, and gave up would report `keyring-unavailable` for the rest of the session over
//! a race it lost by half a second.

use tidemark_core::providers::{BoxFuture, Credential};
use tidemark_core::secrets::{Kind, SecretError, Secrets, Store};
use tidemark_types::{AccountId, ProviderId};
use tokio::sync::Mutex;

/// A [`Store`] that connects on first use and reconnects after the bus drops it.
#[derive(Debug, Default)]
pub struct Keyring {
    store: Mutex<Option<Store>>,
}

impl Keyring {
    /// Runs one operation against a connected store, connecting first if needed.
    ///
    /// Written as a combinator rather than repeated three times because the reconnect rule
    /// is the interesting part: a D-Bus failure drops the held connection so the *next*
    /// call builds a new one, and a call that merely found nothing does not.
    async fn with_store<T>(
        &self,
        operation: impl AsyncFnOnce(&Store) -> Result<T, SecretError>,
    ) -> Result<T, SecretError> {
        let mut held = self.store.lock().await;
        if held.is_none() {
            *held = Some(Store::connect().await?);
            tracing::debug!("connected to the secret service");
        }

        let store = held.as_ref().expect("connected just above");
        let result = operation(store).await;
        if matches!(result, Err(SecretError::Dbus(_))) {
            // The connection is the likeliest casualty; the next call builds a new one
            // rather than reusing a socket the bus has already forgotten about.
            *held = None;
        }
        result
    }
}

impl Secrets for Keyring {
    fn get<'a>(
        &'a self,
        kind: Kind,
        provider: &'a ProviderId,
        account: &'a AccountId,
    ) -> BoxFuture<'a, Result<Option<Credential>, SecretError>> {
        Box::pin(self.with_store(async move |store| store.get(kind, provider, account).await))
    }

    fn set<'a>(
        &'a self,
        kind: Kind,
        provider: &'a ProviderId,
        account: &'a AccountId,
        secret: &'a Credential,
    ) -> BoxFuture<'a, Result<(), SecretError>> {
        Box::pin(
            self.with_store(async move |store| store.set(kind, provider, account, secret).await),
        )
    }

    fn delete<'a>(
        &'a self,
        kind: Kind,
        provider: &'a ProviderId,
        account: &'a AccountId,
    ) -> BoxFuture<'a, Result<(), SecretError>> {
        Box::pin(self.with_store(async move |store| store.delete(kind, provider, account).await))
    }
}
