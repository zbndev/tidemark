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
    header_of(&scoped(cookies, request_url))
}

/// The cookies a browser would send to one request URL: request host/path/secure scope,
/// first of each name only, in jar order. What [`header_for`] renders, exposed so a caller
/// that needs one cookie's value picks it from exactly the set the request would carry.
pub(crate) fn scoped<'a>(cookies: &'a [Cookie], request_url: &str) -> Vec<&'a Cookie> {
    let Ok(url) = reqwest::Url::parse(request_url) else {
        return Vec::new();
    };
    let Some(host) = url.host_str() else {
        return Vec::new();
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
        .collect()
}

/// Renders a [`scoped`] selection as the header text.
pub(crate) fn header_of(scoped: &[&Cookie]) -> String {
    scoped
        .iter()
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
    /// The platform can identify the store but cannot decrypt its cookie format. On
    /// Windows this includes Chromium App-Bound `v20`: a state to report, never a panic
    /// and never a faked answer.
    #[error("browser cookies cannot be read on this platform: {0}")]
    PlatformUnavailable(&'static str),
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
                #[cfg(unix)]
                {
                    let password =
                        storage
                            .password(self.browser.application)
                            .await
                            .map_err(|error| match error {
                                crate::secrets::SecretError::Locked => CookieError::KeyringLocked,
                                other => CookieError::KeyringUnavailable(other.to_string()),
                            })?;
                    chromium::read(&snapshot.database(), query, password.as_deref())
                }
                #[cfg(windows)]
                {
                    let _ = storage;
                    let key = safe_storage::key_for(&self.path)?;
                    chromium::read_windows(&snapshot.database(), query, &key)
                }
            }
        }
    }
}

/// Every cookie database on this machine, in [`BROWSERS`] order.
///
/// Cheap: it stats directories and never opens a database, so a caller may call it on
/// every poll rather than caching a list that a newly created profile would make stale.
pub fn stores() -> Vec<Store> {
    let mut stores = Vec::new();
    for browser in BROWSERS {
        // Missing environment roots and absent browser installations are both an empty
        // answer; the scan has no error channel because discovery is absence-tolerant.
        let Ok(roots) = storage::profile_roots(browser) else {
            return Vec::new();
        };
        stores.extend(scan_roots(browser, &roots));
    }
    stores
}

/// [`stores`], against a stated home directory, so the scan is testable against a fixture
/// tree rather than against whatever browsers a developer happens to have installed.
pub fn stores_in(home: &Path) -> Vec<Store> {
    let mut stores = Vec::new();
    for browser in BROWSERS {
        let roots: Vec<PathBuf> = browser.roots.iter().map(|root| home.join(root)).collect();
        stores.extend(scan_roots(browser, &roots));
    }
    stores
}

