//! Secret Service access for keys Tidemark owns.
//!
//! Z.ai and Kimi are plain API keys the user types in; those go here, under
//! `io.github.zbndev.Tidemark.ProviderKey`. Claude and Codex's OAuth tokens live in files
//! owned by their own CLIs and are read/refreshed/written back per ADR 0001 — a different
//! path, not this one.
//!
//! # The trap this module exists to avoid
//!
//! The Secret Service can be locked when the daemon starts — `graphical-session.target`
//! does not imply an unlocked keyring, and a daemon restart can land before the user has
//! unlocked anything. Every operation on [`Store`] goes through `unlocked_collection`,
//! which checks [`oo7::dbus::Collection::is_locked`] *before* touching the collection and
//! returns [`SecretError::Locked`] rather than calling into `create_item`/`search_items`,
//! which on a locked collection would drive a Secret Service prompt — a graphical unlock
//! dialog popping up from an unattended background process. Locked is a state for the
//! caller to wait out, not an error to log and not a crash.

use oo7::dbus::{Collection, Service};
use tidemark_types::{AccountId, ProviderId};

use crate::providers::Credential;

/// The Secret Service schema every key Tidemark stores is filed under, matching
/// `CONTEXT.md` § Identity. `xdg:schema` is the attribute name `libsecret` and other
/// Secret Service clients use by convention to carry it — there is no dedicated field in
/// the D-Bus API.
pub const SCHEMA: &str = "io.github.zbndev.Tidemark.ProviderKey";

const ATTR_SCHEMA: &str = "xdg:schema";
const ATTR_PROVIDER: &str = "provider";
const ATTR_ACCOUNT: &str = "account";

/// Why a Secret Service operation did not produce a value.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The collection is locked. Not a failure — see the module docs. The caller should
    /// surface a "waiting for keyring" state and try again later rather than treat this as
    /// a credential problem or a crash.
    #[error("the keyring is locked")]
    Locked,
    /// The stored secret is not valid UTF-8. Nothing we write can produce this; it means
    /// something else wrote to our schema.
    #[error("stored secret is not valid UTF-8")]
    NotUtf8,
    /// Talking to `org.freedesktop.secrets` failed outright: no provider is running, the
    /// bus is unreachable, or the call errored.
    #[error("secret service unavailable: {0}")]
    Dbus(#[from] oo7::dbus::Error),
}

fn attributes(provider: &ProviderId, account: &AccountId) -> [(&'static str, String); 3] {
    [
        (ATTR_SCHEMA, SCHEMA.to_owned()),
        (ATTR_PROVIDER, provider.as_str().to_owned()),
        (ATTR_ACCOUNT, account.as_str().to_owned()),
    ]
}

/// A connection to the freedesktop Secret Service, scoped to Tidemark's own keys.
#[derive(Debug)]
pub struct Store {
    service: Service,
}

impl Store {
    /// Connects to `org.freedesktop.secrets`. Cheap, and safe to call again if a prior
    /// connection was lost — it does not itself touch a collection, so it never blocks on
    /// an unlock prompt.
    pub async fn connect() -> Result<Self, SecretError> {
        let service = Service::new().await?;
        Ok(Self { service })
    }

    /// The default collection, checked for the locked state described in the module docs
    /// before it is handed back — every operation below goes through here so none of them
    /// can accidentally skip the check.
    async fn unlocked_collection(&self) -> Result<Collection, SecretError> {
        let collection = self.service.default_collection().await?;
        if collection.is_locked().await? {
            return Err(SecretError::Locked);
        }
        Ok(collection)
    }

    /// The credential stored for `(provider, account)`, if any.
    ///
    /// Returns [`SecretError::Locked`] without attempting the search when the collection
    /// is locked, so a caller can distinguish "waiting for keyring" from "no key saved
    /// yet" — the latter is `Ok(None)`.
    pub async fn provider_key(
        &self,
        provider: &ProviderId,
        account: &AccountId,
    ) -> Result<Option<Credential>, SecretError> {
        let collection = self.unlocked_collection().await?;
        let attrs = attributes(provider, account);
        let items = collection.search_items(&attrs).await?;
        let Some(item) = items.into_iter().next() else {
            return Ok(None);
        };

        let secret = item.secret().await?;
        let secret = String::from_utf8(secret.to_vec()).map_err(|_| SecretError::NotUtf8)?;
        Ok(Some(Credential::new(secret)))
    }

    /// Stores (or replaces) the credential for `(provider, account)`.
    ///
    /// Returns [`SecretError::Locked`] without attempting the write when the collection is
    /// locked, for the same reason as [`Store::provider_key`].
    pub async fn set_provider_key(
        &self,
        provider: &ProviderId,
        account: &AccountId,
        credential: &Credential,
    ) -> Result<(), SecretError> {
        let collection = self.unlocked_collection().await?;
        let attrs = attributes(provider, account);
        let label = format!("Tidemark: {} ({})", provider.as_str(), account.as_str());
        collection
            .create_item(&label, &attrs, credential.expose(), true, None)
            .await?;
        Ok(())
    }

    /// Removes the credential for `(provider, account)`, if one is stored. Removing a key
    /// that is not there is not an error.
    pub async fn delete_provider_key(
        &self,
        provider: &ProviderId,
        account: &AccountId,
    ) -> Result<(), SecretError> {
        let collection = self.unlocked_collection().await?;
        let attrs = attributes(provider, account);
        for item in collection.search_items(&attrs).await? {
            item.delete(None).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! These tests talk to a real `org.freedesktop.secrets` implementation over the
    //! session bus — there is no mock in the loop, on the same principle as the corpus
    //! replay test: the traps here (schema attributes, UTF-8, the locked check) are real
    //! Secret Service behaviour, not something a fake would reproduce faithfully. They are
    //! skipped, not failed, when no session bus is reachable (headless CI), so a public
    //! checkout does not need a keyring to build.

    use super::*;

    async fn connect_or_skip() -> Option<Store> {
        match Store::connect().await {
            Ok(store) => Some(store),
            Err(err) => {
                eprintln!("skipped: no secret service reachable ({err})");
                None
            }
        }
    }

    #[tokio::test]
    async fn a_stored_key_survives_being_read_back() {
        let Some(store) = connect_or_skip().await else {
            return;
        };
        let provider = ProviderId::new("tidemark-test-zai");
        let account = AccountId::default();
        let credential = Credential::new("sk-test-only-not-a-real-key");

        store
            .set_provider_key(&provider, &account, &credential)
            .await
            .expect("keyring is unlocked in the test session");

        let read_back = store
            .provider_key(&provider, &account)
            .await
            .expect("read after write")
            .expect("the key we just stored");
        assert_eq!(read_back.expose(), credential.expose());

        store
            .delete_provider_key(&provider, &account)
            .await
            .expect("cleanup");
        assert!(
            store
                .provider_key(&provider, &account)
                .await
                .expect("read after delete")
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_key_never_stored_reads_back_as_none_not_an_error() {
        let Some(store) = connect_or_skip().await else {
            return;
        };
        let provider = ProviderId::new("tidemark-test-nonexistent");
        let account = AccountId::default();

        let result = store
            .provider_key(&provider, &account)
            .await
            .expect("no entry is Ok(None), not an error");
        assert!(result.is_none());
    }
}
