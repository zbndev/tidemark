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
pub mod keyed;

pub use keyed::{kimi, zai};

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tidemark_types::{AccountId, AuthCandidate, ProviderId, Snapshot, Timestamp, WindowLength};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

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

    /// Which set of credentials this instance speaks for, when it has more than one.
    fn account(&self) -> AccountId {
        AccountId::default()
    }

    /// The source selected for this account, when the provider owns source selection.
    fn source(&self) -> Option<Source> {
        None
    }

    /// Fetch current quota.
    fn fetch(&self) -> BoxFuture<'_, Result<Snapshot, ProviderError>>;

    /// Inspects selectable local authentication sources without exposing their credentials.
    ///
    /// Most providers have no such choice. Browser-cookie providers override this so the
    /// daemon can discover and validate candidates through the same trait object it polls.
    fn inspect_auth_sources(&self) -> BoxFuture<'_, Result<Vec<AuthCandidate>, ProviderError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// Which of two credentials an account uses.
///
/// Claude, Codex and Antigravity each hold two: the vendor's own session — the CLI's
/// credential file, or the running `agy` server — and a login the user performed **from
/// Tidemark**, stored in the Secret Service. Two sources rather than a source and a
/// fallback, because neither subsumes the other: the vendor's session is the account the
/// user is actually working in, and Tidemark's login is the one that works on a machine
/// the vendor's tool is not installed on at all. Neither is the right default everywhere,
/// so the choice is the user's, and [`Source::Auto`] is what an account does until they
/// make one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Source {
    /// The provider's own rule: the stored login when there is one and the vendor's
    /// session otherwise — except Antigravity, which asks its local server first.
    #[default]
    Auto,
    /// Only the login Tidemark performed.
    OAuth,
    /// Only the vendor's own session: the CLI's credential file, or `agy`.
    Cli,
}

impl Source {
    /// The mode a stored setting names, defaulting to [`Source::Auto`].
    ///
    /// An unrecognised value is the default rather than an error: the settings file is
    /// hand-editable, and a typo should cost the default rather than the account.
    pub fn from_value(value: Option<&str>) -> Self {
        match value {
            Some(OAUTH_SOURCE) => Self::OAuth,
            Some(CLI_SOURCE) => Self::Cli,
            _ => Self::Auto,
        }
    }

    /// The stored spelling of the mode — what [`Source::from_value`] reads back.
    pub const fn as_value(self) -> &'static str {
        match self {
            Self::Auto => AUTO_SOURCE,
            Self::OAuth => OAUTH_SOURCE,
            Self::Cli => CLI_SOURCE,
        }
    }
}

/// The stored spelling of [`Source::Auto`].
pub const AUTO_SOURCE: &str = "auto";
/// The stored spelling of [`Source::OAuth`].
pub const OAUTH_SOURCE: &str = "oauth";
/// The stored spelling of [`Source::Cli`].
pub const CLI_SOURCE: &str = "cli";

