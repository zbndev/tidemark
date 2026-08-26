//! Reading a browser's cookies, for the providers whose only credential is a session.
//!
//! # Why this exists
//!
//! Some services publish usage to their own dashboard and nowhere else: there is no API
//! key to paste, and the only thing that authenticates a request is the session cookie the
//! user's browser already holds. Cursor is the first of them here. Asking a person to open
//! the network panel, find a request and copy a header out of it is a credential ceremony
//! nobody should have to repeat every time a session rolls over, so this module reads the
//! cookie the same way the browser would.
//!
//! # The rules this module keeps
//!
//! - **Nothing is written into a browser's directory.** A live browser holds its cookie
//!   database in WAL mode, and opening that database directly can create the `-shm` and
//!   `-wal` sidecars — files in someone else's application directory that we have no
//!   business creating. Every read is taken from a private copy ([`Snapshot`]), which is
//!   also what makes the read immune to the browser writing underneath it.
//! - **Only the asked-for cookies leave this module.** A query names its domains, so a
//!   provider that wants a Cursor session never gets handed a bank's.
//! - **A cookie value is a secret.** [`Cookie`] prints its value as redacted for the same
//!   reason [`crate::providers::Credential`] does: the workspace warns on a missing
//!   `Debug`, and a derived one would put a live session in the log.
//! - **A locked keyring is a state, not a failure.** Chromium seals its cookie values with
//!   a password kept in the Secret Service; if the collection is locked we say so and let
//!   the caller wait, exactly as [`crate::secrets`] does.
//!
//! # What a browser stores
//!
//! Two families, and the difference is the whole of the work:
//!
//! - **Gecko** — Firefox, Zen, LibreWolf. `cookies.sqlite`, table `moz_cookies`, values in
//!   plain text.
//! - **Chromium** — Chrome, Chromium, Brave, Edge, Vivaldi, Opera. `Cookies`, table
//!   `cookies`, values sealed with `os_crypt`. See [`chromium`] for that scheme.

pub mod auth;
pub mod chromium;
pub mod gecko;
mod safe_storage;

pub use safe_storage::{Keyring, SafeStorage};

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use tidemark_types::Timestamp;

/// How a browser stores its cookies, which is the only thing that differs between the
/// browsers of one family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// Firefox and its forks: a plain-text `moz_cookies` table.
    Gecko,
    /// Chrome and its forks: an `os_crypt`-sealed `cookies` table.
    Chromium,
}

/// One browser this build knows how to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Browser {
    /// The stable spelling a setting stores. Never changes once shipped.
    pub slug: &'static str,
    /// What to call it in front of a person.
    pub title: &'static str,
    /// How it stores cookies.
    pub family: Family,
    /// Directories under `$HOME` that hold this browser's profiles, in the order they are
    /// looked for. More than one because the same browser installs differently as a
    /// distribution package, a Flatpak and a Snap.
    roots: &'static [&'static str],
    /// The `application` attribute this browser files its `os_crypt` password under in the
    /// Secret Service. Empty for the Gecko family, which seals nothing.
    application: &'static str,
}

/// Every browser this build can read, in the order profiles are scanned.
///
/// Chromium-family browsers come first only because their share of Linux desktops is
/// larger; the scan does not stop at the first store, so the order decides which session
/// wins a tie, not which is looked at.
pub static BROWSERS: &[Browser] = &[
    Browser {
        slug: "chrome",
        title: "Google Chrome",
        family: Family::Chromium,
        roots: &[
            ".config/google-chrome",
            ".config/google-chrome-beta",
            ".config/google-chrome-unstable",
            ".var/app/com.google.Chrome/config/google-chrome",
        ],
        application: "chrome",
    },
    Browser {
        slug: "chromium",
        title: "Chromium",
        family: Family::Chromium,
        roots: &[
            ".config/chromium",
            ".var/app/org.chromium.Chromium/config/chromium",
            "snap/chromium/common/chromium",
        ],
        application: "chromium",
    },
    Browser {
        slug: "brave",
        title: "Brave",
        family: Family::Chromium,
        roots: &[
            ".config/BraveSoftware/Brave-Browser",
            ".var/app/com.brave.Browser/config/BraveSoftware/Brave-Browser",
        ],
        application: "brave",
    },
    Browser {
        slug: "edge",
        title: "Microsoft Edge",
        family: Family::Chromium,
        roots: &[
            ".config/microsoft-edge",
            ".var/app/com.microsoft.Edge/config/microsoft-edge",
        ],
        application: "microsoft-edge",
    },
    Browser {
        slug: "vivaldi",
        title: "Vivaldi",
        family: Family::Chromium,
        roots: &[
            ".config/vivaldi",
            ".var/app/com.vivaldi.Vivaldi/config/vivaldi",
        ],
        application: "vivaldi",
    },
    Browser {
        slug: "opera",
        title: "Opera",
        family: Family::Chromium,
        roots: &[".config/opera", ".var/app/com.opera.Opera/config/opera"],
        application: "opera",
    },
    Browser {
        slug: "firefox",
        title: "Firefox",
        family: Family::Gecko,
        roots: &[
            ".config/mozilla/firefox",
            ".mozilla/firefox",
            ".var/app/org.mozilla.firefox/.mozilla/firefox",
            "snap/firefox/common/.mozilla/firefox",
        ],
        application: "",
    },
    Browser {
        slug: "zen",
        title: "Zen",
        family: Family::Gecko,
        roots: &[".zen", ".var/app/app.zen_browser.zen/.zen"],
        application: "",
    },
    Browser {
        slug: "librewolf",
        title: "LibreWolf",
        family: Family::Gecko,
        roots: &[
            ".librewolf",
            ".var/app/io.gitlab.librewolf-community/.librewolf",
        ],
        application: "",
    },
];

