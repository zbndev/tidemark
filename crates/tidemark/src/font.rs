//! App-private font discovery for Windows.
//!
//! The installer carries Rubik beside the GTK runtime and adds the private
//! font to GTK's display-wide map.

use std::path::{Path, PathBuf};

use gtk::prelude::*;

/// Adds the installed Rubik to GTK's display-wide Pango font map.
///
/// This must run during application startup, before the first window builds
/// its labels. Pango owns the registration, and nothing is installed in the
/// user's Windows font collection.
pub(crate) fn configure() {
    let Ok(executable) = std::env::current_exe() else {
        return;
    };
    let font = bundled_font_file(&executable);
    if !font.is_file() {
        return;
    }

    let label = gtk::Label::new(None);
    let Some(font_map) = label.pango_context().font_map() else {
        tracing::warn!("could not find GTK's Pango font map for bundled Rubik");
        return;
    };
    if let Err(error) = register(&font_map, &font) {
        tracing::warn!(%error, path = %font.display(), "could not register the bundled Rubik font");
    }
}

/// Locates the font installed with the GUI executable.
pub(crate) fn bundled_font_file(executable: &Path) -> PathBuf {
    executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("share")
        .join("fonts")
        .join("Rubik%5Bwght%5D.ttf")
}

/// Adds a bundled font file to a Pango font map.
pub(crate) fn register(font_map: &pango::FontMap, font: &Path) -> Result<(), gtk::glib::Error> {
    font_map.add_font_file(font)
}
