//! The plumbing browser-session providers share: remembering one chosen profile, reading its
//! cookies, and inspecting every available profile without exposing a cookie to the UI.

use super::{OptionSchema, Options, ProviderError};
use crate::browser::{
    self, Query, SafeStorage,
    auth::{self, Selection},
};
use std::fmt;
use std::future::Future;
use std::path::Path;
use tidemark_types::{AuthCandidate, AuthCandidateState};

/// The selected browser's stable slug.
pub const AUTH_BROWSER: &str = "auth-browser";
/// The selected browser profile, when the person chose one explicitly.
pub const AUTH_PROFILE: &str = "auth-profile";
/// The browser mode that contains scanned browser and profile candidates on D-Bus.
pub const BROWSER_SOURCE: &str = "browser";

/// The browser options every browser-session provider publishes.
pub static OPTIONS: &[OptionSchema] = &[
    OptionSchema {
        name: AUTH_BROWSER,
        title: "Browser",
        description: None,
        default: "",
        choices: &[],
        required: false,
    },
    OptionSchema {
        name: AUTH_PROFILE,
        title: "Browser profile",
        description: None,
        default: "",
        choices: &[],
        required: false,
    },
];

/// The full header a browser would send, together with the cookie that made the jar usable.
pub struct Session {
    /// The full Cookie header for `url`, the same one a browser would send.
    pub header: String,
    /// Which of `session_names` was found — the gate that made this jar worth reading.
    pub session_name: String,
    /// That cookie's value, for providers whose API wants it as a bearer.
    pub session_value: String,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("header", &"<redacted>")
            .field("session_name", &self.session_name)
            .field("session_value", &"<redacted>")
            .finish()
    }
}

/// Reads the browser/profile selection stored in an account's options.
pub fn selection(options: &Options) -> Option<Selection> {
    let browser = options
        .get(AUTH_BROWSER)
        .map(String::as_str)
        .map(str::trim)
        .filter(|browser| !browser.is_empty())?;
    let profile = options
        .get(AUTH_PROFILE)
        .map(String::as_str)
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
        .map(str::to_owned);
    Some(Selection {
        browser: browser.to_owned(),
        profile,
    })
}

/// Stores exactly one browser/profile selection, removing a stale profile when it is absent.
pub fn store_selection(options: &mut Options, selection: &Selection) {
    options.insert(AUTH_BROWSER.into(), selection.browser.clone());
    match &selection.profile {
        Some(profile) => {
            options.insert(AUTH_PROFILE.into(), profile.clone());
        }
        None => {
            options.remove(AUTH_PROFILE);
        }
    }
}

fn stores(home: Option<&Path>) -> Vec<browser::Store> {
    home.map(browser::stores_in).unwrap_or_else(browser::stores)
}

/// Reads a session from exactly the selected store.
pub async fn session(
    home: Option<&Path>,
    storage: &dyn SafeStorage,
    selection: &Selection,
    session_names: &[&str],
    query: &Query,
    url: &str,
) -> Result<Option<Session>, ProviderError> {
    session_matching(
        home,
        storage,
        selection,
        |name| session_names.is_empty() || session_names.contains(&name),
        query,
        url,
    )
    .await
}

/// Reads a session whose cookie name *starts with* `prefix` — auth stacks that rotate the
/// session cookie's suffix (Ory) cannot be gated on an exact name.
pub async fn session_prefix(
    home: Option<&Path>,
    storage: &dyn SafeStorage,
    selection: &Selection,
    prefix: &str,
    query: &Query,
    url: &str,
) -> Result<Option<Session>, ProviderError> {
    session_matching(
        home,
        storage,
        selection,
        |name| name.starts_with(prefix),
        query,
        url,
    )
    .await
}

