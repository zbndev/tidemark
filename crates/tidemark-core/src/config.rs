//! The settings the user owns, at `$XDG_CONFIG_HOME/tidemark/config.toml`.
//!
//! Not everything a provider needs is a secret. Z.ai is the same API on two hosts and the
//! key alone does not say which; a BigModel CN key answered by `api.z.ai` is a 401 with no
//! hint in it. That choice has to live somewhere the daemon can read at startup and the
//! settings dialog can change, and the Secret Service is the wrong place for it — an
//! attribute of a secret is not itself a secret, and a locked keyring would take the
//! region down with the key.
//!
//! # Why the file is edited rather than rewritten
//!
//! This is a file a person may open. Serialising a struct over the top of it would be
//! correct in content and hostile in every other way: comments gone, ordering shuffled,
//! keys this build does not know silently dropped. [`toml_edit`] keeps the document and
//! replaces one value inside it, so the only thing that changes is the thing that changed.
//!
//! A file that does not parse is an **error, not an empty document**. Starting from blank
//! would look like a working daemon right up to the moment it wrote the file back and took
//! the user's settings with it.

use std::path::{Path, PathBuf};

use tidemark_types::{AuthSelection, Preferences};
use toml_edit::{Array, DocumentMut, Item, Table, Value, value};

use crate::paths;

/// Table every provider's own settings live under: `[provider.zai]`.
const PROVIDER_TABLE: &str = "provider";
const PROVIDERS_KEY: &str = "providers";
/// The shared storage keys used by browser-cookie authentication providers.
const AUTH_SOURCE_KEY: &str = "auth-source";
const AUTH_BROWSER_KEY: &str = "auth-browser";
const AUTH_PROFILE_KEY: &str = "auth-profile";

/// Table the notification opt-in lives under: `[notify.claude]`.
///
/// Deliberately not a key inside `[provider.<slug>]`. That table holds the settings a
/// provider *declares* — enumerated choices the daemon validates against the provider's
/// own list — and an array of window keys the user happens to have switched on is neither
/// declared nor enumerable.
const NOTIFY_TABLE: &str = "notify";
const NOTIFY_WINDOWS_KEY: &str = "windows";

const GENERAL_TABLE: &str = "general";
const MINIMIZE_ON_CLOSE_KEY: &str = "minimize_on_close";
const STARTUP_KEY: &str = "startup";
const UPDATES_TABLE: &str = "updates";
const RELEASE_CHECK_KEY: &str = "check";
const HISTORY_TABLE: &str = "history";
const HISTORY_RETENTION_KEY: &str = "retention";
/// Table the raw-response log lives under: `[debug] raw_responses = true`.
///
/// Deliberately not part of [`Preferences`]. That type is the D-Bus wire shape the
/// interface and every other client read, and this is not a preference anyone is meant to
/// find in a settings dialog: it is a switch a person flips in the file, on purpose, to
/// collect evidence for a bug report.
const DEBUG_TABLE: &str = "debug";
const RAW_RESPONSES_KEY: &str = "raw_responses";
const PROXY_TABLE: &str = "proxy";
const PROXY_MODE_KEY: &str = "mode";
const PROXY_HOST_KEY: &str = "host";
const PROXY_PORT_KEY: &str = "port";

/// Why the settings could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// No base directory could be resolved.
    #[error(transparent)]
    NoBaseDirectory(#[from] paths::NoBaseDirectory),
    /// The file could not be read or written.
    #[error("cannot access {path}: {source}")]
    Io {
        /// What was being accessed.
        path: PathBuf,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },
    /// The file exists and is not valid TOML. Never repaired automatically — see the
    /// module docs.
    #[error("{path} is not valid TOML: {source}")]
    Malformed {
        /// The file that would not parse.
        path: PathBuf,
        /// Where it went wrong.
        #[source]
        source: toml_edit::TomlError,
    },
    /// A table the settings live under is occupied by something that is not a table.
    #[error("{path}: [{table}] is not a table")]
    NotATable {
        /// The file.
        path: PathBuf,
        /// The dotted name of the offending entry.
        table: String,
    },
    /// The configured provider list is not an array of strings.
    #[error("{path}: {reason}")]
    InvalidProviders {
        /// The file.
        path: PathBuf,
        /// Why the provider list is invalid.
        reason: String,
    },
    /// A provider's notification opt-in list is not an array of window keys.
    #[error("{path}: [{NOTIFY_TABLE}.{provider}] {NOTIFY_WINDOWS_KEY} must be an array of strings")]
    InvalidNotify {
        /// The file.
        path: PathBuf,
        /// Whose list it is.
        provider: String,
    },
    /// A browser-auth selection cannot be represented by the shared local-source keys.
    #[error(
        "{path}: [{PROVIDER_TABLE}.{provider}] has an invalid authentication selection: {reason}"
    )]
    InvalidAuthSelection {
        /// The file.
        path: PathBuf,
        /// The provider carrying the selection.
        provider: String,
        /// Why the selected opaque candidate cannot be stored.
        reason: String,
    },
    /// An application preference has a wrong type or an unknown named value.
    #[error("{path}: [{table}] {key} {reason}")]
    InvalidPreference {
        /// The file.
        path: PathBuf,
        /// The table carrying the preference.
        table: &'static str,
        /// The key carrying the preference.
        key: &'static str,
        /// What was wrong with it.
        reason: String,
    },
}

/// The settings file, held open across edits.
#[derive(Debug)]
pub struct Config {
    path: PathBuf,
    document: DocumentMut,
}

impl Config {
    /// Reads the canonical settings file. A file that is not there is an empty document,
    /// which is a normal first run rather than a failure.
    pub fn load() -> Result<Self, ConfigError> {
        Self::at(paths::config_path()?)
    }

