//! Installed plugin definitions, on disk and in memory.
//!
//! Two rules shape this. **Import is transactional**: a file is validated, and only then are
//! its exact bytes written, staged-and-renamed, so a rejected replacement leaves the previous
//! definition installed and pollable. **The id is the storage key**: the file is named from
//! it, accounts and keys hang off it, and a definition with configured accounts cannot be
//! removed — the user removes the accounts first, with the removal semantics that already
//! delete their credentials.
//!
//! The original validated bytes are stored unchanged, so a definition stays inspectable and
//! exportable; only the sanitized mark is materialized separately, because the icon theme
//! loads marks from files by name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tidemark_core::plugin::{Definition, PluginError, lua, schema};

/// The extension every installed definition is filed under.
const EXTENSION: &str = "tidemark-provider";
/// Where a materialized mark goes inside the store's icon-theme root, relative to `icons`.
const MARK_DIR: &str = "hicolor/symbolic/apps";

/// A failure of the store itself, as opposed to a failure of the file it was handed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A definition still has accounts configured against it.
    #[error("{id} still has {accounts} account(s) configured")]
    InUse {
        /// The definition that was not removed.
        id: String,
        /// How many accounts are still using it.
        accounts: usize,
    },
    /// The store has no directory: an engine that was never given one.
    #[error("this daemon has no plugin directory")]
    Detached,
    /// The store's own directory or one of its files could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// What was being read or written.
        path: PathBuf,
        /// The underlying failure.
        source: std::io::Error,
    },
    /// The file the store was handed is not a usable definition.
    #[error(transparent)]
    Plugin(#[from] PluginError),
}

/// The installed definitions, held open for the daemon's lifetime.
#[derive(Debug)]
pub struct Store {
    /// Absent for a store nothing may be written to. An engine built without one has this,
    /// so a daemon misconfigured at startup refuses an import in one sentence rather than
    /// writing a definition into a directory nobody chose.
    root: Option<PathBuf>,
    installed: BTreeMap<String, Arc<Definition>>,
}

