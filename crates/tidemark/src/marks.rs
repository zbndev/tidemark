//! Provider logos. Slint has no icon theme to ask, so this looks in the
//! same places the theme would — the plugin marks the daemon materializes, the installed
//! hicolor theme (on Windows, `share\icons` beside the executable), and the source tree in
//! a development build — and colours the symbolic SVG itself in the markup.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tidemark_types::present::provider_icon_name;

#[derive(Debug, Default)]
pub struct Marks {
    plugin_root: RefCell<Option<PathBuf>>,
    cache: RefCell<HashMap<String, Option<slint::Image>>>,
}

impl Marks {
    /// Adds the daemon's plugin icon root and forgets every earlier miss, so a mark
    /// installed while the window is open is found on the next redraw.
    pub fn set_plugin_root(&self, root: &str) {
        let root = (!root.is_empty()).then(|| PathBuf::from(root));
        self.plugin_root.replace(root);
        self.forget();
    }

    pub fn forget(&self) {
        self.cache.borrow_mut().clear();
    }

    pub fn get(&self, provider: &str) -> Option<slint::Image> {
        if let Some(found) = self.cache.borrow().get(provider) {
            return found.clone();
        }
        let found = provider_icon_name(provider)
            .and_then(|name| self.find(&name))
            .and_then(|path| slint::Image::load_from_path(&path).ok());
        self.cache
            .borrow_mut()
            .insert(provider.to_owned(), found.clone());
        found
    }

    fn find(&self, name: &str) -> Option<PathBuf> {
        let file = format!("{name}.svg");
        let mut roots: Vec<PathBuf> = Vec::new();
        roots.extend(self.plugin_root.borrow().clone());
        if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
            roots.push(Path::new(&data).join("icons"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            roots.push(Path::new(&home).join(".local/share/icons"));
        }
        // Beside the executable on Windows; the prefix it is installed under elsewhere,
        // which is the only place a Nix store path keeps them.
        if let Ok(exe) = std::env::current_exe() {
            roots.extend(
                exe.ancestors()
                    .skip(1)
                    .take(2)
                    .map(|dir| dir.join("share").join("icons")),
            );
        }
        let data_dirs = std::env::var_os("XDG_DATA_DIRS")
            .filter(|dirs| !dirs.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        roots.extend(std::env::split_paths(&data_dirs).map(|dir| dir.join("icons")));
        roots.push(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/icons"));

        roots.iter().find_map(|root| {
            [
                root.join(&file),
                root.join("hicolor/symbolic/apps").join(&file),
                root.join("hicolor/scalable/apps").join(&file),
            ]
            .into_iter()
            .find(|candidate| candidate.is_file())
        })
    }
}