/// The stores under one browser's profile roots: profiles in name order, roots in the
/// order the browser table states them.
fn scan_roots(browser: &Browser, roots: &[PathBuf]) -> Vec<Store> {
    let mut stores = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
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

/// The name a snapshot's database copy goes by, whatever the browser called the
/// original: the snapshot is Tidemark's, so its layout is too.
const SNAPSHOT_DATABASE: &str = "cookies";

/// Copies one SQLite database through the browser platform's read-only snapshot primitive.
/// The caller owns `directory` and its cleanup; `name` is the private copy's filename.
pub(crate) fn copy_private_database(
    source: &Path,
    directory: &Path,
    name: &str,
) -> Result<(), CookieError> {
    storage::copy_snapshot(source, directory)?;
    if name != SNAPSHOT_DATABASE {
        let from = directory.join(SNAPSHOT_DATABASE);
        let to = directory.join(name);
        std::fs::rename(&from, &to).map_err(|source_error| CookieError::Unreadable {
            path: source.to_path_buf(),
            source: source_error,
        })?;
        for suffix in ["-wal", "-shm"] {
            let sidecar = with_suffix(&from, suffix);
            if sidecar.is_file() {
                std::fs::rename(sidecar, with_suffix(&to, suffix)).map_err(|source_error| {
                    CookieError::Unreadable {
                        path: source.to_path_buf(),
                        source: source_error,
                    }
                })?;
            }
        }
    }
    Ok(())
}

impl Snapshot {
    fn of(path: &Path) -> Result<Self, CookieError> {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        let directory = std::env::temp_dir().join(format!(
            "tidemark-cookies-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        // The copy itself is the platform's half; the name and the cleanup are every
        // platform's.
        match copy_private_database(path, &directory, SNAPSHOT_DATABASE) {
            Ok(()) => Ok(Self { directory }),
            Err(error) => {
                // A half-made copy is a set of live sessions in the temp directory; it
                // goes the moment the copy fails, exactly as it goes on drop.
                let _ = std::fs::remove_dir_all(&directory);
                Err(error)
            }
        }
    }

    fn database(&self) -> PathBuf {
        self.directory.join(SNAPSHOT_DATABASE)
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

/// The platform's half of reading a browser's cookies — where profiles live, and how a
/// private copy of a cookie database is taken. Everything around it (the browser table,
/// the profile scan, the queries, the reading of a copy) is the same on every platform.
///
/// Windows resolves vendor roots from AppData and takes a no-write, share-mode-compatible
/// copy. Linux retains its HOME roots and owner-only copy with SQLite sidecars.
mod storage {
    use std::path::{Path, PathBuf};

    use super::{Browser, CookieError};
    #[cfg(unix)]
    use super::{SNAPSHOT_DATABASE, with_suffix};

    /// The absolute directories one browser keeps its profiles under on this machine,
    /// in scan order. Linux resolves a browser's roots against `$HOME`: distribution
    /// packages under `.config`, Flatpaks under `.var/app`, Snaps under `snap`.
    ///
    /// Absence is an empty answer rather than an error: a root that does not exist is a
    /// browser that is not installed.
    #[cfg(unix)]
    pub(crate) fn profile_roots(browser: &Browser) -> Result<Vec<PathBuf>, CookieError> {
        match std::env::var_os("HOME") {
            Some(home) => Ok(browser
                .roots
                .iter()
                .map(|root| Path::new(&home).join(root))
                .collect()),
            None => Ok(Vec::new()),
        }
    }

    /// [`profile_roots`], for Windows: vendor directories under `%LOCALAPPDATA%` and
    /// `%APPDATA%`. Either environment root may be absent without failing discovery.
    #[cfg(windows)]
    pub(crate) fn profile_roots(browser: &Browser) -> Result<Vec<PathBuf>, CookieError> {
        let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        let roaming = std::env::var_os("APPDATA").map(PathBuf::from);
        Ok(match (local, roaming) {
            (Some(local), Some(roaming)) => profile_roots_in(browser, &local, &roaming),
            (Some(local), None) => profile_roots_in(browser, &local, Path::new("")),
            (None, Some(roaming)) => profile_roots_in(browser, Path::new(""), &roaming),
            (None, None) => Vec::new(),
        })
    }

    #[cfg(windows)]
    pub(crate) fn profile_roots_in(
        browser: &Browser,
        local: &Path,
        roaming: &Path,
    ) -> Vec<PathBuf> {
        let (base, roots): (&Path, &[&str]) = match browser.slug {
            "chrome" => (
                local,
                &[
                    "Google/Chrome/User Data",
                    "Google/Chrome Beta/User Data",
                    "Google/Chrome Dev/User Data",
                    "Google/Chrome SxS/User Data",
                ],
            ),
            "chromium" => (local, &["Chromium/User Data"]),
            "brave" => (
                local,
                &[
                    "BraveSoftware/Brave-Browser/User Data",
                    "BraveSoftware/Brave-Browser-Beta/User Data",
                    "BraveSoftware/Brave-Browser-Nightly/User Data",
                ],
            ),
            "edge" => (
                local,
                &[
                    "Microsoft/Edge/User Data",
                    "Microsoft/Edge Beta/User Data",
                    "Microsoft/Edge Dev/User Data",
                    "Microsoft/Edge SxS/User Data",
                ],
            ),
            "vivaldi" => (local, &["Vivaldi/User Data"]),
            "opera" => (roaming, &["Opera Software"]),
            "firefox" => (roaming, &["Mozilla/Firefox/Profiles"]),
            "zen" => (roaming, &["zen/Profiles"]),
            "librewolf" => (roaming, &["librewolf/Profiles"]),
            _ => return Vec::new(),
        };
        if base.as_os_str().is_empty() {
            return Vec::new();
        }
        roots.iter().map(|root| base.join(root)).collect()
    }

    /// Takes the private copy of one cookie database into `directory`: the directory is
    /// created owner-only, because what lands in it is a set of live sessions, and the
    /// database plus any `-wal`/`-shm` sidecars beside it are copied in — copying the
    /// sidecars is what makes the copy show the same cookies the browser itself sees.
    #[cfg(unix)]
    pub(crate) fn copy_snapshot(source: &Path, directory: &Path) -> Result<(), CookieError> {
        use std::os::unix::fs::DirBuilderExt;

        let unreadable = |error: std::io::Error| CookieError::Unreadable {
            // The browser's own database is the path an error carries — not the copy's,
            // which is Tidemark's private business.
            path: source.to_path_buf(),
            source: error,
        };
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(directory)
            .map_err(unreadable)?;
        let database = directory.join(SNAPSHOT_DATABASE);
        std::fs::copy(source, &database).map_err(unreadable)?;
        for sidecar in ["-wal", "-shm"] {
            let sidecar_source = with_suffix(source, sidecar);
            if sidecar_source.is_file() {
                std::fs::copy(&sidecar_source, with_suffix(&database, sidecar))
                    .map_err(unreadable)?;
            }
        }
        Ok(())
    }

    /// [`copy_snapshot`], for Windows, where the private-copy discipline needs the
    /// platform's own share-mode open. Declared for todo 17 of the Windows port; until
    /// then this platform answers that its half is missing.
    #[cfg(windows)]
    #[allow(unsafe_code)]
    pub(crate) fn copy_snapshot(source: &Path, directory: &Path) -> Result<(), CookieError> {
        use std::os::windows::ffi::OsStrExt as _;
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows::Win32::Storage::FileSystem::{
            CopyFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        use windows::core::PCWSTR;

        let unreadable = |error: std::io::Error| CookieError::Unreadable {
            path: source.to_path_buf(),
            source: error,
        };
        std::fs::create_dir(directory).map_err(unreadable)?;
        // Keep a share-mode read handle open while CopyFileW takes the snapshot. This asks
        // for no write access and remains compatible with a live browser's SQLite handle.
        let _source_handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
            .open(source)
            .map_err(unreadable)?;
        let destination = directory.join(super::SNAPSHOT_DATABASE);
        let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
        let destination_wide: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // SAFETY: both arguments are live, NUL-terminated UTF-16 paths for this call.
        unsafe {
            CopyFileW(
                PCWSTR(source_wide.as_ptr()),
                PCWSTR(destination_wide.as_ptr()),
                true,
            )
        }
        .map_err(|error| unreadable(std::io::Error::other(error)))?;
        Ok(())
    }
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

    /// Plants a cookie database at `home/root/profile/database`, the way an installed
    /// browser would have left it.
    fn plant(home: &Path, root: &str, profile: &str, database: &str) {
        let path = home.join(root).join(profile).join(database);
        std::fs::create_dir_all(path.parent().expect("has a parent")).expect("creates");
        std::fs::write(&path, b"").expect("writes");
    }

    #[test]
    fn the_machine_scan_is_the_scan_of_the_home_directory() {
        // `stores()` is `stores_in($HOME)`: the only thing the machine scan knows that a
        // stated-home scan does not is where the home directory is.
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };

        assert_eq!(stores(), stores_in(Path::new(&home)));
    }

    #[test]
    fn flatpak_and_snap_roots_are_scanned_like_packaged_ones() {
        let home = TestHome::new();
        plant(
            home.path(),
            ".var/app/com.google.Chrome/config/google-chrome",
            "Default",
            "Cookies",
        );
        plant(
            home.path(),
            "snap/chromium/common/chromium",
            "Default",
            "Cookies",
        );
        plant(
            home.path(),
            "snap/firefox/common/.mozilla/firefox",
            "xyz42.default",
            "cookies.sqlite",
        );

        let stores = stores_in(home.path());

        let found: Vec<(&str, &str)> = stores
            .iter()
            .map(|store| (store.browser.slug, store.profile.as_str()))
            .collect();
        assert_eq!(
            found,
            [
                ("chrome", "Default"),
                ("chromium", "Default"),
                ("firefox", "xyz42.default"),
            ]
        );
    }

    #[test]
    fn a_profile_holding_both_cookie_locations_reads_the_network_one() {
        let home = TestHome::new();
        // Chromium moved cookies under `Network/` in M96 and kept the old location for
        // profiles that predate it; a profile holding both answers through the new one.
        plant(home.path(), ".config/google-chrome", "Default", "Cookies");
        plant(
            home.path(),
            ".config/google-chrome",
            "Default",
            "Network/Cookies",
        );

        let stores = stores_in(home.path());

        assert_eq!(stores.len(), 1);
        assert!(stores[0].path.ends_with("Default/Network/Cookies"));
    }

    #[test]
    fn a_profile_is_a_store_only_through_its_own_familys_database_name() {
        let home = TestHome::new();
        // A Gecko database inside a Chromium profile, and a Chromium database inside a
        // Gecko one, are both just files with the wrong names for their browser.
        plant(home.path(), ".config/chromium", "Default", "cookies.sqlite");
        plant(home.path(), ".mozilla/firefox", "abcd.default", "Cookies");

        let stores = stores_in(home.path());

        assert!(stores.is_empty());
    }

    #[test]
    fn the_same_profile_name_under_two_roots_is_found_twice_in_root_order() {
        let home = TestHome::new();
        plant(home.path(), ".config/google-chrome", "Default", "Cookies");
        plant(
            home.path(),
            ".var/app/com.google.Chrome/config/google-chrome",
            "Default",
            "Cookies",
        );

        let stores = stores_in(home.path());
        let chrome: Vec<&Store> = stores
            .iter()
            .filter(|store| store.browser.slug == "chrome")
            .collect();

        assert_eq!(chrome.len(), 2);
        assert_eq!(chrome[0].profile, "Default");
        assert!(
            chrome[0]
                .path
                .starts_with(home.path().join(".config/google-chrome"))
        );
        assert!(
            chrome[1].path.starts_with(
                home.path()
                    .join(".var/app/com.google.Chrome/config/google-chrome")
            )
        );
    }

    #[test]
    fn a_scan_of_a_home_with_garbage_in_it_skips_the_garbage() {
        let home = TestHome::new();
        std::fs::create_dir_all(home.path().join(".config")).expect("creates");
        // A root that is a file, a profile that is a file, a database that is a
        // directory: none of it is this scan's business to report, and none of it stops
        // the scan from finding what is real.
        std::fs::write(home.path().join(".config/vivaldi"), b"not a directory").expect("writes");
        std::fs::create_dir_all(home.path().join(".config/opera")).expect("creates");
        std::fs::write(home.path().join(".config/opera/Default"), b"not a profile")
            .expect("writes");
        std::fs::create_dir_all(
            home.path()
                .join(".config/BraveSoftware/Brave-Browser/Default/Network/Cookies"),
        )
        .expect("creates a directory where the database would be");
        plant(home.path(), ".config/google-chrome", "Default", "Cookies");

        let stores = stores_in(home.path());

        assert_eq!(stores.len(), 1);
        assert_eq!(stores[0].browser.slug, "chrome");
    }

    #[test]
    fn a_home_without_browser_roots_answers_no_stores() {
        let home = TestHome::new();

        assert!(stores_in(home.path()).is_empty());
        assert!(stores_in(&home.path().join("nowhere")).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_vendor_roots_are_discovered_from_real_profile_trees() {
        let fixture = TestHome::new();
        let local = fixture.path().join("Local");
        let roaming = fixture.path().join("Roaming");
        plant(
            &local,
            "Google/Chrome/User Data",
            "Default",
            "Network/Cookies",
        );
        plant(
            &local,
            "Microsoft/Edge/User Data",
            "Profile 2",
            "Network/Cookies",
        );
        plant(
            &local,
            "BraveSoftware/Brave-Browser/User Data",
            "Default",
            "Cookies",
        );
        plant(
            &roaming,
            "Opera Software",
            "Opera Stable",
            "Network/Cookies",
        );

        let found: Vec<(&str, String)> = BROWSERS
            .iter()
            .flat_map(|browser| {
                storage::profile_roots_in(browser, &local, &roaming)
                    .into_iter()
                    .flat_map(|root| scan_roots(browser, &[root]))
            })
            .map(|store| (store.browser.slug, store.profile))
            .collect();

        assert_eq!(
            found,
            [
                ("chrome", "Default".to_owned()),
                ("brave", "Default".to_owned()),
                ("edge", "Profile 2".to_owned()),
                ("opera", "Opera Stable".to_owned()),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_snapshot_is_a_private_copy_without_unix_sidecars() {
        let home = TestHome::new();
        let path = home.profile("chromium/Default", "Cookies");
        std::fs::write(with_suffix(&path, "-wal"), b"must not be copied").expect("writes");

        let snapshot = Snapshot::of(&path).expect("copies");

        assert_eq!(std::fs::read(snapshot.database()).expect("reads"), b"");
        assert!(!with_suffix(&snapshot.database(), "-wal").exists());
    }

    #[cfg(unix)]
    #[test]
    fn the_snapshot_directory_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let home = TestHome::new();
        let path = home.profile("chromium/Default", "Cookies");
        let snapshot = Snapshot::of(&path).expect("copies");

        let mode = std::fs::metadata(&snapshot.directory)
            .expect("the directory exists")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[cfg(unix)]
    #[test]
    fn both_sidecars_are_copied_when_both_exist() {
        let home = TestHome::new();
        let path = home.profile("chromium/Default", "Cookies");
        std::fs::write(with_suffix(&path, "-wal"), b"wal").expect("writes");
        std::fs::write(with_suffix(&path, "-shm"), b"shm").expect("writes");

        let snapshot = Snapshot::of(&path).expect("copies");

        assert!(with_suffix(&snapshot.database(), "-wal").is_file());
        assert!(with_suffix(&snapshot.database(), "-shm").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn a_source_that_cannot_be_read_is_a_typed_error() {
        let home = TestHome::new();
        let absent = home.path().join(".config/chromium/Default/Cookies");

        match Snapshot::of(&absent) {
            Err(CookieError::Unreadable { path, .. }) => assert_eq!(path, absent),
            other => panic!("an absent source is an Unreadable error, not {other:?}"),
        }

        // And a file this environment genuinely cannot read answers the same way.
        let locked = home.profile("chromium/Default", "Cookies");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
                .expect("locks");
        }
        if std::fs::read(&locked).is_err() {
            assert!(matches!(
                Snapshot::of(&locked),
                Err(CookieError::Unreadable { .. })
            ));
        }
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
