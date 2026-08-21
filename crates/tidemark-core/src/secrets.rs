//! Secret Service access for the credentials Tidemark owns.
//!
//! Two kinds, under two schemas, and the split is not cosmetic.
//!
//! * **Keys** — Z.ai and Kimi are plain API keys the user types in, filed under
//!   `io.github.zbndev.Tidemark.ProviderKey`.
//! * **Tokens** — the OAuth credential a Tidemark login obtained for itself, filed under
//!   `io.github.zbndev.Tidemark.ProviderToken`. Claude and Codex normally read the token
//!   files owned by their own CLIs and refresh them in place per ADR 0001; a token here is
//!   the other case, an account the user signed into *from Tidemark*, which is stored here
//!   precisely so that no vendor credential file has to be created or replaced to hold it.
//!
//! The two schemas are separate so that a lookup for one can never return the other. They
//! could not have been one schema with an extra attribute: `search_items` matches on the
//! whole attribute set, so adding a discriminator to the key schema would have made every
//! key already in a user's keyring invisible.
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

use crate::providers::{BoxFuture, Credential};

/// The Secret Service schema every key Tidemark stores is filed under, matching
/// `CONTEXT.md` § Identity. `xdg:schema` is the attribute name `libsecret` and other
/// Secret Service clients use by convention to carry it — there is no dedicated field in
/// the D-Bus API.
pub const SCHEMA: &str = "io.github.zbndev.Tidemark.ProviderKey";

/// The Secret Service schema a Tidemark-owned OAuth credential is filed under. Separate
/// from [`SCHEMA`] for the reason in the module docs.
pub const TOKEN_SCHEMA: &str = "io.github.zbndev.Tidemark.ProviderToken";

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

/// Which of the two things stored under `(provider, account)` a call means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An API key the user pasted in.
    Key,
    /// An OAuth credential a Tidemark login obtained, as the provider's own document.
    Token,
}

impl Kind {
    /// The schema this kind is filed under.
    pub const fn schema(self) -> &'static str {
        match self {
            Self::Key => SCHEMA,
            Self::Token => TOKEN_SCHEMA,
        }
    }

    const fn noun(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Token => "login",
        }
    }
}

fn attributes(
    kind: Kind,
    provider: &ProviderId,
    account: &AccountId,
) -> [(&'static str, String); 3] {
    [
        (ATTR_SCHEMA, kind.schema().to_owned()),
        (ATTR_PROVIDER, provider.as_str().to_owned()),
        (ATTR_ACCOUNT, account.as_str().to_owned()),
    ]
}

/// Where the daemon reads and writes the credentials Tidemark owns.
///
/// The trait exists for one reason: the state this module is built around — a locked
/// keyring — is the one state that cannot responsibly be produced on a developer's
/// machine. Locking the real login collection mid-session to see what the scheduler does
/// would throw an unlock prompt at every other application holding a secret. Behind this
/// seam the daemon's handling of [`SecretError::Locked`] is exercised for real, with a
/// source that simply says it is locked.
pub trait Secrets: std::fmt::Debug + Send + Sync {
    /// The secret of this kind stored for `(provider, account)`, if any. `Ok(None)` means
    /// nothing has been saved yet, which is a different thing from the keyring being
    /// locked.
    fn get<'a>(
        &'a self,
        kind: Kind,
        provider: &'a ProviderId,
        account: &'a AccountId,
    ) -> BoxFuture<'a, Result<Option<Credential>, SecretError>>;

    /// Stores, or replaces, the secret of this kind.
    fn set<'a>(
        &'a self,
        kind: Kind,
        provider: &'a ProviderId,
        account: &'a AccountId,
        secret: &'a Credential,
    ) -> BoxFuture<'a, Result<(), SecretError>>;

    /// Removes the secret of this kind. Removing one that is not there is not an error.
    fn delete<'a>(
        &'a self,
        kind: Kind,
        provider: &'a ProviderId,
        account: &'a AccountId,
    ) -> BoxFuture<'a, Result<(), SecretError>>;
}

impl Secrets for Store {
    fn get<'a>(
        &'a self,
        kind: Kind,
        provider: &'a ProviderId,
        account: &'a AccountId,
    ) -> BoxFuture<'a, Result<Option<Credential>, SecretError>> {
        Box::pin(Store::get(self, kind, provider, account))
    }

    fn set<'a>(
        &'a self,
        kind: Kind,
        provider: &'a ProviderId,
        account: &'a AccountId,
        secret: &'a Credential,
    ) -> BoxFuture<'a, Result<(), SecretError>> {
        Box::pin(Store::set(self, kind, provider, account, secret))
    }

    fn delete<'a>(
        &'a self,
        kind: Kind,
        provider: &'a ProviderId,
        account: &'a AccountId,
    ) -> BoxFuture<'a, Result<(), SecretError>> {
        Box::pin(Store::delete(self, kind, provider, account))
    }
}