async fn session_matching<M>(
    home: Option<&Path>,
    storage: &dyn SafeStorage,
    selection: &Selection,
    matches: M,
    query: &Query,
    url: &str,
) -> Result<Option<Session>, ProviderError>
where
    M: Fn(&str) -> bool,
{
    let now = tidemark_types::Timestamp::now();
    let mut keyring_locked = false;

    for store in stores(home).into_iter().filter(|store| {
        store.browser.slug == selection.browser
            && selection
                .profile
                .as_ref()
                .is_none_or(|profile| profile == &store.profile)
    }) {
        let cookies = match store.cookies(query, storage).await {
            Ok(cookies) => cookies,
            Err(browser::CookieError::KeyringLocked) => {
                keyring_locked = true;
                continue;
            }
            Err(_) => continue,
        };
        let live: Vec<_> = cookies
            .into_iter()
            .filter(|cookie| cookie.is_live(now))
            .collect();
        // Scope first, then gate: the session value must be a cookie the request URL would
        // actually receive, or a same-named cookie from another host or path — another
        // account's — could travel as a bearer while the header carried the right one.
        let scoped = browser::scoped(&live, url);
        let Some(cookie) = scoped.iter().find(|cookie| matches(&cookie.name)) else {
            continue;
        };
        return Ok(Some(Session {
            header: browser::header_of(&scoped),
            session_name: cookie.name.clone(),
            session_value: cookie.value.clone(),
        }));
    }

    if keyring_locked {
        return Err(ProviderError::KeyringLocked);
    }
    Ok(None)
}

/// Inspects every local browser profile while leaving its session values inside core.
///
/// The `session_names` gate must be the same one [`session`] applies for the poll: an
/// inspection that accepts a jar the fetch would refuse advertises a source that fails
/// only after the person has selected it.
pub async fn inspect_sources<F, Fut>(
    home: Option<&Path>,
    storage: &dyn SafeStorage,
    session_names: &[&str],
    query: &Query,
    probe_url: &str,
    validate: F,
) -> Vec<AuthCandidate>
where
    F: Fn(auth::CandidateCredential) -> Fut,
    Fut: Future<Output = auth::Validation>,
{
    auth::inspect_named(
        stores(home),
        session_names,
        query,
        probe_url,
        storage,
        validate,
    )
    .await
}

/// Inspects browser profiles with a live cookie whose name starts with `prefix`.
pub async fn inspect_sources_prefix<F, Fut>(
    home: Option<&Path>,
    storage: &dyn SafeStorage,
    prefix: &str,
    query: &Query,
    probe_url: &str,
    validate: F,
) -> Vec<AuthCandidate>
where
    F: Fn(auth::CandidateCredential) -> Fut,
    Fut: Future<Output = auth::Validation>,
{
    auth::inspect_prefix(stores(home), prefix, query, probe_url, storage, validate).await
}

/// Inspects browser profiles whose jars carry every cookie in `required` — the fetch
/// gate of providers whose API authenticates against a cookie pair rather than one
/// named session.
pub async fn inspect_sources_all<F, Fut>(
    home: Option<&Path>,
    storage: &dyn SafeStorage,
    required: &[&str],
    query: &Query,
    probe_url: &str,
    validate: F,
) -> Vec<AuthCandidate>
where
    F: Fn(auth::CandidateCredential) -> Fut,
    Fut: Future<Output = auth::Validation>,
{
    auth::inspect_all(stores(home), required, query, probe_url, storage, validate).await
}

/// Puts scanned browser/profile candidates under the `browser` authentication mode.
///
/// The settings protocol chooses a mode first, then renders its candidates. Browser stores
/// therefore cannot be the report's top-level entries even when only one browser mode exists.
pub fn browser_sources(children: Vec<AuthCandidate>) -> Vec<AuthCandidate> {
    let state = aggregate_state(&children);
    vec![AuthCandidate {
        id: BROWSER_SOURCE.into(),
        title: "Browser".into(),
        subtitle: None,
        state: state.as_wire().to_owned(),
        children,
    }]
}