/// Checks a response's status and reads its body, writing the whole exchange to the
/// raw-response log on the way through.
///
/// The providers that keep their own client — Claude, Codex, Antigravity — do these three
/// steps identically, and each of them is a place the debug log would otherwise be
/// forgotten. `keyed::request` is deliberately not this function: it redacts a query
/// string out of its own errors, which these four have none of.
pub(crate) async fn read_body(
    provider: &str,
    sent: crate::debug::Sent<'_>,
    response: reqwest::Response,
) -> Result<String, ProviderError> {
    use crate::debug::{Answer, Exchange, record};

    let status = response.status();
    let retry_after = http::retry_after_header(&response).map(str::to_owned);
    let note = |answer| {
        record(Exchange {
            provider,
            sent,
            answer,
        });
    };

    if let Err(error) = http::check(status, retry_after.as_deref()) {
        // Refused on its status: nothing has read the body, so the line says what came
        // back rather than pretending to a body it never held.
        note(Answer::Refused {
            status: status.as_u16(),
        });
        return Err(error);
    }
    let body = match response.text().await {
        Ok(body) => body,
        Err(error) => {
            let error = ProviderError::Transport(error);
            note(Answer::Failed {
                error: &error.to_string(),
            });
            return Err(error);
        }
    };
    note(Answer::Body {
        status: status.as_u16(),
        body: &body,
    });
    Ok(body)
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

/// Parses an RFC-3339 timestamp — the spelling most providers state a reset time in.
///
/// `None` when the string is not RFC-3339 or is not a plausible instant; the two are not
/// worth distinguishing because either way the caller drops the value or fails the fetch,
/// and it knows which. Lives here rather than on `Timestamp` so that `tidemark-types`
/// stays free of a parsing dependency; `time` is already what the adapters parse with.
pub(crate) fn parse_rfc3339(raw: &str) -> Option<Timestamp> {
    let parsed = OffsetDateTime::parse(raw, &Rfc3339).ok()?;
    Timestamp::from_unix(parsed.unix_timestamp()).ok()
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
    /// The provider's edge answered with a browser challenge this client does not perform.
    /// The string is the whole message: the refusal is the edge's, not this machine's.
    #[error("{0}")]
    Challenged(String),
    /// A local prerequisite such as a credential file could not be accessed safely.
    #[error("local provider state is unavailable: {0}")]
    Local(String),
    /// The browser-emulating transport a provider's edge demands failed. The string is
    /// the whole message: [`ProviderError::Transport`] it cannot be, because that variant
    /// carries a `reqwest` error and this stack is not `reqwest`.
    #[error("{0}")]
    Emulated(String),
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
    fn a_source_is_read_from_its_stored_spelling_and_an_unknown_one_is_auto() {
        assert_eq!(Source::from_value(Some("oauth")), Source::OAuth);
        assert_eq!(Source::from_value(Some("cli")), Source::Cli);
        assert_eq!(Source::from_value(Some("auto")), Source::Auto);
        // Hand-editable file: a typo costs the default, not a card that will not start.
        assert_eq!(Source::from_value(Some("nonsense")), Source::Auto);
        assert_eq!(Source::from_value(None), Source::Auto);
    }

    #[test]
    fn a_source_spells_itself_the_way_from_value_reads_it_back() {
        for (source, spelling) in [
            (Source::Auto, "auto"),
            (Source::OAuth, "oauth"),
            (Source::Cli, "cli"),
        ] {
            assert_eq!(source.as_value(), spelling);
            assert_eq!(Source::from_value(Some(source.as_value())), source);
        }
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

    #[test]
    fn rfc3339_reads_both_spellings_providers_send() {
        // Z.ai's millisecond epochs are not this shape; the twenty-six keyed ports state
        // their resets as ISO-8601, with or without an offset.
        assert_eq!(
            parse_rfc3339("2026-08-21T12:00:00Z").map(Timestamp::as_unix),
            Some(1_787_313_600)
        );
        assert_eq!(
            parse_rfc3339("2026-08-21T17:30:00+05:30").map(Timestamp::as_unix),
            Some(1_787_313_600)
        );
        assert_eq!(
            parse_rfc3339("2026-08-21T12:00:00.123Z").map(Timestamp::as_unix),
            Some(1_787_313_600),
            "fractional seconds do not move a whole-second timestamp"
        );
    }

    #[test]
    fn rfc3339_refuses_what_a_provider_might_send_instead() {
        assert_eq!(
            parse_rfc3339("1969-07-20T20:17:40Z"),
            None,
            "before the plausible range"
        );
        assert_eq!(
            parse_rfc3339("9999-01-01T00:00:00Z"),
            None,
            "beyond the plausible range"
        );
        assert_eq!(
            parse_rfc3339("1787313600"),
            None,
            "a bare epoch is not RFC-3339"
        );
        assert_eq!(parse_rfc3339(""), None);
    }
}