/// A connection to the freedesktop Secret Service, scoped to Tidemark's own secrets.
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

    /// The secret of this kind stored for `(provider, account)`, if any.
    ///
    /// Returns [`SecretError::Locked`] without attempting the search when the collection
    /// is locked, so a caller can distinguish "waiting for keyring" from "nothing saved
    /// yet" — the latter is `Ok(None)`.
    pub async fn get(
        &self,
        kind: Kind,
        provider: &ProviderId,
        account: &AccountId,
    ) -> Result<Option<Credential>, SecretError> {
        let collection = self.unlocked_collection().await?;
        let attrs = attributes(kind, provider, account);
        let items = collection.search_items(&attrs).await?;
        let Some(item) = items.into_iter().next() else {
            return Ok(None);
        };

        let secret = item.secret().await?;
        let secret = String::from_utf8(secret.to_vec()).map_err(|_| SecretError::NotUtf8)?;
        Ok(Some(Credential::new(secret)))
    }

    /// Stores (or replaces) the secret of this kind for `(provider, account)`.
    ///
    /// Returns [`SecretError::Locked`] without attempting the write when the collection is
    /// locked, for the same reason as [`Store::get`].
    pub async fn set(
        &self,
        kind: Kind,
        provider: &ProviderId,
        account: &AccountId,
        secret: &Credential,
    ) -> Result<(), SecretError> {
        let collection = self.unlocked_collection().await?;
        let attrs = attributes(kind, provider, account);
        // What a keyring manager shows the user in a list of stored secrets. It names the
        // account rather than describing the bytes, because "Tidemark: claude (default)"
        // is what someone deleting things by hand needs to recognise.
        let label = format!(
            "Tidemark {}: {} ({})",
            kind.noun(),
            provider.as_str(),
            account.as_str()
        );
        collection
            .create_item(&label, &attrs, secret.expose(), true, None)
            .await?;
        Ok(())
    }

    /// Removes the secret of this kind, if one is stored. Removing one that is not there
    /// is not an error.
    pub async fn delete(
        &self,
        kind: Kind,
        provider: &ProviderId,
        account: &AccountId,
    ) -> Result<(), SecretError> {
        let collection = self.unlocked_collection().await?;
        let attrs = attributes(kind, provider, account);
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
            .set(Kind::Key, &provider, &account, &credential)
            .await
            .expect("keyring is unlocked in the test session");

        let read_back = store
            .get(Kind::Key, &provider, &account)
            .await
            .expect("read after write")
            .expect("the key we just stored");
        assert_eq!(read_back.expose(), credential.expose());

        store
            .delete(Kind::Key, &provider, &account)
            .await
            .expect("cleanup");
        assert!(
            store
                .get(Kind::Key, &provider, &account)
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
            .get(Kind::Key, &provider, &account)
            .await
            .expect("no entry is Ok(None), not an error");
        assert!(result.is_none());
    }

    /// The whole reason the two schemas exist. A key and a login for the same account are
    /// two different secrets, and a lookup for one that returned the other would hand a
    /// provider a JSON document where it expected a bearer token.
    #[tokio::test]
    async fn a_key_and_a_login_for_one_account_do_not_see_each_other() {
        let Some(store) = connect_or_skip().await else {
            return;
        };
        let provider = ProviderId::new("tidemark-test-both");
        let account = AccountId::default();

        store
            .set(Kind::Key, &provider, &account, &Credential::new("a-key"))
            .await
            .expect("keyring is unlocked in the test session");
        store
            .set(
                Kind::Token,
                &provider,
                &account,
                &Credential::new(r#"{"tokens":{}}"#),
            )
            .await
            .expect("stores");

        let key = store
            .get(Kind::Key, &provider, &account)
            .await
            .expect("read")
            .expect("present");
        let token = store
            .get(Kind::Token, &provider, &account)
            .await
            .expect("read")
            .expect("present");
        assert_eq!(key.expose(), "a-key");
        assert_eq!(token.expose(), r#"{"tokens":{}}"#);

        // And deleting one leaves the other standing: signing out of a Tidemark login must
        // not take a pasted API key with it.
        store
            .delete(Kind::Token, &provider, &account)
            .await
            .expect("cleanup");
        assert!(
            store
                .get(Kind::Key, &provider, &account)
                .await
                .expect("read")
                .is_some()
        );
        store
            .delete(Kind::Key, &provider, &account)
            .await
            .expect("cleanup");
    }
}
