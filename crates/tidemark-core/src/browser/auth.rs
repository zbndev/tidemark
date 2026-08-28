//! Generic inspection of browser-cookie authentication sources.

use super::{CookieError, Query, SafeStorage, Store, header_for};
use std::future::Future;
use tidemark_types::{AuthCandidate, AuthCandidateState};

/// A browser source selected by its durable browser and optional profile identifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    /// Stable browser slug from [`super::Browser::slug`].
    pub browser: String,
    /// A profile directory name when the person chose one explicitly.
    pub profile: Option<String>,
}

impl Selection {
    /// The opaque stable path a client sends back to select this browser or profile.
    pub fn candidate_id(&self) -> String {
        match &self.profile {
            Some(profile) => format!("{}/{}", self.browser, profile),
            None => self.browser.clone(),
        }
    }
}

/// The outcome of a provider's proof request for a candidate cookie header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Validation {
    /// The provider accepted the source.
    Ready,
    /// The provider rejected the source's credential.
    Rejected,
    /// The proof request could not say whether the source works.
    Unreachable,
}

/// A selected store's cookie header, kept entirely inside core.
///
/// This deliberately has no derived `Debug`: an inspected header can carry a live session
/// and must never reach a log while the provider validates it.
pub struct CandidateCredential {
    selection: Selection,
    header: String,
}

impl std::fmt::Debug for CandidateCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CandidateCredential")
            .field("selection", &self.selection)
            .field("header", &"<redacted>")
            .finish()
    }
}

impl CandidateCredential {
    /// The selected source this header was read from.
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// The header only a provider's in-core validator may send.
    pub fn header(&self) -> &str {
        &self.header
    }
}

/// Inspects browser profiles in caller-provided scan order without exposing cookies.
///
/// A profile with no live matching cookie is missing. A locked Safe Storage keyring is
/// deliberately distinct from a missing source. Every other local read error and an
/// inconclusive proof request is temporarily unreachable, so the UI never paints a source
/// invalid without Cursor (or a future provider) having actually rejected it.
pub async fn inspect<F, Fut>(
    stores: Vec<Store>,
    query: &Query,
    request_url: &str,
    storage: &dyn SafeStorage,
    validate: F,
) -> Vec<AuthCandidate>
where
    F: Fn(CandidateCredential) -> Fut,
    Fut: Future<Output = Validation>,
{
    inspect_matching(stores, query, request_url, storage, |_| true, validate).await
}

/// Inspects browser profiles whose usable jars contain a live cookie name with `prefix`.
///
/// Providers with rotating session-cookie suffixes use this to avoid treating an unrelated
/// cookie jar as a selectable credential source.
pub async fn inspect_prefix<F, Fut>(
    stores: Vec<Store>,
    prefix: &str,
    query: &Query,
    request_url: &str,
    storage: &dyn SafeStorage,
    validate: F,
) -> Vec<AuthCandidate>
where
    F: Fn(CandidateCredential) -> Fut,
    Fut: Future<Output = Validation>,
{
    inspect_matching(
        stores,
        query,
        request_url,
        storage,
        |name| name.starts_with(prefix),
        validate,
    )
    .await
}

async fn inspect_matching<F, Fut, M>(
    stores: Vec<Store>,
    query: &Query,
    request_url: &str,
    storage: &dyn SafeStorage,
    matches: M,
    validate: F,
) -> Vec<AuthCandidate>
where
    F: Fn(CandidateCredential) -> Fut,
    Fut: Future<Output = Validation>,
    M: Fn(&str) -> bool,
{
    let now = tidemark_types::Timestamp::now();
    let mut browsers: Vec<(super::Browser, Vec<AuthCandidate>)> = Vec::new();

    for store in stores {
        let state = match store.cookies(query, storage).await {
            Ok(cookies) => {
                let live: Vec<_> = cookies
                    .into_iter()
                    .filter(|cookie| cookie.is_live(now))
                    .collect();
                let header = header_for(&live, request_url);
                if !live.iter().any(|cookie| matches(&cookie.name)) || header.is_empty() {
                    AuthCandidateState::Missing
                } else {
                    match validate(CandidateCredential {
                        selection: Selection {
                            browser: store.browser.slug.to_owned(),
                            profile: Some(store.profile.clone()),
                        },
                        header,
                    })
                    .await
                    {
                        Validation::Ready => AuthCandidateState::Ready,
                        Validation::Rejected => AuthCandidateState::Rejected,
                        Validation::Unreachable => AuthCandidateState::Unreachable,
                    }
                }
            }
            Err(CookieError::KeyringLocked) => AuthCandidateState::WaitingForKeyring,
            Err(_) => AuthCandidateState::Unreachable,
        };
        let child = AuthCandidate {
            id: Selection {
                browser: store.browser.slug.to_owned(),
                profile: Some(store.profile.clone()),
            }
            .candidate_id(),
            title: store.profile,
            subtitle: None,
            state: state.as_wire().to_owned(),
            children: Vec::new(),
        };

        match browsers.last_mut() {
            Some((browser, children)) if browser.slug == store.browser.slug => {
                children.push(child);
            }
            _ => browsers.push((store.browser, vec![child])),
        }
    }

    browsers
        .into_iter()
        .map(|(browser, children)| AuthCandidate {
            id: browser.slug.to_owned(),
            title: browser.title.to_owned(),
            subtitle: None,
            state: aggregate_state(&children).as_wire().to_owned(),
            children,
        })
        .collect()
}