    /// Reads a settings file at a given path.
    pub fn at(path: PathBuf) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(source) => return Err(ConfigError::Io { path, source }),
        };
        let document = text
            .parse::<DocumentMut>()
            .map_err(|source| ConfigError::Malformed {
                path: path.clone(),
                source,
            })?;
        Ok(Self { path, document })
    }

    /// Reads the application-wide preferences, applying documented defaults only to
    /// absent keys. A present value with the wrong type is refused rather than guessed.
    pub fn preferences(&self) -> Result<Preferences, ConfigError> {
        let defaults = Preferences::default();
        let history_retention = self
            .preference_string(HISTORY_TABLE, HISTORY_RETENTION_KEY)?
            .unwrap_or(defaults.history_retention.as_str())
            .to_owned();
        if !Preferences::valid_retention(&history_retention) {
            return Err(self.invalid_preference(
                HISTORY_TABLE,
                HISTORY_RETENTION_KEY,
                format!("has unknown value {history_retention:?}"),
            ));
        }
        let startup_mode = self
            .preference_string(GENERAL_TABLE, STARTUP_KEY)?
            .unwrap_or(defaults.startup_mode.as_str())
            .to_owned();
        if !Preferences::valid_startup(&startup_mode) {
            return Err(self.invalid_preference(
                GENERAL_TABLE,
                STARTUP_KEY,
                format!("has unknown value {startup_mode:?}"),
            ));
        }
        let proxy_mode = self
            .preference_string(PROXY_TABLE, PROXY_MODE_KEY)?
            .unwrap_or(defaults.proxy_mode.as_str())
            .to_owned();
        if !Preferences::valid_proxy_mode(&proxy_mode) {
            return Err(self.invalid_preference(
                PROXY_TABLE,
                PROXY_MODE_KEY,
                format!("has unknown value {proxy_mode:?}"),
            ));
        }
        Ok(Preferences {
            release_check: self
                .preference_bool(UPDATES_TABLE, RELEASE_CHECK_KEY)?
                .unwrap_or(defaults.release_check),
            minimize_on_close: self
                .preference_bool(GENERAL_TABLE, MINIMIZE_ON_CLOSE_KEY)?
                .unwrap_or(defaults.minimize_on_close),
            startup_mode,
            history_retention,
            proxy_mode,
            proxy_host: self
                .preference_string(PROXY_TABLE, PROXY_HOST_KEY)?
                .unwrap_or(defaults.proxy_host.as_str())
                .to_owned(),
            proxy_port: self.preference_port(PROXY_TABLE, PROXY_PORT_KEY)?,
            refresh_mode: defaults.refresh_mode.clone(),
            refresh_minutes: defaults.refresh_minutes,
        })
    }

    /// Whether the daemon should write every provider response to the raw-response log.
    ///
    /// Read once, at startup: a switch that took effect mid-poll would leave a file whose
    /// gaps mean nothing. There is no setter, because there is deliberately no control —
    /// see [`DEBUG_TABLE`].
    pub fn debug_raw_responses(&self) -> Result<bool, ConfigError> {
        Ok(self
            .preference_bool(DEBUG_TABLE, RAW_RESPONSES_KEY)?
            .unwrap_or(false))
    }

    pub fn set_release_check(&mut self, enabled: bool) -> Result<(), ConfigError> {
        self.set_preference(UPDATES_TABLE, RELEASE_CHECK_KEY, value(enabled))
    }

    pub fn set_minimize_on_close(&mut self, enabled: bool) -> Result<(), ConfigError> {
        self.set_preference(GENERAL_TABLE, MINIMIZE_ON_CLOSE_KEY, value(enabled))
    }

    pub fn set_startup_mode(&mut self, mode: &str) -> Result<(), ConfigError> {
        if !Preferences::valid_startup(mode) {
            return Err(self.invalid_preference(
                GENERAL_TABLE,
                STARTUP_KEY,
                format!("has unknown value {mode:?}"),
            ));
        }
        self.set_preference(GENERAL_TABLE, STARTUP_KEY, value(mode))
    }

    pub fn set_history_retention(&mut self, retention: &str) -> Result<(), ConfigError> {
        if !Preferences::valid_retention(retention) {
            return Err(self.invalid_preference(
                HISTORY_TABLE,
                HISTORY_RETENTION_KEY,
                format!("has unknown value {retention:?}"),
            ));
        }
        self.set_preference(HISTORY_TABLE, HISTORY_RETENTION_KEY, value(retention))
    }

    /// Writes the whole proxy setting as one edit.
    ///
    /// The three values are one decision: a mode with no host to reach, or a host the mode
    /// does not use, is not a state worth persisting on the way to a state that makes
    /// sense. Whether they agree is the caller's to check — this refuses only what this
    /// build cannot read back.
    pub fn set_proxy(&mut self, mode: &str, host: &str, port: u16) -> Result<(), ConfigError> {
        if !Preferences::valid_proxy_mode(mode) {
            return Err(self.invalid_preference(
                PROXY_TABLE,
                PROXY_MODE_KEY,
                format!("has unknown value {mode:?}"),
            ));
        }
        let table = self.preference_table_mut(PROXY_TABLE)?;
        table.insert(PROXY_MODE_KEY, value(mode));
        table.insert(PROXY_HOST_KEY, value(host));
        table.insert(PROXY_PORT_KEY, value(i64::from(port)));
        self.write()
    }

    fn preference_bool(
        &self,
        table: &'static str,
        key: &'static str,
    ) -> Result<Option<bool>, ConfigError> {
        let Some(item) = self.preference(table, key)? else {
            return Ok(None);
        };
        item.as_bool()
            .map(Some)
            .ok_or_else(|| self.invalid_preference(table, key, "must be true or false".into()))
    }

    fn preference_string(
        &self,
        table: &'static str,
        key: &'static str,
    ) -> Result<Option<&str>, ConfigError> {
        let Some(item) = self.preference(table, key)? else {
            return Ok(None);
        };
        item.as_str()
            .map(Some)
            .ok_or_else(|| self.invalid_preference(table, key, "must be a string".into()))
    }

    /// A TCP port, or zero for a port nobody has set yet.
    ///
    /// Refused rather than clamped when it is out of range: a `70000` in the file was
    /// meant to be a port, and silently reading it as some other port is worse than
    /// saying it is not one.
    fn preference_port(&self, table: &'static str, key: &'static str) -> Result<u16, ConfigError> {
        let Some(item) = self.preference(table, key)? else {
            return Ok(0);
        };
        item.as_integer()
            .and_then(|port| u16::try_from(port).ok())
            .ok_or_else(|| {
                self.invalid_preference(table, key, "must be a whole number of 0 to 65535".into())
            })
    }

    fn preference(
        &self,
        table: &'static str,
        key: &'static str,
    ) -> Result<Option<&Item>, ConfigError> {
        let Some(table_item) = self.document.get(table) else {
            return Ok(None);
        };
        let table_item = table_item
            .as_table_like()
            .ok_or_else(|| ConfigError::NotATable {
                path: self.path.clone(),
                table: table.into(),
            })?;
        Ok(table_item.get(key))
    }

    fn set_preference(
        &mut self,
        table: &'static str,
        key: &'static str,
        setting: Item,
    ) -> Result<(), ConfigError> {
        self.preference_table_mut(table)?.insert(key, setting);
        self.write()
    }

    /// The table one group of preferences lives in, created empty if it is not there yet.
    fn preference_table_mut(
        &mut self,
        table: &'static str,
    ) -> Result<&mut dyn toml_edit::TableLike, ConfigError> {
        let path = self.path.clone();
        self.document
            .entry(table)
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_like_mut()
            .ok_or_else(|| ConfigError::NotATable {
                path,
                table: table.into(),
            })
    }

    fn invalid_preference(
        &self,
        table: &'static str,
        key: &'static str,
        reason: String,
    ) -> ConfigError {
        ConfigError::InvalidPreference {
            path: self.path.clone(),
            table,
            key,
            reason,
        }
    }

    /// One provider setting, if the user has one.
    ///
    /// Only strings are read back. Every setting this file carries is a choice between
    /// named alternatives, and a caller that got a bare `4` where it expected `"global"`
    /// would have to invent a meaning for it.
    pub fn option(&self, provider: &str, name: &str) -> Option<&str> {
        self.document
            .get(PROVIDER_TABLE)?
            .get(provider)?
            .get(name)?
            .as_str()
    }

    /// Returns configured providers in file order, with duplicates removed.
    pub fn providers(&self) -> Result<Vec<String>, ConfigError> {
        let Some(item) = self.document.get(PROVIDERS_KEY) else {
            return Ok(Vec::new());
        };
        let array = item
            .as_array()
            .ok_or_else(|| ConfigError::InvalidProviders {
                path: self.path.clone(),
                reason: "providers must be an array of strings".to_owned(),
            })?;
        let mut seen = std::collections::BTreeSet::new();
        let mut providers = Vec::new();
        for item in array.iter() {
            let slug = item.as_str().ok_or_else(|| ConfigError::InvalidProviders {
                path: self.path.clone(),
                reason: "every providers entry must be a string".to_owned(),
            })?;
            if seen.insert(slug.to_owned()) {
                providers.push(slug.to_owned());
            }
        }
        Ok(providers)
    }

    /// Adds a provider to the configured set and normalizes any existing duplicates.
    pub fn add_provider(&mut self, provider: &str) -> Result<bool, ConfigError> {
        let normalized = self.normalize_providers(None)?;
        let already_configured = self
            .providers()?
            .iter()
            .any(|configured| configured == provider);
        let item = self
            .document
            .entry(PROVIDERS_KEY)
            .or_insert_with(|| Item::Value(Array::new().into()));
        let array = item
            .as_array_mut()
            .ok_or_else(|| ConfigError::InvalidProviders {
                path: self.path.clone(),
                reason: "providers must be an array of strings".to_owned(),
            })?;
        if !already_configured {
            push_provider(array, provider);
        }
        if normalized || !already_configured {
            self.write()?;
        }
        Ok(!already_configured)
    }

    /// Rewrites the order of the configured providers.
    ///
    /// The array is a permutation, not a replacement: `order` must name every configured
    /// provider exactly once. A list that names something else — a slug that is not
    /// configured, a slug that is missing, the same slug twice — is refused with nothing
    /// written, because a client whose idea of the set is out of date should read the set
    /// again rather than have this function guess which half of the disagreement it meant.
    ///
    /// **Only the values move; the decoration stays where it was written.** A TOML array
    /// has no notion of a comment belonging to an element: in the style this file's own
    /// tests use — `"claude", # first survivor` — the comment about one element is part of
    /// the *next* element's prefix, and in the style above it, part of its own. So a
    /// permutation that carried decoration along would scramble both the comments and the
    /// indentation, in opposite directions depending on which style the file uses. Moving
    /// the strings inside the existing layout is the one behaviour that is predictable:
    /// the file comes back byte-identical apart from the slugs.
    pub fn set_provider_order(&mut self, order: &[String]) -> Result<bool, ConfigError> {
        let configured = self.providers()?;
        let mut wanted = order.to_vec();
        wanted.sort_unstable();
        wanted.dedup();
        let mut held = configured.clone();
        held.sort_unstable();
        if wanted.len() != order.len() || wanted != held {
            return Err(ConfigError::InvalidProviders {
                path: self.path.clone(),
                reason: "the order must name every configured provider exactly once".to_owned(),
            });
        }
        if configured == order {
            return Ok(false);
        }

        self.normalize_providers(None)?;
        let array = self
            .document
            .get_mut(PROVIDERS_KEY)
            .and_then(Item::as_array_mut)
            .ok_or_else(|| ConfigError::InvalidProviders {
                path: self.path.clone(),
                reason: "providers must be an array of strings".to_owned(),
            })?;
        // `Array::replace` keeps the decoration of the slot it writes into, which is
        // exactly what is wanted here, and `order` was checked to be a permutation of the
        // normalized array above, so this writes every slug back exactly once.
        for (index, slug) in order.iter().enumerate() {
            array.replace(index, slug.as_str());
        }
        self.write()?;
        Ok(true)
    }

    /// Removes a provider and its settings while normalizing any survivor duplicates.
    pub fn remove_provider(&mut self, provider: &str) -> Result<bool, ConfigError> {
        let providers = self.providers()?;
        let configured = providers.iter().any(|candidate| candidate == provider);

        if configured {
            for name in [PROVIDER_TABLE, NOTIFY_TABLE] {
                let Some(item) = self.document.get_mut(name) else {
                    continue;
                };
                let table = item
                    .as_table_like_mut()
                    .ok_or_else(|| ConfigError::NotATable {
                        path: self.path.clone(),
                        table: name.to_owned(),
                    })?;
                table.remove(provider);
            }
        }
        let normalized = self.normalize_providers(Some(provider))?;
        if configured || normalized {
            self.write()?;
        }
        Ok(configured)
    }

    /// Sets one provider setting and writes the file.
    ///
    /// The write is staged and renamed, so a daemon killed mid-write leaves the previous
    /// settings intact rather than a truncated file the next start refuses to parse.
    pub fn set_option(
        &mut self,
        provider: &str,
        name: &str,
        setting: &str,
    ) -> Result<(), ConfigError> {
        self.normalize_providers(None)?;
        let providers = self
            .document
            .entry(PROVIDER_TABLE)
            .or_insert_with(|| Item::Table(implicit_table()));
        let providers = providers
            .as_table_like_mut()
            .ok_or_else(|| ConfigError::NotATable {
                path: self.path.clone(),
                table: PROVIDER_TABLE.to_owned(),
            })?;
        let table = providers
            .entry(provider)
            .or_insert_with(|| Item::Table(Table::new()));
        let table = table
            .as_table_like_mut()
            .ok_or_else(|| ConfigError::NotATable {
                path: self.path.clone(),
                table: format!("{PROVIDER_TABLE}.{provider}"),
            })?;
        table.insert(name, value(setting));
        self.write()
    }

    /// Stores one daemon-validated browser-cookie source in one staged config write.
    ///
    /// Browser candidates travel over D-Bus as opaque `browser` or `browser/profile` paths.
    /// The daemon resolves and validates that path before it reaches this method; config only
    /// turns it into the stable fields a provider rebuild consumes. Keeping all three fields
    /// in this transaction prevents an old profile from surviving a newly selected browser.
    pub fn set_auth_selection(
        &mut self,
        provider: &str,
        selection: &AuthSelection,
    ) -> Result<(), ConfigError> {
        let browser_profile = match (selection.mode.as_str(), selection.candidate.as_deref()) {
            ("cursor-app", None) => None,
            ("browser", Some(candidate)) => {
                Some(parse_browser_candidate(candidate).ok_or_else(|| {
                    ConfigError::InvalidAuthSelection {
                        path: self.path.clone(),
                        provider: provider.to_owned(),
                        reason: "browser candidates must name a browser and optional profile"
                            .into(),
                    }
                })?)
            }
            ("cursor-app", Some(_)) => {
                return Err(ConfigError::InvalidAuthSelection {
                    path: self.path.clone(),
                    provider: provider.to_owned(),
                    reason: "Cursor App does not take a browser candidate".into(),
                });
            }
            ("browser", None) => {
                return Err(ConfigError::InvalidAuthSelection {
                    path: self.path.clone(),
                    provider: provider.to_owned(),
                    reason: "Browser needs a selected candidate".into(),
                });
            }
            _ => {
                return Err(ConfigError::InvalidAuthSelection {
                    path: self.path.clone(),
                    provider: provider.to_owned(),
                    reason: "unknown authentication mode".into(),
                });
            }
        };

        self.normalize_providers(None)?;
        let providers = self
            .document
            .entry(PROVIDER_TABLE)
            .or_insert_with(|| Item::Table(implicit_table()));
        let providers = providers
            .as_table_like_mut()
            .ok_or_else(|| ConfigError::NotATable {
                path: self.path.clone(),
                table: PROVIDER_TABLE.to_owned(),
            })?;
        let table = providers
            .entry(provider)
            .or_insert_with(|| Item::Table(Table::new()));
        let table = table
            .as_table_like_mut()
            .ok_or_else(|| ConfigError::NotATable {
                path: self.path.clone(),
                table: format!("{PROVIDER_TABLE}.{provider}"),
            })?;
        table.insert(AUTH_SOURCE_KEY, value(selection.mode.as_str()));
        match browser_profile {
            Some((browser, profile)) => {
                table.insert(AUTH_BROWSER_KEY, value(browser));
                if let Some(profile) = profile {
                    table.insert(AUTH_PROFILE_KEY, value(profile));
                } else {
                    table.remove(AUTH_PROFILE_KEY);
                }
            }
            None => {
                table.remove(AUTH_BROWSER_KEY);
                table.remove(AUTH_PROFILE_KEY);
            }
        }
        self.write()
    }

    /// Window keys of this provider whose notifications the user has switched on.
    ///
    /// Absent table, absent key and empty array all mean the same thing: this provider
    /// notifies about nothing. Notifications are opted into per window, so silence is the
    /// state a fresh installation is in — see `CONTEXT.md` § Notifications.
    pub fn notify_windows(&self, provider: &str) -> Result<Vec<String>, ConfigError> {
        let Some(item) = self
            .document
            .get(NOTIFY_TABLE)
            .and_then(|table| table.get(provider))
            .and_then(|table| table.get(NOTIFY_WINDOWS_KEY))
        else {
            return Ok(Vec::new());
        };
        let array = item.as_array().ok_or_else(|| ConfigError::InvalidNotify {
            path: self.path.clone(),
            provider: provider.to_owned(),
        })?;
        let mut seen = std::collections::BTreeSet::new();
        let mut windows = Vec::new();
        for entry in array.iter() {
            let key = entry.as_str().ok_or_else(|| ConfigError::InvalidNotify {
                path: self.path.clone(),
                provider: provider.to_owned(),
            })?;
            if seen.insert(key.to_owned()) {
                windows.push(key.to_owned());
            }
        }
        Ok(windows)
    }

    /// Switches one window's notifications on or off and writes the file.
    ///
    /// The list is rewritten from the validated set rather than edited in place: it holds
    /// opaque window keys with no decoration worth preserving, unlike the `providers`
    /// array a person is expected to read.
    pub fn set_window_notify(
        &mut self,
        provider: &str,
        window: &str,
        enabled: bool,
    ) -> Result<(), ConfigError> {
        let mut windows = self.notify_windows(provider)?;
        let held = windows.iter().any(|held| held == window);
        match (enabled, held) {
            (true, false) => windows.push(window.to_owned()),
            (false, true) => windows.retain(|held| held != window),
            _ => return Ok(()),
        }

        let notify = self
            .document
            .entry(NOTIFY_TABLE)
            .or_insert_with(|| Item::Table(implicit_table()));
        let notify = notify
            .as_table_like_mut()
            .ok_or_else(|| ConfigError::NotATable {
                path: self.path.clone(),
                table: NOTIFY_TABLE.to_owned(),
            })?;
        let table = notify
            .entry(provider)
            .or_insert_with(|| Item::Table(Table::new()));
        let table = table
            .as_table_like_mut()
            .ok_or_else(|| ConfigError::NotATable {
                path: self.path.clone(),
                table: format!("{NOTIFY_TABLE}.{provider}"),
            })?;
        let entry = table
            .entry(NOTIFY_WINDOWS_KEY)
            .or_insert_with(|| Item::Value(Array::new().into()));
        let array = entry
            .as_array_mut()
            .ok_or_else(|| ConfigError::InvalidNotify {
                path: self.path.clone(),
                provider: provider.to_owned(),
            })?;
        // Edited rather than replaced, for the same reason the whole file is: a comment
        // somebody wrote on this line is theirs, not ours to drop.
        if enabled {
            array.push(window);
        } else {
            let removed: Vec<usize> = array
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.as_str() == Some(window))
                .map(|(index, _)| index)
                .collect();
            for (already, index) in removed.into_iter().enumerate() {
                remove_carrying_prefix(array, index - already);
            }
        }
        self.write()
    }

    /// Normalizes the editable provider array without replacing it, retaining every
    /// surviving value's decoration and the array's own prefix and suffix.
    ///
    /// When `remove` is present, every occurrence of that slug is removed as part of the
    /// same index pass. All entries are validated before the first edit, so an
    /// array containing a non-string is still refused rather than partly repaired.
    fn normalize_providers(&mut self, remove: Option<&str>) -> Result<bool, ConfigError> {
        let Some(item) = self.document.get_mut(PROVIDERS_KEY) else {
            return Ok(false);
        };
        let array = item
            .as_array_mut()
            .ok_or_else(|| ConfigError::InvalidProviders {
                path: self.path.clone(),
                reason: "providers must be an array of strings".to_owned(),
            })?;
        let mut seen = std::collections::BTreeSet::new();
        let mut remove_indices = Vec::new();
        for (index, item) in array.iter().enumerate() {
            let slug = item.as_str().ok_or_else(|| ConfigError::InvalidProviders {
                path: self.path.clone(),
                reason: "every providers entry must be a string".to_owned(),
            })?;
            if remove == Some(slug) || !seen.insert(slug.to_owned()) {
                remove_indices.push(index);
            }
        }
        let changed = !remove_indices.is_empty();
        for (removed, index) in remove_indices.into_iter().enumerate() {
            remove_carrying_prefix(array, index - removed);
        }
        Ok(changed)
    }

    fn write(&self) -> Result<(), ConfigError> {
        let parent = self.path.parent().unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let staged = self.path.with_extension("toml.new");
        std::fs::write(&staged, self.document.to_string()).map_err(|source| ConfigError::Io {
            path: staged.clone(),
            source,
        })?;
        std::fs::rename(&staged, &self.path).map_err(|source| ConfigError::Io {
            path: self.path.clone(),
            source,
        })
    }
}