/// One profile's cookie database: what a person means by "the session I am signed in with".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    /// Which browser holds it.
    pub browser: Browser,
    /// The profile's directory name — `Default`, `Profile 2`, `k26qcf29.Default (release)`.
    pub profile: String,
    /// The database itself.
    pub path: PathBuf,
}

/// Which cookies a caller wants: a provider names the domains its session lives on, and
/// optionally the names it needs, so nothing else is ever read out of the database.
#[derive(Debug, Clone, Default)]
pub struct Query {
    /// Domains, matched as suffixes: `cursor.com` matches `cursor.com`, `.cursor.com` and
    /// `www.cursor.com`, and does not match `notcursor.com`. Empty matches every domain,
    /// which no provider should ask for.
    pub domains: Vec<String>,
    /// Cookie names. Empty means every name on a matching domain.
    pub names: Vec<String>,
}

impl Query {
    /// A query for one provider's domains and the cookie names its session is carried in.
    pub fn new<D, N>(domains: D, names: N) -> Self
    where
        D: IntoIterator,
        D::Item: Into<String>,
        N: IntoIterator,
        N::Item: Into<String>,
    {
        Self {
            domains: domains.into_iter().map(Into::into).collect(),
            names: names.into_iter().map(Into::into).collect(),
        }
    }

    /// Whether a stored cookie is one of the ones asked for.
    pub(crate) fn matches(&self, host: &str, name: &str) -> bool {
        let host = host.trim_start_matches('.').to_ascii_lowercase();
        let domain_matches = self.domains.is_empty()
            || self.domains.iter().any(|domain| {
                let domain = domain.trim_start_matches('.').to_ascii_lowercase();
                host == domain || host.ends_with(&format!(".{domain}"))
            });
        let name_matches = self.names.is_empty() || self.names.iter().any(|wanted| wanted == name);
        domain_matches && name_matches
    }
}

/// One cookie, as the browser holds it.
#[derive(Clone, PartialEq, Eq)]
pub struct Cookie {
    /// The domain it was set for, with any leading dot kept: `.cursor.com` is a domain
    /// cookie and `cursor.com` a host-only one, and a caller may care which.
    pub host: String,
    /// The cookie's name.
    pub name: String,
    /// The cookie's value. A live session — never log it.
    pub value: String,
    /// The path it was set for.
    pub path: String,
    /// Whether the browser will only send it over HTTPS.
    pub secure: bool,
    /// When it expires, or `None` for a session cookie or an unreadable expiry.
    pub expires_at: Option<Timestamp>,
}

impl Cookie {
    /// Whether this cookie is still live at `now`. A session cookie — one with no stated
    /// expiry — counts as live: the browser holding it is what keeps it alive.
    pub fn is_live(&self, now: Timestamp) -> bool {
        self.expires_at
            .is_none_or(|expires_at| now.seconds_until(expires_at) > 0)
    }
}

#[cfg(test)]
mod scope_tests {
    use super::{Cookie, header_for};

