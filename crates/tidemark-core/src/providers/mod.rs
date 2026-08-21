//! Provider clients: one module per service, behind a common trait.
//!
//! # The contract, and the convention underneath it
//!
//! The trait is deliberately thin — a provider knows its own identity and can produce a
//! [`Snapshot`]. It says nothing about credentials, because the five providers acquire
//! them in five different ways: a user-supplied key in the Secret Service, an OAuth token
//! refreshed out of a third-party CLI's credential file, or a local server that holds its
//! own session in the keyring. Anything the trait tried to say about credentials would be
//! true of at most two implementations.
//!
//! Every implementation follows one convention the trait cannot express: **transport and
//! meaning are separate functions.** `fetch` performs the request and hands the response
//! body to a pure `parse(body, captured_at)`. That split is what makes the traps testable
//! — the unit table, the millisecond timestamps, Kimi's numbers-as-strings, Antigravity's
//! remaining-instead-of-used — none of which need a live key to get wrong.
//!
//! # What a provider must not do
//!
//! Silently drop a window. A window missing from the interface reads as "you have no such
//! limit", which is the most dangerous thing this program can say. An entry of a kind we
//! recognize but cannot parse is a [`ProviderError::Malformed`] for the whole fetch. Only
//! an entry of an *unrecognized* kind is skipped, because that is a quota type that did
//! not exist when this was written, not a failure to understand one that did.

pub mod antigravity;
pub mod claude;
pub mod codex;
pub mod http;
pub mod kimi;
pub mod zai;

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tidemark_types::{AccountId, ProviderId, Snapshot, WindowLength};

/// A future returned from a trait method.
///
/// `async fn` in traits does not yet work behind `dyn`, and the daemon holds its providers
/// as a heterogeneous list. Boxing one future per poll costs nothing at a five-minute
/// interval, and it keeps the trait free of a proc-macro dependency.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One service Tidemark can ask about quota.
pub trait Provider: fmt::Debug + Send + Sync {
    /// The stable slug this provider's history is filed under.
    fn id(&self) -> ProviderId;

    /// Which set of credentials this instance speaks for. v1 has one per provider.
    fn account(&self) -> AccountId {
        AccountId::default()
    }

    /// Fetch current quota.
    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>>;
}

/// What to call a window of this length, in the plainest terms that divide evenly.
///
/// Shared because a window's span is the provider-neutral half of its title: every
/// adapter that derives a length rather than being handed a name for it needs the same
/// sentence, and two of them spelling five hours differently would read as two different
/// limits on two cards.
pub(crate) fn length_title(length: WindowLength) -> String {
    let seconds = length.as_secs();
    for (unit, noun) in [(86_400, "day"), (3_600, "hour"), (60, "minute")] {
        if seconds.is_multiple_of(unit) {
            let count = seconds / unit;
            let plural = if count == 1 { "" } else { "s" };
            return format!("{count} {noun}{plural}");
        }
    }
    format!("{seconds} seconds")
}