fn aggregate_state(candidates: &[AuthCandidate]) -> AuthCandidateState {
    let states = candidates.iter().filter_map(AuthCandidate::state);
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
        .any(|state| state == AuthCandidateState::Challenged)
    {
        return AuthCandidateState::Challenged;
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
    use super::{AUTH_BROWSER, AUTH_PROFILE, BROWSER_SOURCE, selection, session, store_selection};
    use crate::browser::{Query, SafeStorage, auth::Selection};
    #[cfg(unix)]
    use crate::providers::ProviderError;
    use crate::secrets::SecretError;
    use rusqlite::Connection;
    use tidemark_types::AuthCandidate;

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

    #[derive(Debug)]
    #[cfg(unix)]
    struct LockedKeyring;

    #[cfg(unix)]
    impl SafeStorage for LockedKeyring {
        fn password(
            &self,
            _application: &str,
        ) -> crate::providers::BoxFuture<'_, Result<Option<String>, SecretError>> {
            Box::pin(async { Err(SecretError::Locked) })
        }
    }

    fn gecko_profile(
        home: &crate::browser::tests::TestHome,
        profile: &str,
        cookies: &[(&str, &str)],
    ) {
        let rows: Vec<(&str, &str, &str)> = cookies
            .iter()
            .map(|(name, value)| (*name, *value, "/"))
            .collect();
        gecko_jar(home, profile, &rows);
    }

    /// Writes one profile's whole jar in a single database. `TestHome::gecko` empties the
    /// database on every call, so a profile written twice keeps only its second jar —
    /// scope tests need both rows of one jar to mean anything.
    fn gecko_jar(
        home: &crate::browser::tests::TestHome,
        profile: &str,
        cookies: &[(&str, &str, &str)],
    ) {
        let database = home.gecko(profile);
        let connection = Connection::open(database).expect("opens");
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
        for (name, value, path) in cookies {
            connection
                .execute(
                    "INSERT INTO moz_cookies (
                        host, name, value, path, expiry, isSecure, lastAccessed,
                        creationTime, isHttpOnly
                    ) VALUES ('.example.com', ?1, ?2, ?3, 0, 1, 0, 0, 0)",
                    (name, value, path),
                )
                .expect("inserts a cookie");
        }
    }

    fn query() -> Query {
        Query::new(["example.com"], Vec::<String>::new())
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn the_session_comes_from_exactly_the_selected_browser_and_profile() {
        // Removing the selected-profile filter would send another browser's account cookie.
        let home = crate::browser::tests::TestHome::new();
        gecko_profile(&home, ".mozilla/firefox/aa", &[("session", "tok")]);
        gecko_profile(&home, ".zen/bb", &[("session", "other")]);
        let selection = Selection {
            browser: "firefox".into(),
            profile: Some("aa".into()),
        };

        let found = runtime()
            .block_on(session(
                Some(home.path()),
                &NoKeyring,
                &selection,
                &["session"],
                &query(),
                "https://example.com/api",
            ))
            .expect("reads the selected store")
            .expect("finds the selected session");

        assert_eq!(found.session_name, "session");
        assert_eq!(found.session_value, "tok");
        assert_eq!(found.header, "session=tok");
    }

    #[test]
    fn a_jar_without_a_recognised_session_cookie_is_not_a_credential() {
        // Treating analytics cookies as credentials would make a provider poll an account it cannot use.
        let home = crate::browser::tests::TestHome::new();
        gecko_profile(&home, ".mozilla/firefox/aa", &[("_ga", "analytics")]);
        let selection = Selection {
            browser: "firefox".into(),
            profile: None,
        };

        let found = runtime()
            .block_on(session(
                Some(home.path()),
                &NoKeyring,
                &selection,
                &["session"],
                &query(),
                "https://example.com/api",
            ))
            .expect("reads the selected store");

        assert!(found.is_none());
    }

    #[test]
    fn an_unnamed_session_accepts_any_live_cookie_in_the_selected_jar() {
        // Requiring a name here would make whole-jar providers fail despite having a valid session.
        let home = crate::browser::tests::TestHome::new();
        gecko_profile(&home, ".mozilla/firefox/aa", &[("provider-session", "tok")]);
        let selection = Selection {
            browser: "firefox".into(),
            profile: None,
        };

        let found = runtime()
            .block_on(session(
                Some(home.path()),
                &NoKeyring,
                &selection,
                &[],
                &query(),
                "https://example.com/api",
            ))
            .expect("reads the selected store")
            .expect("finds any live cookie");

        assert_eq!(found.session_name, "provider-session");
        assert_eq!(found.session_value, "tok");
    }

    #[test]
    fn a_prefix_gate_accepts_a_session_whose_name_carries_a_suffix() {
        // Ory rotates the session cookie's suffix, so an exact-name gate would reject a live session.
        let home = crate::browser::tests::TestHome::new();
        gecko_profile(
            &home,
            ".mozilla/firefox/aa",
            &[("ory_session_admin", "tok")],
        );
        let selection = Selection {
            browser: "firefox".into(),
            profile: None,
        };

        let found = runtime()
            .block_on(super::session_prefix(
                Some(home.path()),
                &NoKeyring,
                &selection,
                "ory_session_",
                &query(),
                "https://example.com/api",
            ))
            .expect("reads the selected store")
            .expect("finds the prefixed session");

        assert_eq!(found.session_name, "ory_session_admin");
        assert_eq!(found.session_value, "tok");
    }

    #[test]
    fn a_session_cookie_the_request_url_would_not_receive_is_not_a_credential() {
        // Returning the jar anyway would let a provider send a bearer from a host or path
        // scope the request never carries — possibly another account's session.
        let home = crate::browser::tests::TestHome::new();
        gecko_jar(
            &home,
            ".mozilla/firefox/aa",
            &[
                ("session", "narrow", "/settings"),
                ("analytics", "public", "/"),
            ],
        );
        let selection = Selection {
            browser: "firefox".into(),
            profile: None,
        };

        let found = runtime()
            .block_on(session(
                Some(home.path()),
                &NoKeyring,
                &selection,
                &["session"],
                &query(),
                "https://example.com/api",
            ))
            .expect("reads the selected store");

        assert!(
            found.is_none(),
            "the only session cookie is path-scoped away"
        );
    }

    #[test]
    fn the_session_value_is_the_same_named_cookie_the_header_carries() {
        // Taking the first same-named row in the jar instead of the scoped one would send
        // a path-scoped duplicate's value as the bearer beside the header's real cookie.
        let home = crate::browser::tests::TestHome::new();
        gecko_jar(
            &home,
            ".mozilla/firefox/aa",
            &[("session", "narrow", "/settings"), ("session", "root", "/")],
        );
        let selection = Selection {
            browser: "firefox".into(),
            profile: None,
        };

        let found = runtime()
            .block_on(session(
                Some(home.path()),
                &NoKeyring,
                &selection,
                &["session"],
                &query(),
                "https://example.com/api",
            ))
            .expect("reads the selected store")
            .expect("finds the scoped session");

        assert_eq!(found.session_value, "root");
        assert_eq!(found.header, "session=root");
    }

    #[test]
    fn inspecting_a_prefixed_session_ignores_an_unrelated_cookie_jar() {
        // Advertising a source as selectable based on analytics cookies would defer a missing
        // credential error until the next poll.
        let home = crate::browser::tests::TestHome::new();
        gecko_profile(&home, ".mozilla/firefox/aa", &[("_ga", "analytics")]);

        let report = runtime().block_on(super::inspect_sources_prefix(
            Some(home.path()),
            &NoKeyring,
            "_zm_",
            &query(),
            "https://example.com/api",
            |_| async { unreachable!("a non-session jar must not be validated") },
        ));

        assert_eq!(report[0].children[0].state, "missing");
    }

    #[test]
    fn a_named_inspection_ignores_a_jar_without_any_of_the_names() {
        // Advertising a source as selectable because it holds analytics cookies would
        // defer a missing credential error until the person has selected the source.
        let home = crate::browser::tests::TestHome::new();
        gecko_profile(&home, ".mozilla/firefox/aa", &[("_ga", "analytics")]);

        let report = runtime().block_on(super::inspect_sources(
            Some(home.path()),
            &NoKeyring,
            &["session"],
            &query(),
            "https://example.com/api",
            |_| async { unreachable!("a non-session jar must not be validated") },
        ));

        assert_eq!(report[0].children[0].state, "missing");
    }

    #[test]
    fn an_inspection_ignores_a_session_cookie_the_request_url_would_not_receive() {
        // Gating on the whole jar would run the proof on a header that never carries the
        // session, painting as rejected a source the poll itself treats as missing.
        let home = crate::browser::tests::TestHome::new();
        gecko_jar(
            &home,
            ".mozilla/firefox/aa",
            &[
                ("session", "narrow", "/settings"),
                ("analytics", "public", "/"),
            ],
        );

        let report = runtime().block_on(super::inspect_sources(
            Some(home.path()),
            &NoKeyring,
            &["session"],
            &query(),
            "https://example.com/api",
            |_| async { unreachable!("a jar without a scoped session must not be validated") },
        ));

        assert_eq!(report[0].children[0].state, "missing");
    }

    #[test]
    fn a_whole_jar_inspection_accepts_any_live_cookie_in_the_jar() {
        // The whole-jar providers gate on nothing: an empty names slice is their fetch gate.
        let home = crate::browser::tests::TestHome::new();
        gecko_profile(&home, ".mozilla/firefox/aa", &[("provider-session", "tok")]);

        let report = runtime().block_on(super::inspect_sources(
            Some(home.path()),
            &NoKeyring,
            &[],
            &query(),
            "https://example.com/api",
            |_| async { crate::browser::auth::Validation::Ready },
        ));

        assert_eq!(report[0].children[0].state, "ready");
    }

    #[test]
    fn browser_sources_wrap_profiles_in_the_browser_mode() {
        // Returning Firefox at the top level leaves a Browser mode with no matching body,
        // so the settings UI has to say that its selected mode is not offered.
        let report = super::browser_sources(vec![AuthCandidate {
            id: "firefox".into(),
            title: "Firefox".into(),
            subtitle: None,
            state: "ready".into(),
            children: Vec::new(),
        }]);

        assert_eq!(report.len(), 1);
        assert_eq!(report[0].id, BROWSER_SOURCE);
        assert_eq!(report[0].state, "ready");
        assert_eq!(report[0].children[0].id, "firefox");
    }

    #[test]
    fn a_ready_browser_keeps_the_mode_ready_when_another_source_waits_for_the_keyring() {
        let report = super::browser_sources(vec![
            AuthCandidate {
                id: "firefox".into(),
                title: "Firefox".into(),
                subtitle: None,
                state: "ready".into(),
                children: Vec::new(),
            },
            AuthCandidate {
                id: "chromium".into(),
                title: "Chromium".into(),
                subtitle: None,
                state: "waiting-for-keyring".into(),
                children: Vec::new(),
            },
        ]);

        assert_eq!(report[0].state, "ready");
    }

    // Pins the Secret Service (unix keyring) locked-state contract.
    #[cfg(unix)]
    #[test]
    fn a_locked_keyring_without_another_answer_is_a_waiting_state() {
        // Converting this to None would tell the user to reauthenticate while the keyring is merely locked.
        let home = crate::browser::tests::TestHome::new();
        home.profile("chromium/Default", "Cookies");
        let selection = Selection {
            browser: "chromium".into(),
            profile: None,
        };

        let result = runtime().block_on(session(
            Some(home.path()),
            &LockedKeyring,
            &selection,
            &["session"],
            &query(),
            "https://example.com/api",
        ));

        assert!(matches!(result, Err(ProviderError::KeyringLocked)));
    }

    #[test]
    fn a_selection_round_trips_and_an_absent_profile_leaves_the_default_eligible() {
        // Retaining a stale profile would silently stop the browser's default profile from winning.
        let expected = Selection {
            browser: "firefox".into(),
            profile: Some("aa".into()),
        };
        let mut options = crate::providers::keyed::Options::new();

        store_selection(&mut options, &expected);
        assert_eq!(selection(&options), Some(expected));

        let default = Selection {
            browser: "firefox".into(),
            profile: None,
        };
        store_selection(&mut options, &default);
        assert_eq!(options.get(AUTH_BROWSER), Some(&"firefox".to_owned()));
        assert!(!options.contains_key(AUTH_PROFILE));
        assert_eq!(selection(&options), Some(default));
    }
}
