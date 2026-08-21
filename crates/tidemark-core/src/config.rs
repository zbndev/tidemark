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

use toml_edit::{Array, DocumentMut, Item, Table, Value, value};

use crate::paths;

/// Table every provider's own settings live under: `[provider.zai]`.
const PROVIDER_TABLE: &str = "provider";
const PROVIDERS_KEY: &str = "providers";

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

    /// Removes a provider and its settings while normalizing any survivor duplicates.
    pub fn remove_provider(&mut self, provider: &str) -> Result<bool, ConfigError> {
        let providers = self.providers()?;
        let configured = providers.iter().any(|candidate| candidate == provider);

        if configured && let Some(item) = self.document.get_mut(PROVIDER_TABLE) {
            let table = item
                .as_table_like_mut()
                .ok_or_else(|| ConfigError::NotATable {
                    path: self.path.clone(),
                    table: PROVIDER_TABLE.to_owned(),
                })?;
            table.remove(provider);
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
            let index = index - removed;
            let prefix = array
                .get(index)
                .and_then(|value| value.decor().prefix())
                .cloned();
            array.remove(index);
            if let Some(prefix) = prefix {
                if let Some(next) = array.get_mut(index) {
                    next.decor_mut().set_prefix(prefix);
                } else {
                    array.set_trailing(prefix);
                }
            }
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
}