/// A provider's own enum value, in words a person reads.
///
/// Providers name plans, tiers and regions in spellings meant for their own code — `plus`,
/// `pro_plus`, `LEVEL_INTERMEDIATE`, `REGION_OVERSEA`. Nothing is translated here, only
/// re-cased: a name we do not recognise is still the provider's own word for it, which is
/// what the card is supposed to be showing.
///
/// A word that shouts is quietened — `INTERMEDIATE` becomes `Intermediate` — but only when
/// every letter of it is uppercase, so a provider that capitalises deliberately keeps its
/// spelling instead of being flattened to `Gpt`.
pub(crate) fn title_case(raw: &str) -> String {
    raw.split(['_', '-', ' '])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let shouting = word.chars().all(|c| !c.is_lowercase());
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => {
                    let rest: String = chars.collect();
                    first.to_uppercase().collect::<String>()
                        + &if shouting { rest.to_lowercase() } else { rest }
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A secret used to authenticate to a provider.
///
/// The only reason this is a type and not a `String` is [`fmt::Debug`]: the workspace warns
/// on missing `Debug` impls, so a struct holding a bare key would grow a derived one and
/// print the key into the log the first time anything traced it.
#[derive(Clone, PartialEq, Eq)]
pub struct Credential(String);

impl Credential {
    /// Wraps a secret.
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    /// The secret, for putting on the wire. Nowhere else.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// True when the secret is empty or blank, which every provider should refuse before
    /// spending a request to be told the same thing.
    pub fn is_blank(&self) -> bool {
        self.0.trim().is_empty()
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Credential(<{} bytes redacted>)", self.0.len())
    }
}

/// Why a fetch did not produce a snapshot.
///
/// The variants exist to be acted on, not just printed: the scheduler backs off on
/// [`ProviderError::RateLimited`], the interface asks for a new key on
/// [`ProviderError::Credential`], and everything else is a transient failure to retry
/// normally. Which provider failed is not carried here — the caller made the call.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The HTTP client could not be constructed at all.
    #[error("could not build an HTTP client: {0}")]
    Client(#[source] reqwest::Error),
    /// The request never completed: DNS, connection, TLS, timeout.
    #[error("request failed: {0}")]
    Transport(#[source] reqwest::Error),
    /// No credential exists at the provider's canonical source yet.
    #[error("no credential is available")]
    NoCredential,
    /// The credential is in the Secret Service and the collection is locked. Waited out,
    /// not reported as the user's mistake — see `crate::secrets`.
    #[error("the keyring is locked")]
    KeyringLocked,
    /// The credential is in the Secret Service and nothing answered on the bus.
    #[error("the keyring is unavailable: {0}")]
    KeyringUnavailable(String),
    /// The credential is missing, expired or rejected.
    #[error("the credential was rejected (HTTP {status})")]
    Credential {
        /// Status that carried the rejection.
        status: u16,
    },
    /// The provider asked us to slow down. The wait it asked for, if any, is in
    /// `retry_after`; the scheduler reads it through [`ProviderError::retry_after`].
    #[error("rate limited by the provider")]
    RateLimited {
        /// Seconds the provider asked us to wait, if it said.
        retry_after: Option<u64>,
    },
    /// Any other unsuccessful status.
    #[error("provider answered HTTP {status}")]
    Http {
        /// The status.
        status: u16,
    },
    /// A local prerequisite such as a credential file could not be accessed safely.
    #[error("local provider state is unavailable: {0}")]
    Local(String),
    /// The response arrived but does not mean what we expect it to mean.
    #[error("unparseable response: {0}")]
    Malformed(String),
}

impl ProviderError {
    /// Shorthand for the malformed case.
    pub fn malformed(detail: impl Into<String>) -> Self {
        Self::Malformed(detail.into())
    }

    /// How long the provider asked us to wait, when it asked.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after } => retry_after.map(Duration::from_secs),
            _ => None,
        }
    }

    /// True when retrying with the same credential is pointless until the user acts.
    pub fn needs_user_action(&self) -> bool {
        matches!(self, Self::NoCredential | Self::Credential { .. })
    }

    /// The provider-neutral translation of a Secret Service failure.
    ///
    /// Lives here rather than in `crate::secrets` so that the two providers which may read
    /// their credential out of the keyring reach the same three states as the two which
    /// always do — a locked keyring must mean the same thing on every card.
    pub fn from_secret_error(error: crate::secrets::SecretError) -> Self {
        use crate::secrets::SecretError;
        match error {
            SecretError::Locked => Self::KeyringLocked,
            SecretError::NotUtf8 => Self::malformed("the stored credential is not text"),
            SecretError::Dbus(error) => Self::KeyringUnavailable(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_credential_never_prints_itself() {
        let rendered = format!("{:?}", Credential::new("sk-super-secret"));
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }

    #[test]
    fn a_blank_credential_is_recognised_before_it_is_spent() {
        assert!(Credential::new("   ").is_blank());
        assert!(!Credential::new("sk-1").is_blank());
    }

    #[test]
    fn only_user_fixable_credential_states_ask_the_user_for_anything() {
        assert!(ProviderError::NoCredential.needs_user_action());
        assert!(ProviderError::Credential { status: 401 }.needs_user_action());
        assert!(!ProviderError::Http { status: 503 }.needs_user_action());
        assert!(
            !ProviderError::RateLimited { retry_after: None }.needs_user_action(),
            "a 429 resolves itself; do not send the user to the settings dialog"
        );
    }

    #[test]
    fn backoff_reads_the_providers_own_answer() {
        let err = ProviderError::RateLimited {
            retry_after: Some(90),
        };
        assert_eq!(err.retry_after(), Some(Duration::from_secs(90)));
    }
}