    #[test]
    fn a_cookie_header_keeps_only_the_one_cursor_session_the_request_url_can_receive() {
        // Sending either a www-only token or a path-scoped duplicate to cursor.com lets the
        // server choose an account Tidemark did not validate.
        let cookies = vec![
            Cookie {
                host: ".cursor.com".into(),
                name: "WorkosCursorSessionToken".into(),
                value: "selected".into(),
                path: "/".into(),
                secure: true,
                expires_at: None,
            },
            Cookie {
                host: "www.cursor.com".into(),
                name: "WorkosCursorSessionToken".into(),
                value: "wrong-host".into(),
                path: "/".into(),
                secure: true,
                expires_at: None,
            },
            Cookie {
                host: ".cursor.com".into(),
                name: "WorkosCursorSessionToken".into(),
                value: "wrong-path".into(),
                path: "/settings".into(),
                secure: true,
                expires_at: None,
            },
            Cookie {
                host: ".cursor.com".into(),
                name: "analytics".into(),
                value: "allowed".into(),
                path: "/".into(),
                secure: false,
                expires_at: None,
            },
        ];

        assert_eq!(
            header_for(&cookies, "https://cursor.com/api/usage-summary"),
            "WorkosCursorSessionToken=selected; analytics=allowed"
        );
    }
}

impl fmt::Debug for Cookie {
    /// Written by hand: a derived impl would print a live session the first time anything
    /// traced a cookie.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cookie")
            .field("host", &self.host)
            .field("name", &self.name)
            .field(
                "value",
                &format_args!("<{} bytes redacted>", self.value.len()),
            )
            .field("secure", &self.secure)
            .finish_non_exhaustive()
    }
}

/// The `Cookie:` header a browser would send to one HTTPS request URL.
///
/// Browser stores can contain same-named cookies for several hosts and paths.  Sending all
/// of them makes the remote server choose a session rather than the browser's own matching
/// rules, so this keeps request host/path/secure scope and the first matching name only.
pub fn header_for(cookies: &[Cookie], request_url: &str) -> String {
    let Ok(url) = reqwest::Url::parse(request_url) else {
        return String::new();
    };
    let Some(host) = url.host_str() else {
        return String::new();
    };
    let path = url.path();
    let secure = url.scheme() == "https";
    let mut names = std::collections::BTreeSet::new();

    cookies
        .iter()
        .filter(|cookie| {
            (!cookie.secure || secure)
                && cookie_host_matches(&cookie.host, host)
                && cookie_path_matches(&cookie.path, path)
                && names.insert(&cookie.name)
        })
        .map(|cookie| format!("{}={}", cookie.name, cookie.value))
        .collect::<Vec<_>>()
        .join("; ")
}

fn cookie_host_matches(cookie_host: &str, request_host: &str) -> bool {
    let request_host = request_host.to_ascii_lowercase();
    match cookie_host.strip_prefix('.') {
        Some(domain) => {
            let domain = domain.to_ascii_lowercase();
            request_host == domain || request_host.ends_with(&format!(".{domain}"))
        }
        None => request_host == cookie_host.to_ascii_lowercase(),
    }
}

fn cookie_path_matches(cookie_path: &str, request_path: &str) -> bool {
    let cookie_path = if cookie_path.is_empty() {
        "/"
    } else {
        cookie_path
    };
    request_path.starts_with(cookie_path)
        && (cookie_path.ends_with('/')
            || request_path.len() == cookie_path.len()
            || request_path.as_bytes().get(cookie_path.len()) == Some(&b'/'))
}

/// Why cookies could not be read.
#[derive(Debug, thiserror::Error)]
pub enum CookieError {
    /// The database could not be copied or opened.
    #[error("{path} could not be read: {source}")]
    Unreadable {
        /// What was being read.
        path: PathBuf,
        /// Why it could not be.
        #[source]
        source: std::io::Error,
    },
    /// The copy opened but is not the database this build expects — a schema change, or a
    /// file that is not a cookie store at all.
    #[error("{path} is not a readable cookie database: {source}")]
    Database {
        /// What was being read.
        path: PathBuf,
        /// Why it could not be read.
        #[source]
        source: rusqlite::Error,
    },
    /// The Secret Service holds the browser's key and the collection is locked. A state to
    /// wait out, not a failure to report — see the module docs.
    #[error("the keyring is locked")]
    KeyringLocked,
    /// Nothing answered on the bus, so the browser's key cannot be reached at all.
    #[error("the keyring is unavailable: {0}")]
    KeyringUnavailable(String),
}

impl Store {
    /// The cookies in this store that the query asks for.
    ///
    /// Async only because a Chromium store's key lives in the Secret Service; a Gecko store
    /// never touches `storage` and answers without a bus.
    pub async fn cookies(
        &self,
        query: &Query,
        storage: &dyn SafeStorage,
    ) -> Result<Vec<Cookie>, CookieError> {
        let snapshot = Snapshot::of(&self.path)?;
        match self.browser.family {
            Family::Gecko => gecko::read(&snapshot.database(), query),
            Family::Chromium => {
                let password = storage.password(self.browser.application).await.map_err(
                    |error| match error {
                        crate::secrets::SecretError::Locked => CookieError::KeyringLocked,
                        other => CookieError::KeyringUnavailable(other.to_string()),
                    },
                )?;
                chromium::read(&snapshot.database(), query, password.as_deref())
            }
        }
    }
}