/// Removes one array element and hands its leading decoration to whatever follows it.
///
/// An element's prefix carries the line break and indentation that put it on its own line,
/// and any comment written above it. Dropping the element without passing that on collapses
/// the rest of the array onto one line, or strands a comment against the closing bracket.
fn remove_carrying_prefix(array: &mut Array, index: usize) {
    let prefix = array
        .get(index)
        .and_then(|value| value.decor().prefix())
        .cloned();
    array.remove(index);
    let Some(prefix) = prefix else {
        return;
    };
    if let Some(next) = array.get_mut(index) {
        next.decor_mut().set_prefix(prefix);
    } else {
        array.set_trailing(prefix);
    }
}

/// Splits the opaque browser candidate path used by browser-cookie source selection.
///
/// Browser slugs and profile directory names cannot contain `/`, so one separator retains
/// both durable components while a bare browser path represents the common parent choice.
fn parse_browser_candidate(candidate: &str) -> Option<(&str, Option<&str>)> {
    let (browser, profile) = match candidate.split_once('/') {
        Some((browser, profile)) => (browser, Some(profile)),
        None => (candidate, None),
    };
    (!browser.is_empty() && profile.is_none_or(|profile| !profile.is_empty()))
        .then_some((browser, profile))
}

/// Appends without moving the previous last element's inline comment onto the new value.
fn push_provider(array: &mut Array, provider: &str) {
    let trailing = array.trailing().as_str().map(str::to_owned);
    let item_indent = array
        .get(array.len().saturating_sub(1))
        .and_then(|value| value.decor().prefix())
        .and_then(|prefix| prefix.as_str())
        .and_then(|prefix| prefix.rsplit_once('\n').map(|(_, indent)| indent))
        .map(str::to_owned);
    let Some((before_closing_indent, closing_indent)) = trailing
        .as_deref()
        .and_then(|trailing| trailing.rsplit_once('\n'))
    else {
        array.push(provider);
        return;
    };

    let line_ending = if before_closing_indent.ends_with('\r') {
        "\r\n"
    } else {
        "\n"
    };
    let before_newline = before_closing_indent
        .strip_suffix('\r')
        .unwrap_or(before_closing_indent);
    let mut value = Value::from(provider);
    value.decor_mut().set_prefix(format!(
        "{before_newline}{line_ending}{}",
        item_indent.as_deref().unwrap_or(closing_indent)
    ));
    array.set_trailing(format!("{line_ending}{closing_indent}"));
    array.push_formatted(value);
}

