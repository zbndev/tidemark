//! The daemon's own connection to the Secret Service.
//!
//! Connecting is deferred and retried rather than done once at startup, because the daemon
//! is ordered after `graphical-session.target` and that says nothing about whether
//! `org.freedesktop.secrets` is up yet, let alone unlocked. A daemon that connected once,
//! failed, and gave up would report `keyring-unavailable` for the rest of the session over
//! a race it lost by half a second.

use tidemark_core::providers::{BoxFuture, Credential};
use tidemark_core::secrets::{KeySource, SecretError, Store};
use tidemark_types::{AccountId, ProviderId};
use tokio::sync::Mutex;

/// A [`Store`] that connects on first use and reconnects after the bus drops it.
#[derive(Debug, Default)]
pub struct Keyring {
    store: Mutex<Option<Store>>,
}

impl KeySource for Keyring {
    fn provider_key<'a>(
        &'a self,
        provider: &'a ProviderId,
        account: &'a AccountId,
    ) -> BoxFuture<'a, Result<Option<Credential>, SecretError>> {
        Box::pin(async move {
            let mut held = self.store.lock().await;
            if held.is_none() {
                *held = Some(Store::connect().await?);
                tracing::debug!("connected to the secret service");
            }

            let store = held.as_ref().expect("connected just above");
            let result = store.provider_key(provider, account).await;
            if matches!(result, Err(SecretError::Dbus(_))) {
                // The connection is the likeliest casualty; the next poll builds a new one
                // rather than reusing a socket the bus has already forgotten about.
                *held = None;
            }
            result
        })
    }
}