impl Store {
    /// Reads every definition already on disk.
    ///
    /// A file that no longer validates is logged and skipped rather than failing the start:
    /// one plugin written against a future format must not stop the daemon polling the
    /// built-in providers, or the other plugins.
    pub fn open(root: PathBuf) -> Result<Self, StoreError> {
        let mut installed = BTreeMap::new();
        match std::fs::read_dir(&root) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|source| StoreError::Io {
                        path: root.clone(),
                        source,
                    })?;
                    let path = entry.path();
                    if path.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
                        continue;
                    }
                    match Self::read(&path) {
                        Ok(definition) => {
                            installed.insert(definition.id.clone(), Arc::new(definition));
                        }
                        Err(error) => {
                            tracing::warn!(
                                path = %path.display(),
                                %error,
                                "skipping an unreadable plugin definition"
                            );
                        }
                    }
                }
            }
            // A store nobody has installed into yet is empty, not broken.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(StoreError::Io { path: root, source });
            }
        }
        Ok(Self {
            root: Some(root),
            installed,
        })
    }

    /// A store with nowhere to write, and nothing in it.
    ///
    /// What an [`crate::engine::Engine`] holds until one is attached, so a test that says
    /// nothing about plugins does not have to name a directory.
    pub fn detached() -> Self {
        Self {
            root: None,
            installed: BTreeMap::new(),
        }
    }

    /// The directory this store writes to.
    fn root(&self) -> Result<&Path, StoreError> {
        self.root.as_deref().ok_or(StoreError::Detached)
    }

    /// Validates a file without storing it: the import preview, and a dry run in every sense.
    ///
    /// The parser is compiled here as well as parsed, so a chunk with a syntax error is
    /// refused while the user is looking at the import dialog rather than silently at the
    /// first poll, hours later, as a provider failure.
    pub fn inspect(&self, bytes: &[u8], reserved: &[&str]) -> Result<Definition, PluginError> {
        let definition = schema::parse(bytes, reserved)?;
        lua::compile(&definition.lua_source)?;
        Ok(definition)
    }

    /// Validates a file and, only then, stores it under its own id.
    ///
    /// A definition already installed under that id is replaced. Because validation runs
    /// first and the bytes are staged and renamed, a replacement that does not validate
    /// leaves the previous definition installed, on disk and in the map.
    pub fn install(&mut self, bytes: &[u8], reserved: &[&str]) -> Result<Definition, StoreError> {
        let definition = self.inspect(bytes, reserved)?;
        self.write(&self.definition_path(&definition.id)?, bytes)?;
        // The mark is written after the definition, so a failure here leaves an installed
        // definition with no mark — which renders as a provider without an icon, rather
        // than as an icon with no provider.
        if let (Some(svg), Some(path)) = (&definition.icon_svg, self.mark_path(&definition.id)?) {
            self.write(&path, svg.as_bytes())?;
        }
        self.installed
            .insert(definition.id.clone(), Arc::new(definition.clone()));
        Ok(definition)
    }

    /// Every installed definition, in id order.
    pub fn installed(&self) -> Vec<Arc<Definition>> {
        self.installed.values().cloned().collect()
    }

    /// One installed definition.
    pub fn get(&self, id: &str) -> Option<Arc<Definition>> {
        self.installed.get(id).cloned()
    }

    /// Removes a definition, its stored bytes and its mark.
    ///
    /// `configured_accounts` is how many accounts the caller's config still lists against
    /// this id. A definition that is still in use is refused rather than removed: removing it
    /// would leave accounts naming a provider that no longer exists, with their keys still in
    /// the keyring. The user removes the accounts first, which already deletes those keys.
    pub fn remove(&mut self, id: &str, configured_accounts: usize) -> Result<(), StoreError> {
        if configured_accounts > 0 {
            return Err(StoreError::InUse {
                id: id.to_owned(),
                accounts: configured_accounts,
            });
        }
        Self::forget(&self.definition_path(id)?)?;
        if let Some(path) = self.mark_path(id)? {
            Self::forget(&path)?;
        }
        self.installed.remove(id);
        Ok(())
    }

    /// Reads and validates one stored file.
    fn read(path: &Path) -> Result<Definition, StoreError> {
        let bytes = std::fs::read(path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        // Nothing is reserved against a file already on disk: the id it was installed under
        // is the id it keeps, and a built-in taking that name later is a collision the
        // registry resolves in favour of the built-in rather than a reason to lose the file.
        let definition = schema::parse(&bytes, &[])?;
        lua::compile(&definition.lua_source)?;
        Ok(definition)
    }

    /// Where one definition's bytes live.
    fn definition_path(&self, id: &str) -> Result<PathBuf, StoreError> {
        Ok(self.root()?.join(format!("{id}.{EXTENSION}")))
    }

    /// Where one definition's mark is materialized, when its id names a usable slug.
    fn mark_path(&self, id: &str) -> Result<Option<PathBuf>, StoreError> {
        let root = self.root()?;
        Ok(tidemark_types::plugin_icon_slug(id)
            .and_then(|slug| tidemark_types::icon_name(&slug))
            .map(|name| {
                root.join("icons")
                    .join(MARK_DIR)
                    .join(format!("{name}.svg"))
            }))
    }

    /// Stages a file beside its destination and renames it into place, so a daemon killed
    /// mid-write leaves the previous file intact rather than a truncated one.
    fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), StoreError> {
        let parent = path.parent().unwrap_or(self.root()?);
        std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let staged = path.with_extension("tmp");
        std::fs::write(&staged, bytes).map_err(|source| StoreError::Io {
            path: staged.clone(),
            source,
        })?;
        std::fs::rename(&staged, path).map_err(|source| StoreError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Removes a file, treating one that is already gone as removed.
    fn forget(path: &Path) -> Result<(), StoreError> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(StoreError::Io {
                path: path.to_path_buf(),
                source,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Task 5's minimal file, plus a mark, so the store's icon materialization is covered.
    const FILE: &str = r#"
format_version = 1

[provider]
id = "com.acme.quota"
name = "Acme AI"
plugin_version = "1.0.0"

[request]
method = "GET"
api_key_header = "X-Acme-Key"
api_key_prefix = ""

[parser]
language = "lua54"
source = '''
function parse(response, context)
    return { metrics = {}, card = {}, details = {} }
end
'''

[icon]
svg = '''
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><path fill="currentColor" d="M0 0h64v64H0z"/></svg>
'''
"#;

    /// A store over its own scratch root. The workspace depends on no temp-directory crate —
    /// its tests build paths under `std::env::temp_dir()` with the process id in the name — so
    /// this does the same rather than adding one.
    fn store(name: &str) -> (PathBuf, Store) {
        let root =
            std::env::temp_dir().join(format!("tidemark-plugins-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch root");
        (root.clone(), Store::open(root).expect("opens"))
    }

    /// Where a definition's materialized mark is expected to land.
    fn mark_path(root: &Path) -> PathBuf {
        root.join("icons/hicolor/symbolic/apps/tidemark-com-acme-quota-symbolic.svg")
    }

    #[test]
    fn inspecting_a_file_validates_it_and_writes_nothing() {
        let (root, store) = store("inspect");
        let definition = store.inspect(FILE.as_bytes(), &[]).expect("valid");
        assert_eq!(definition.id, "com.acme.quota");
        assert_eq!(
            std::fs::read_dir(&root).expect("readable").count(),
            0,
            "inspection is a dry run: nothing is stored and nothing is polled"
        );
    }

    #[test]
    fn installing_writes_the_exact_validated_bytes_under_the_stable_id() {
        let (root, mut store) = store("install");
        store.install(FILE.as_bytes(), &[]).expect("installs");
        let path = root.join("com.acme.quota.tidemark-provider");
        assert_eq!(std::fs::read(&path).expect("stored"), FILE.as_bytes());
        assert_eq!(store.installed().len(), 1);
        assert!(store.get("com.acme.quota").is_some());
    }

    #[test]
    fn a_rejected_replacement_leaves_the_installed_definition_untouched() {
        let (_root, mut store) = store("rejected-replacement");
        store.install(FILE.as_bytes(), &[]).expect("installs");
        let broken = FILE.replace("method = \"GET\"", "method = \"PATCH\"");
        assert!(store.install(broken.as_bytes(), &[]).is_err());
        let kept = store.get("com.acme.quota").expect("still installed");
        assert_eq!(kept.method.as_str(), "GET", "import is transactional");
    }

    #[test]
    fn replacing_a_definition_keeps_its_id_and_replaces_its_bytes() {
        let (_root, mut store) = store("replacement");
        store.install(FILE.as_bytes(), &[]).expect("installs");
        let updated = FILE.replace("plugin_version = \"1.0.0\"", "plugin_version = \"1.1.0\"");
        store.install(updated.as_bytes(), &[]).expect("replaces");
        assert_eq!(
            store.installed().len(),
            1,
            "a replacement is not a second provider"
        );
        assert_eq!(store.get("com.acme.quota").unwrap().plugin_version, "1.1.0");
    }

    #[test]
    fn a_definition_with_accounts_configured_cannot_be_removed() {
        let (_root, mut store) = store("in-use");
        store.install(FILE.as_bytes(), &[]).expect("installs");
        assert!(matches!(
            store.remove("com.acme.quota", 2),
            Err(StoreError::InUse { accounts: 2, .. })
        ));
        assert!(
            store.get("com.acme.quota").is_some(),
            "the refusal changed nothing"
        );
        store
            .remove("com.acme.quota", 0)
            .expect("removable once no account uses it");
        assert!(store.get("com.acme.quota").is_none());
    }

    #[test]
    fn a_parser_that_does_not_compile_is_refused_at_import_rather_than_at_the_first_poll() {
        let (_root, mut store) = store("compile");
        let broken = FILE.replace("function parse(response, context)", "function parse( then");
        assert!(matches!(
            store.inspect(broken.as_bytes(), &[]),
            Err(PluginError::LuaCompile { .. })
        ));
        assert!(store.install(broken.as_bytes(), &[]).is_err());
        assert!(
            store.get("com.acme.quota").is_none(),
            "and nothing was stored"
        );
    }

    #[test]
    fn a_reserved_or_colliding_id_is_refused_at_install_time_too() {
        let (_root, mut store) = store("reserved");
        assert!(store.install(FILE.as_bytes(), &["com.acme.quota"]).is_err());
    }

    #[test]
    fn opening_a_store_reads_every_installed_definition_and_skips_the_unreadable_ones() {
        let (root, mut store) = store("reopen");
        store.install(FILE.as_bytes(), &[]).expect("installs");
        std::fs::write(root.join("junk.tidemark-provider"), b"not toml").expect("written");
        std::fs::write(root.join("notes.txt"), b"ignored").expect("written");
        let reopened = Store::open(root.clone()).expect("opens");
        assert_eq!(
            reopened.installed().len(),
            1,
            "one bad file must not stop the daemon reading the good ones"
        );
    }

    #[test]
    fn a_definitions_mark_is_materialized_where_the_icon_theme_finds_it() {
        let (root, mut store) = store("mark");
        store.install(FILE.as_bytes(), &[]).expect("installs");
        assert!(
            mark_path(&root).exists(),
            "the GUI loads plugin marks through the icon theme, by name"
        );
    }

    #[test]
    fn removing_a_definition_removes_its_mark() {
        let (root, mut store) = store("mark-removal");
        store.install(FILE.as_bytes(), &[]).expect("installs");
        store.remove("com.acme.quota", 0).expect("removes");
        assert!(!mark_path(&root).exists());
    }
}