/// A `[provider]` header that only appears once something is filed under it, so a config
/// with one Z.ai setting in it reads as `[provider.zai]` rather than as two headers.
fn implicit_table() -> Table {
    let mut table = Table::new();
    table.set_implicit(true);
    table
}

#[cfg(test)]
mod tests {
    use super::*;
    use tidemark_types::AuthSelection;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("tidemark-config-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch directory");
        dir.join("config.toml")
    }

    #[test]
    fn a_first_run_has_no_file_and_no_settings() {
        let config = Config::at(scratch("absent").with_file_name("nothing.toml"))
            .expect("a missing file is an empty document");
        assert_eq!(config.option("zai", "region"), None);
    }

    #[test]
    fn an_auth_selection_replaces_only_its_stale_browser_fields() {
        // Replacing this with individual option writes could leave an old profile paired
        // with a newly chosen browser if the daemon stopped between them.
        let path = scratch("auth-selection");
        std::fs::write(
            &path,
            "[provider.cursor]\n\
             auth-source = \"browser\"\n\
             auth-browser = \"firefox\"\n\
             auth-profile = \"Default\"\n",
        )
        .expect("seed");
        let mut config = Config::at(path.clone()).expect("parses");

        config
            .set_auth_selection(
                "cursor",
                &AuthSelection {
                    mode: "cursor-app".into(),
                    candidate: None,
                },
            )
            .expect("stores Cursor App");
        assert_eq!(config.option("cursor", "auth-source"), Some("cursor-app"));
        assert_eq!(config.option("cursor", "auth-browser"), None);
        assert_eq!(config.option("cursor", "auth-profile"), None);

        config
            .set_auth_selection(
                "cursor",
                &AuthSelection {
                    mode: "browser".into(),
                    candidate: Some("zen".into()),
                },
            )
            .expect("stores a browser parent");
        assert_eq!(config.option("cursor", "auth-browser"), Some("zen"));
        assert_eq!(config.option("cursor", "auth-profile"), None);

        config
            .set_auth_selection(
                "cursor",
                &AuthSelection {
                    mode: "browser".into(),
                    candidate: Some("firefox/work".into()),
                },
            )
            .expect("stores a browser profile");
        assert_eq!(config.option("cursor", "auth-browser"), Some("firefox"));
        assert_eq!(config.option("cursor", "auth-profile"), Some("work"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn the_raw_response_log_is_off_until_the_file_asks_for_it() {
        let path = scratch("debug-absent");
        let _ = std::fs::remove_file(&path);
        let config = Config::at(path.clone()).expect("missing is valid");
        assert!(!config.debug_raw_responses().expect("readable"));

        std::fs::write(&path, "[debug]\nraw_responses = true\n").expect("seed");
        let config = Config::at(path.clone()).expect("parses");
        assert!(config.debug_raw_responses().expect("readable"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_raw_response_switch_that_is_not_a_boolean_is_refused_rather_than_ignored() {
        // Silently defaulting would leave someone collecting evidence for a bug report
        // from a log that was never opened.
        let path = scratch("debug-not-a-bool");
        std::fs::write(&path, "[debug]\nraw_responses = \"yes\"\n").expect("seed");
        let config = Config::at(path.clone()).expect("parses");
        assert!(matches!(
            config.debug_raw_responses(),
            Err(ConfigError::InvalidPreference {
                table: DEBUG_TABLE,
                key: RAW_RESPONSES_KEY,
                ..
            })
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_first_run_has_no_configured_providers() {
        let config = Config::at(scratch("providers-absent")).expect("missing is valid");
        assert_eq!(config.providers().expect("readable"), Vec::<String>::new());
    }

    #[test]
    fn configured_providers_keep_their_order_and_first_duplicate() {
        let path = scratch("providers-order");
        std::fs::write(&path, "providers = [\"claude\", \"zai\", \"claude\"]\n").expect("seed");
        let config = Config::at(path.clone()).expect("parses");
        assert_eq!(config.providers().expect("readable"), ["claude", "zai"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn adding_normalizes_duplicate_providers_without_losing_array_comments() {
        let path = scratch("providers-add-normalizes");
        std::fs::write(
            &path,
            "# root comment\n\
             providers = [ # array heading\n\
                 \"claude\", # first survivor\n\
                 \"zai\", # second survivor\n\
                 \"claude\", # duplicate\n\
             ] # array tail\n",
        )
        .expect("seed");
        let mut config = Config::at(path.clone()).expect("parses");

        assert!(config.add_provider("kimi").expect("added"));

        let text = std::fs::read_to_string(&path).expect("written");
        assert_eq!(text.matches("\"claude\"").count(), 1, "{text}");
        assert!(text.contains("# root comment"), "{text}");
        assert!(text.contains("# array heading"), "{text}");
        assert!(text.contains("\"claude\", # first survivor"), "{text}");
        assert!(text.contains("\"zai\", # second survivor"), "{text}");
        assert!(text.contains("# array tail"), "{text}");
        assert!(text.contains("\"kimi\""), "{text}");
        let reread = Config::at(path.clone()).expect("parses again");
        assert_eq!(
            reread.providers().expect("readable"),
            ["claude", "zai", "kimi"]
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn removing_normalizes_in_place_and_preserves_survivor_comments() {
        let path = scratch("providers-remove-normalizes");
        std::fs::write(
            &path,
            "providers = [ # array heading\n\
                 \"claude\", # first survivor\n\
                 \"zai\", # removed target\n\
                 \"claude\", # duplicate\n\
                 \"future\", # unknown survivor\n\
                 \"zai\", # duplicate target\n\
             ] # array tail\n\
             \n\
             [provider.zai]\n\
             region = \"global\"\n",
        )
        .expect("seed");
        let mut config = Config::at(path.clone()).expect("parses");

        assert!(config.remove_provider("zai").expect("removed"));

        let text = std::fs::read_to_string(&path).expect("written");
        assert_eq!(text.matches("\"claude\"").count(), 1, "{text}");
        assert!(!text.contains("\"zai\""), "{text}");
        assert!(text.contains("# array heading"), "{text}");
        assert!(text.contains("\"claude\", # first survivor"), "{text}");
        assert!(text.contains("\"future\", # unknown survivor"), "{text}");
        assert!(text.contains("# array tail"), "{text}");
        assert!(!text.contains("[provider.zai]"), "{text}");
        let reread = Config::at(path.clone()).expect("parses again");
        assert_eq!(reread.providers().expect("readable"), ["claude", "future"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reordering_moves_only_the_slugs_and_leaves_the_layout_alone() {
        let path = scratch("providers-reorder-layout");
        std::fs::write(
            &path,
            "# root comment\n\
             providers = [ # array heading\n\
                 \"claude\",\n\
                 \"zai\",\n\
                 \"kimi\",\n\
             ] # array tail\n\
             \n\
             [provider.zai]\n\
             region = \"global\"\n",
        )
        .expect("seed");
        let mut config = Config::at(path.clone()).expect("parses");

        assert!(
            config
                .set_provider_order(&["kimi".into(), "claude".into(), "zai".into()])
                .expect("reordered")
        );

        let text = std::fs::read_to_string(&path).expect("written");
        assert_eq!(
            text,
            "# root comment\n\
             providers = [ # array heading\n\
                 \"kimi\",\n\
                 \"claude\",\n\
                 \"zai\",\n\
             ] # array tail\n\
             \n\
             [provider.zai]\n\
             region = \"global\"\n",
            "the layout, the comments and the provider table must all survive"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reordering_a_single_line_array_keeps_it_on_one_line() {
        let path = scratch("providers-reorder-inline");
        std::fs::write(&path, "providers = [\"claude\", \"zai\"]\n").expect("seed");
        let mut config = Config::at(path.clone()).expect("parses");

        assert!(
            config
                .set_provider_order(&["zai".into(), "claude".into()])
                .expect("reordered")
        );

        let text = std::fs::read_to_string(&path).expect("written");
        assert_eq!(text, "providers = [\"zai\", \"claude\"]\n");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reordering_normalizes_duplicates_before_permuting() {
        let path = scratch("providers-reorder-duplicates");
        std::fs::write(&path, "providers = [\"claude\", \"zai\", \"claude\"]\n").expect("seed");
        let mut config = Config::at(path.clone()).expect("parses");

        assert!(
            config
                .set_provider_order(&["zai".into(), "claude".into()])
                .expect("reordered")
        );

        let reread = Config::at(path.clone()).expect("parses again");
        assert_eq!(reread.providers().expect("readable"), ["zai", "claude"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_unchanged_order_is_not_a_write() {
        let path = scratch("providers-reorder-noop");
        std::fs::write(&path, "providers = [\"claude\", \"zai\"]\n").expect("seed");
        let mut config = Config::at(path.clone()).expect("parses");

        assert!(
            !config
                .set_provider_order(&["claude".into(), "zai".into()])
                .expect("accepted")
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_order_that_is_not_a_permutation_is_refused_without_writing() {
        let path = scratch("providers-reorder-refused");
        let seed = "providers = [\"claude\", \"zai\"]\n";
        std::fs::write(&path, seed).expect("seed");
        let mut config = Config::at(path.clone()).expect("parses");

        for order in [
            vec!["claude".to_owned()],
            vec!["claude".to_owned(), "claude".to_owned()],
            vec!["claude".to_owned(), "zai".to_owned(), "kimi".to_owned()],
        ] {
            assert!(
                config.set_provider_order(&order).is_err(),
                "{order:?} is not a permutation of the configured set"
            );
        }
        assert_eq!(std::fs::read_to_string(&path).expect("readable"), seed);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn setting_an_option_normalizes_providers_without_losing_array_comments() {
        let path = scratch("providers-option-normalizes");
        std::fs::write(
            &path,
            "providers = [ # array heading\n\
                 \"zai\", # first survivor\n\
                 \"claude\", # other survivor\n\
                 \"zai\", # duplicate\n\
             ] # array tail\n\
             \n\
             [provider.zai]\n\
             region = \"global\"\n",
        )
        .expect("seed");
        let mut config = Config::at(path.clone()).expect("parses");

        config
            .set_option("zai", "region", "bigmodel-cn")
            .expect("setting written");

        let text = std::fs::read_to_string(&path).expect("written");
        assert_eq!(text.matches("\"zai\"").count(), 1, "{text}");
        assert!(text.contains("# array heading"), "{text}");
        assert!(text.contains("\"zai\", # first survivor"), "{text}");
        assert!(text.contains("\"claude\", # other survivor"), "{text}");
        assert!(text.contains("# array tail"), "{text}");
        assert!(text.contains("region = \"bigmodel-cn\""), "{text}");
        let reread = Config::at(path.clone()).expect("parses again");
        assert_eq!(reread.providers().expect("readable"), ["zai", "claude"]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_non_array_provider_list_is_refused_not_replaced() {
        let path = scratch("providers-wrong-type");
        std::fs::write(&path, "providers = \"claude\"\n").expect("seed");
        let config = Config::at(path.clone()).expect("valid TOML");
        assert!(matches!(
            config.providers(),
            Err(ConfigError::InvalidProviders { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(&path).expect("still there"),
            "providers = \"claude\"\n"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn adding_is_idempotent_and_removing_drops_only_that_provider_table() {
        let path = scratch("provider-mutations");
        std::fs::write(
            &path,
            "# owned by the user\nproviders = [\"claude\"]\n\n[provider.claude]\nfuture = \"gone with claude\"\n\n[unrelated]\nfuture = \"kept\"\n",
        )
        .expect("seed");
        let mut config = Config::at(path.clone()).expect("parses");
        assert!(config.add_provider("zai").expect("added"));
        assert!(!config.add_provider("zai").expect("duplicate is a no-op"));
        assert!(config.remove_provider("claude").expect("removed"));
        assert!(
            !config
                .remove_provider("claude")
                .expect("missing is a no-op")
        );

        let reread = Config::at(path.clone()).expect("parses again");
        assert_eq!(reread.providers().expect("readable"), ["zai"]);
        let text = std::fs::read_to_string(&path).expect("written");
        assert!(text.contains("# owned by the user"));
        assert!(text.contains("[unrelated]"));
        assert!(!text.contains("[provider.claude]"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_setting_survives_being_written_and_read_back() {
        let path = scratch("roundtrip");
        let _ = std::fs::remove_file(&path);
        let mut config = Config::at(path.clone()).expect("empty");
        config
            .set_option("zai", "region", "bigmodel-cn")
            .expect("writes");

        let reread = Config::at(path.clone()).expect("parses");
        assert_eq!(reread.option("zai", "region"), Some("bigmodel-cn"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn editing_one_value_leaves_the_rest_of_the_users_file_alone() {
        let path = scratch("preserve");
        std::fs::write(
            &path,
            "# hand written, and it stays that way\n\
             [provider.zai]\n\
             region = \"global\"\n\
             \n\
             [provider.kimi]\n\
             something-a-newer-build-knows = \"kept\"\n",
        )
        .expect("seed");

        let mut config = Config::at(path.clone()).expect("parses");
        config
            .set_option("zai", "region", "bigmodel-cn")
            .expect("writes");

        let text = std::fs::read_to_string(&path).expect("written");
        assert!(text.contains("# hand written"), "comment lost:\n{text}");
        assert!(
            text.contains("something-a-newer-build-knows = \"kept\""),
            "an unknown key was dropped:\n{text}"
        );
        assert!(text.contains("region = \"bigmodel-cn\""), "{text}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_broken_file_is_refused_rather_than_replaced() {
        let path = scratch("broken");
        std::fs::write(&path, "[provider.zai\nregion = ").expect("seed");
        let error = Config::at(path.clone()).expect_err("will not parse");
        assert!(matches!(error, ConfigError::Malformed { .. }), "{error}");
        assert!(
            std::fs::read_to_string(&path)
                .expect("still there")
                .starts_with("[provider.zai"),
            "the user's file must survive being unreadable to us"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_setting_that_is_not_a_string_is_absent_rather_than_guessed() {
        let path = scratch("wrongtype");
        std::fs::write(&path, "[provider.zai]\nregion = 4\n").expect("seed");
        let config = Config::at(path.clone()).expect("valid toml");
        assert_eq!(config.option("zai", "region"), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_provider_nobody_opted_in_for_notifies_about_nothing() {
        let config = Config::at(scratch("notify-absent").with_file_name("nothing.toml"))
            .expect("a missing file is an empty document");
        assert_eq!(
            config.notify_windows("claude").expect("read"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn switching_a_window_on_persists_it() {
        let path = scratch("notify-on");
        let _ = std::fs::remove_file(&path);
        let mut config = Config::at(path.clone()).expect("loaded");
        config
            .set_window_notify("claude", "w18000", true)
            .expect("written");

        let reread = Config::at(path).expect("reloaded");
        assert_eq!(
            reread.notify_windows("claude").expect("read"),
            vec!["w18000".to_owned()]
        );
    }

    #[test]
    fn switching_the_same_window_on_twice_lists_it_once() {
        let path = scratch("notify-twice");
        let _ = std::fs::remove_file(&path);
        let mut config = Config::at(path).expect("loaded");
        config
            .set_window_notify("claude", "w18000", true)
            .expect("written");
        config
            .set_window_notify("claude", "w18000", true)
            .expect("written again");
        assert_eq!(
            config.notify_windows("claude").expect("read"),
            vec!["w18000".to_owned()]
        );
    }

    #[test]
    fn switching_one_window_off_leaves_the_others_alone() {
        let path = scratch("notify-off");
        let _ = std::fs::remove_file(&path);
        let mut config = Config::at(path).expect("loaded");
        config
            .set_window_notify("claude", "w18000", true)
            .expect("written");
        config
            .set_window_notify("claude", "w604800", true)
            .expect("written");
        config
            .set_window_notify("claude", "w18000", false)
            .expect("written");
        assert_eq!(
            config.notify_windows("claude").expect("read"),
            vec!["w604800".to_owned()]
        );
    }

    #[test]
    fn one_provider_opt_in_says_nothing_about_another() {
        let path = scratch("notify-scoped");
        let _ = std::fs::remove_file(&path);
        let mut config = Config::at(path).expect("loaded");
        config
            .set_window_notify("claude", "w18000", true)
            .expect("written");
        assert_eq!(
            config.notify_windows("codex").expect("read"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn removing_a_provider_forgets_which_windows_it_notified_about() {
        let path = scratch("notify-removed");
        let _ = std::fs::remove_file(&path);
        let mut config = Config::at(path).expect("loaded");
        config.add_provider("claude").expect("added");
        config
            .set_window_notify("claude", "w18000", true)
            .expect("written");
        config.remove_provider("claude").expect("removed");
        assert_eq!(
            config.notify_windows("claude").expect("read"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_opt_in_list_that_is_not_strings_is_refused_rather_than_ignored() {
        let path = scratch("notify-wrongtype");
        std::fs::write(&path, "[notify.claude]\nwindows = [4]\n").expect("seeded");
        let config = Config::at(path).expect("valid TOML");
        assert!(config.notify_windows("claude").is_err());
    }

    #[test]
    fn a_comment_in_the_opt_in_table_survives_a_change() {
        let path = scratch("notify-comment");
        std::fs::write(
            &path,
            "# hand-written\n[notify.claude]\nwindows = [\"w18000\"] # the one that matters\n",
        )
        .expect("seeded");
        let mut config = Config::at(path.clone()).expect("loaded");
        config
            .set_window_notify("claude", "w604800", true)
            .expect("written");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(text.contains("# hand-written"), "{text}");
        assert!(text.contains("# the one that matters"), "{text}");
    }
    #[test]
    fn a_first_run_has_safe_application_preferences() {
        let path = scratch("preferences-defaults");
        let _ = std::fs::remove_file(&path);
        let config = Config::at(path).expect("loaded");

        assert_eq!(
            config.preferences().expect("readable"),
            tidemark_types::Preferences::default()
        );
        assert!(config.preferences().expect("readable").release_check);
        assert!(config.preferences().expect("readable").minimize_on_close);
        assert_eq!(config.preferences().expect("readable").startup_mode, "app");
        assert_eq!(
            config.preferences().expect("readable").history_retention,
            "forever"
        );
        assert_eq!(config.preferences().expect("readable").proxy_mode, "off");
        assert!(
            config
                .preferences()
                .expect("readable")
                .proxy_host
                .is_empty()
        );
        assert_eq!(config.preferences().expect("readable").proxy_port, 0);
    }

    #[test]
    fn application_preferences_survive_a_round_trip_without_rewriting_the_file() {
        let path = scratch("preferences-roundtrip");
        std::fs::write(&path, "# belongs to the user\nproviders = []\n").expect("seeded");
        let mut config = Config::at(path.clone()).expect("loaded");
        let preferences = tidemark_types::Preferences {
            release_check: false,
            minimize_on_close: false,
            startup_mode: "daemon".into(),
            history_retention: "six-months".into(),
            proxy_mode: "socks5".into(),
            proxy_host: "127.0.0.1".into(),
            proxy_port: 1080,
            refresh_mode: "auto".into(),
            refresh_minutes: 5,
        };

        config.set_release_check(false).expect("release setting");
        config.set_minimize_on_close(false).expect("close setting");
        config.set_startup_mode("daemon").expect("startup mode");
        config
            .set_history_retention("six-months")
            .expect("retention setting");
        config
            .set_proxy("socks5", "127.0.0.1", 1080)
            .expect("proxy setting");

        let reread = Config::at(path.clone()).expect("reloaded");
        assert_eq!(reread.preferences().expect("readable"), preferences);
        let text = std::fs::read_to_string(path).expect("read back");
        assert!(text.starts_with("# belongs to the user\n"), "{text}");
    }

    #[test]
    fn an_unknown_history_retention_is_refused() {
        let path = scratch("preferences-retention");
        std::fs::write(&path, "[history]\nretention = \"eventually\"\n").expect("seeded");
        let config = Config::at(path).expect("valid TOML");

        assert!(config.preferences().is_err());
    }

    #[test]
    fn an_unknown_startup_mode_is_refused() {
        let path = scratch("preferences-startup");
        std::fs::write(&path, "[general]\nstartup = \"everything\"\n").expect("seeded");
        let config = Config::at(path).expect("valid TOML");

        assert!(config.preferences().is_err());
    }

    #[test]
    fn an_unknown_proxy_mode_is_refused_reading_and_writing() {
        let path = scratch("preferences-proxy-mode");
        std::fs::write(&path, "[proxy]\nmode = \"socks4\"\n").expect("seeded");
        let mut config = Config::at(path).expect("valid TOML");

        assert!(config.preferences().is_err());
        assert!(config.set_proxy("gopher", "127.0.0.1", 1080).is_err());
    }

    #[test]
    fn a_proxy_port_outside_the_range_is_refused_rather_than_clamped() {
        let path = scratch("preferences-proxy-port");
        std::fs::write(&path, "[proxy]\nmode = \"http\"\nport = 70000\n").expect("seeded");
        let config = Config::at(path).expect("valid TOML");

        assert!(config.preferences().is_err());
    }
}
