//! The daemon's own connection to the Secret Service.
//!
//! Connecting is deferred and retried rather than done once at startup, because the daemon
//! is ordered after `graphical-session.target` and that says nothing about whether
//! `org.freedesktop.secrets` is up yet, let alone unlocked. A daemon that connected once,
//! failed, and gave up would report `keyring-unavailable` for the rest of the session over
//! a race it lost by half a second.

use std::collections::HashMap;
use std::sync::Arc;
use tidemark_core::providers::{BoxFuture, Credential};
use tidemark_core::secrets::{Kind, SecretError, Secrets, Store};
use tidemark_types::{AccountId, ProviderId};
use tokio::sync::Mutex;

type SecretSlot = (Kind, String, String);
type MutationMap = std::sync::Mutex<HashMap<SecretSlot, Arc<Mutex<()>>>>;

/// A [`Store`] that connects on first use and reconnects after the bus drops it.
#[derive(Debug, Default)]
pub struct Keyring {
    store: Mutex<Option<Arc<Store>>>,
    mutations: MutationMap,
}

impl Keyring {
    fn mutation(&self, kind: Kind, provider: &ProviderId, account: &AccountId) -> Arc<Mutex<()>> {
        Arc::clone(
            self.mutations
                .lock()
                .expect("no code panics while holding the credential mutation map")
                .entry((
                    kind,
                    provider.as_str().to_owned(),
                    account.as_str().to_owned(),
                ))
                .or_insert_with(|| Arc::new(Mutex::new(()))),
        )
    }

    /// Runs one operation against a connected store, connecting first if needed.
    ///
    /// Written as a combinator because the reconnect rule is the interesting part: a D-Bus
    /// failure drops the held connection so the *next* call builds a new one, and a call
    /// that merely found nothing does not.
    async fn with_store<T>(
        &self,
        operation: impl AsyncFnOnce(&Store) -> Result<T, SecretError>,
    ) -> Result<T, SecretError> {
        let store = {
            let mut held = self.store.lock().await;
            if held.is_none() {
                *held = Some(Arc::new(Store::connect().await?));
                tracing::debug!("connected to the secret service");
            }
            Arc::clone(held.as_ref().expect("connected just above"))
        };

        let result = operation(&store).await;
        if matches!(result, Err(SecretError::Dbus(_))) {
            // The connection is the likeliest casualty; the next call builds a new one
            // rather than reusing a socket the bus has already forgotten about.
            let mut held = self.store.lock().await;
            if held
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &store))
            {
                *held = None;
            }
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
        Box::pin(async move {
            let mutation = self.mutation(kind, provider, account);
            let _guard = mutation.lock().await;
            self.with_store(async move |store| store.set(kind, provider, account, secret).await)
                .await
        })
    }

    fn compare_and_set<'a>(
        &'a self,
        kind: Kind,
        provider: &'a ProviderId,
        account: &'a AccountId,
        expected: &'a Credential,
        replacement: &'a Credential,
    ) -> BoxFuture<'a, Result<bool, SecretError>> {
        Box::pin(async move {
            let mutation = self.mutation(kind, provider, account);
            let _guard = mutation.lock().await;
            self.with_store(async move |store| {
                store
                    .compare_and_set(kind, provider, account, expected, replacement)
                    .await
            })
            .await
        })
    }

    fn delete<'a>(
        &'a self,
        kind: Kind,
        provider: &'a ProviderId,
        account: &'a AccountId,
    ) -> BoxFuture<'a, Result<(), SecretError>> {
        Box::pin(async move {
            let mutation = self.mutation(kind, provider, account);
            let _guard = mutation.lock().await;
            self.with_store(async move |store| store.delete(kind, provider, account).await)
                .await
        })
    }
}