/// Every cookie database on this machine, in [`BROWSERS`] order.
///
/// Cheap: it stats directories and never opens a database, so a caller may call it on
/// every poll rather than caching a list that a newly created profile would make stale.
pub fn stores() -> Vec<Store> {
    match std::env::var_os("HOME") {
        Some(home) => stores_in(Path::new(&home)),
        None => Vec::new(),
    }
}

/// [`stores`], against a stated home directory, so the scan is testable against a fixture
/// tree rather than against whatever browsers a developer happens to have installed.
pub fn stores_in(home: &Path) -> Vec<Store> {
    let mut stores = Vec::new();
    for browser in BROWSERS {
        for root in browser.roots {
            let root = home.join(root);
            let Ok(entries) = std::fs::read_dir(&root) else {
                continue;
            };
            let mut profiles: Vec<_> = entries
                .flatten()
                .filter(|entry| entry.path().is_dir())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect();
            // Read order from a directory is whatever the filesystem says; a stable scan
            // order is what keeps "the first store that has the session" from meaning a
            // different profile on every poll.
            profiles.sort();
            for profile in profiles {
                let directory = root.join(&profile);
                let Some(path) = database(browser.family, &directory) else {
                    continue;
                };
                stores.push(Store {
                    browser: *browser,
                    profile,
                    path,
                });
            }
        }
    }
    stores
}

/// The cookie database inside one profile directory, if it holds one.
fn database(family: Family, profile: &Path) -> Option<PathBuf> {
    let candidates: &[&str] = match family {
        // Chromium moved cookies under `Network/` in M96 and left the old location in
        // place for profiles that predate it, so both are real.
        Family::Chromium => &["Network/Cookies", "Cookies"],
        Family::Gecko => &["cookies.sqlite"],
    };
    candidates
        .iter()
        .map(|name| profile.join(name))
        .find(|path| path.is_file())
}

/// A private copy of a cookie database, deleted when it goes out of scope.
///
/// The copy exists so that nothing is written into the browser's own directory: opening a
/// WAL database read-only still needs to create its `-shm` sidecar, and a browser that is
/// running would find a file it did not put there. Copying the sidecars along with the
/// main file is what makes the copy show the same cookies the browser itself would see.
#[derive(Debug)]
struct Snapshot {
    directory: PathBuf,
}