fn aggregate_state(children: &[AuthCandidate]) -> AuthCandidateState {
    let states = children.iter().filter_map(AuthCandidate::state);
    if states
        .clone()
        .any(|state| state == AuthCandidateState::Ready)
    {
        return AuthCandidateState::Ready;
    }
    if states
        .clone()
        .any(|state| state == AuthCandidateState::WaitingForKeyring)
    {
        return AuthCandidateState::WaitingForKeyring;
    }
    if states
        .clone()
        .any(|state| state == AuthCandidateState::Unreachable)
    {
        return AuthCandidateState::Unreachable;
    }
    if states
        .clone()
        .any(|state| state == AuthCandidateState::Rejected)
    {
        return AuthCandidateState::Rejected;
    }
    AuthCandidateState::Missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{Query, SafeStorage, stores_in};
    use crate::secrets::SecretError;
    use rusqlite::Connection;
    use tidemark_types::AuthCandidateState;

    #[derive(Debug)]
    struct NoKeyring;

    impl SafeStorage for NoKeyring {
        fn password(
            &self,
            _application: &str,
        ) -> crate::providers::BoxFuture<'_, Result<Option<String>, SecretError>> {
            Box::pin(async { Ok(None) })
        }
    }

    fn gecko_profile(
        home: &crate::browser::tests::TestHome,
        profile: &str,
        value: &str,
        expiry: i64,
    ) {
        let path = home.gecko(profile);
        let connection = Connection::open(path).expect("opens");
        connection
            .execute_batch(
                "CREATE TABLE moz_cookies (
                    id INTEGER PRIMARY KEY,
                    baseDomain TEXT,
                    originAttributes TEXT NOT NULL DEFAULT '',
                    name TEXT, value TEXT, host TEXT, path TEXT,
                    expiry INTEGER, lastAccessed INTEGER, creationTime INTEGER,
                    isSecure INTEGER, isHttpOnly INTEGER
                );",
            )
            .expect("creates the table");
        connection
            .execute(
                "INSERT INTO moz_cookies (
                    host, name, value, path, expiry, isSecure, lastAccessed,
                    creationTime, isHttpOnly
                ) VALUES ('.cursor.com', 'session', ?1, '/', ?2, 1, 0, 0, 0)",
                (value, expiry),
            )
            .expect("inserts the session");
    }

    #[test]
    fn the_browser_profiles_are_nested_in_scan_order_with_their_validation_states() {
        // Replacing inspection with a first-match scan would lose the rejected and expired
        // accounts the person must distinguish before selecting their exact browser source.
        let home = crate::browser::tests::TestHome::new();
        gecko_profile(&home, ".mozilla/firefox/aa.Working", "works", 0);
        gecko_profile(&home, ".mozilla/firefox/zz.Rejected", "rejected", 0);
        gecko_profile(&home, ".zen/aa.Expired", "expired", 1_785_000_000);
        gecko_profile(&home, ".zen/zz.AlsoWorking", "also-works", 0);
        let query = Query::new(["cursor.com"], ["session"]);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");

        let report = runtime.block_on(inspect(
            stores_in(home.path()),
            &query,
            "https://cursor.com/api/usage-summary",
            &NoKeyring,
            |credential| async move {
                if credential.header().contains("rejected") {
                    Validation::Rejected
                } else {
                    Validation::Ready
                }
            },
        ));

        assert_eq!(report.len(), 2);
        assert_eq!(
            (report[0].id.as_str(), report[0].title.as_str()),
            ("firefox", "Firefox")
        );
        assert_eq!(
            report[0]
                .children
                .iter()
                .map(|candidate| (candidate.id.as_str(), candidate.state.as_str()))
                .collect::<Vec<_>>(),
            [
                ("firefox/aa.Working", AuthCandidateState::Ready.as_wire()),
                (
                    "firefox/zz.Rejected",
                    AuthCandidateState::Rejected.as_wire()
                ),
            ]
        );
        assert_eq!(
            (report[1].id.as_str(), report[1].title.as_str()),
            ("zen", "Zen")
        );
        assert_eq!(
            report[1]
                .children
                .iter()
                .map(|candidate| (candidate.id.as_str(), candidate.state.as_str()))
                .collect::<Vec<_>>(),
            [
                ("zen/aa.Expired", AuthCandidateState::Missing.as_wire()),
                ("zen/zz.AlsoWorking", AuthCandidateState::Ready.as_wire()),
            ]
        );
    }

    #[test]
    fn a_candidate_credential_never_prints_its_cookie_header() {
        // A derived Debug would leak the live session while an adapter is being inspected.
        let credential = CandidateCredential {
            selection: Selection {
                browser: "firefox".into(),
                profile: Some("default-release".into()),
            },
            header: "session=do-not-print-this".into(),
        };

        let rendered = format!("{credential:?}");

        assert!(rendered.contains("firefox"));
        assert!(!rendered.contains("do-not-print-this"));
        assert!(rendered.contains("redacted"));
    }
}