impl Snapshot {
    fn of(path: &Path) -> Result<Self, CookieError> {
        use std::os::unix::fs::DirBuilderExt;

        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "tidemark-cookies-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        let unreadable = |source| CookieError::Unreadable {
            path: path.to_path_buf(),
            source,
        };
        // Owner-only, because what lands in it is a set of live sessions.
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(unreadable)?;
        let snapshot = Self { directory };
        std::fs::copy(path, snapshot.database()).map_err(unreadable)?;
        for sidecar in ["-wal", "-shm"] {
            let source = with_suffix(path, sidecar);
            if source.is_file() {
                std::fs::copy(&source, with_suffix(&snapshot.database(), sidecar))
                    .map_err(unreadable)?;
            }
        }
        Ok(snapshot)
    }

    fn database(&self) -> PathBuf {
        self.directory.join("cookies")
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        // Best effort: a copy left behind is owner-only and in the temp directory, and
        // there is nothing useful to do with the failure of a cleanup.
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn cookie(host: &str, name: &str, value: &str) -> Cookie {
        Cookie {
            host: host.to_owned(),
            name: name.to_owned(),
            value: value.to_owned(),
            path: "/".to_owned(),
            secure: true,
            expires_at: None,
        }
    }

    #[test]
    fn a_domain_matches_its_own_host_and_its_subdomains_and_nothing_that_merely_ends_in_it() {
        let query = Query::new(["cursor.com"], Vec::<String>::new());

        assert!(query.matches("cursor.com", "anything"));
        assert!(query.matches(".cursor.com", "anything"));
        assert!(query.matches("www.cursor.com", "anything"));
        assert!(!query.matches("notcursor.com", "anything"));
        assert!(!query.matches("cursor.com.evil.test", "anything"));
    }

    #[test]
    fn a_named_query_reads_only_the_names_it_asked_for() {
        let query = Query::new(["cursor.com"], ["WorkosCursorSessionToken"]);

        assert!(query.matches(".cursor.com", "WorkosCursorSessionToken"));
        assert!(!query.matches(".cursor.com", "_ga"));
    }

    #[test]
    fn a_cookie_never_prints_its_value() {
        let rendered = format!(
            "{:?}",
            cookie(".cursor.com", "session", "do-not-print-this")
        );

        assert!(rendered.contains("session"));
        assert!(!rendered.contains("do-not-print-this"));
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn a_session_cookie_is_live_and_a_past_expiry_is_not() {
        let now = Timestamp::from_unix(1_787_000_000).expect("plausible");
        let mut expired = cookie(".cursor.com", "session", "abc");
        expired.expires_at = Some(now.saturating_add_seconds(-1));
        let mut live = cookie(".cursor.com", "session", "abc");
        live.expires_at = Some(now.saturating_add_seconds(60));

        assert!(cookie(".cursor.com", "session", "abc").is_live(now));
        assert!(live.is_live(now));
        assert!(!expired.is_live(now));
    }

    #[test]
    fn the_scan_finds_every_profile_of_every_installed_browser_and_nothing_else() {
        let home = crate::browser::tests::TestHome::new();
        home.profile("google-chrome/Default", "Cookies");
        home.profile("google-chrome/Profile 1", "Network/Cookies");
        home.profile("chromium/Default", "Cookies");
        home.gecko(".zen/k26qcf29.Default (release)");
        // A profile directory with no cookie database is not a store.
        std::fs::create_dir_all(home.path().join(".config/google-chrome/System Profile"))
            .expect("creates");

        let stores = stores_in(home.path());

        let found: Vec<(&str, &str)> = stores
            .iter()
            .map(|store| (store.browser.slug, store.profile.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                ("chrome", "Default"),
                ("chrome", "Profile 1"),
                ("chromium", "Default"),
                ("zen", "k26qcf29.Default (release)"),
            ]
        );
        assert!(stores[1].path.ends_with("Profile 1/Network/Cookies"));
    }

    #[test]
    fn a_firefox_profile_in_the_config_directory_is_scanned() {
        let home = TestHome::new();
        let path = home
            .path()
            .join(".config/mozilla/firefox/j7aiac5u.default-release/cookies.sqlite");
        std::fs::create_dir_all(path.parent().expect("has parent")).expect("creates");
        std::fs::write(path, b"").expect("creates cookie database");

        let stores = stores_in(home.path());

        assert!(stores.iter().any(|store| {
            store.browser.slug == "firefox" && store.profile == "j7aiac5u.default-release"
        }));
    }

    #[test]
    fn a_snapshot_copies_the_sidecars_and_takes_the_copy_with_it() {
        let home = TestHome::new();
        let path = home.profile("chromium/Default", "Cookies");
        std::fs::write(with_suffix(&path, "-wal"), b"wal").expect("writes");

        let directory;
        {
            let snapshot = Snapshot::of(&path).expect("copies");
            directory = snapshot.directory.clone();
            assert!(snapshot.database().is_file());
            assert!(with_suffix(&snapshot.database(), "-wal").is_file());
            assert!(!with_suffix(&snapshot.database(), "-shm").exists());
        }
        assert!(!directory.exists(), "the copy is a live session; it goes");
    }

    /// A throwaway home directory: browser profiles have to be *found*, and the finding is
    /// the part worth testing, so the fixture is a real tree rather than a stubbed scan.
    #[derive(Debug)]
    pub(crate) struct TestHome {
        path: PathBuf,
    }

    impl TestHome {
        pub(crate) fn new() -> Self {
            static SERIAL: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "tidemark-browser-test-{}-{}",
                std::process::id(),
                SERIAL.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("creates a test home");
            Self { path }
        }

        pub(crate) fn path(&self) -> &Path {
            &self.path
        }

        /// A Chromium profile holding an empty database at the stated relative name.
        pub(crate) fn profile(&self, profile: &str, database: &str) -> PathBuf {
            let path = self.path.join(".config").join(profile).join(database);
            std::fs::create_dir_all(path.parent().expect("has a parent")).expect("creates");
            std::fs::write(&path, b"").expect("writes");
            path
        }

        /// A Gecko profile holding an empty `cookies.sqlite`.
        pub(crate) fn gecko(&self, profile: &str) -> PathBuf {
            let path = self.path.join(profile).join("cookies.sqlite");
            std::fs::create_dir_all(path.parent().expect("has a parent")).expect("creates");
            std::fs::write(&path, b"").expect("writes");
            path
        }
    }

    impl Drop for TestHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}
