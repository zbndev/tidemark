//! The provider's own mark, next to its name on the card.
//!
//! `CONTEXT.md` § Interface: the logo is the provider's own mark, monochrome — not a glyph
//! of our invention standing in for someone else's product. The files are the owners'
//! trademarks, used to identify the service the card is about; `docs/TRADEMARKS.md` says so
//! and ships with them.
//!
//! # Why an icon name and not a file
//!
//! A symbolic SVG only takes the theme's colour when GTK loads it *as a symbolic icon*,
//! through the icon theme: the loader wraps the file in an `<svg>` of the requested size
//! and forces `fill` on every `path` to the widget's foreground colour. Loaded as a file or
//! a texture the same SVG keeps whatever colour is written in it, which on a dark theme is
//! a black smudge. So the marks are installed into `hicolor` under
//! `symbolic/apps/tidemark-<slug>-symbolic.svg` and asked for by name; the source tree
//! mirrors that layout in `data/icons`, so running uninstalled works with
//! `XDG_DATA_DIRS=$PWD/data:$XDG_DATA_DIRS` and no code of its own.
//!
//! # Fills only
//!
//! That loader paints `fill` and nothing else. A `stroke` is not recoloured, it is not drawn
//! at all: a mark whose geometry is strokes renders as an empty card, which is what the xAI
//! mark did until its three lines were outlined into filled paths. Every file here is
//! therefore filled geometry, and `scripts/check-desktop-integration.sh` refuses one that
//! carries a `stroke` attribute rather than leaving the next mark to be found blank by eye.
//!
//! # A missing mark is a normal state
//!
//! A provider we have no mark for gets a card without one, and so does an installation that
//! chose not to ship them — Debian's DFSG and Fedora's trademark rules can both refuse
//! third-party artwork, so a build with no marks stays a supported configuration rather
//! than a broken one. That is why the widget is asked whether the icon exists rather than
//! being handed one: `gtk::Image::from_icon_name` on a name the theme does not have draws
//! the broken-image icon, which is worse than nothing.

use gtk::prelude::*;

/// Rendered size of a mark, in logical pixels. Well above the text beside it on purpose —
/// the mark is what the eye finds the card by, and 18 is only the floor at which the Codex
/// knot still shows the chevron inside it.
///
/// Each file is drawn to stand on the lower edge of this box, so the size is also what puts
/// the mark on the name's baseline; see the note on framing in `docs/TRADEMARKS.md`.
const SIZE: i32 = 28;

/// The icon name a provider's mark is installed under. Defined in the shared crate, so
/// the card and the daemon's notifications look the same mark up.
///
/// Provider-first, not slug-first: a plugin's storage key is its dotted id, and the mark is
/// materialized under that id with the dots dashed out — `provider_icon_name` is why both
/// spellings resolve through one place.
pub use tidemark_types::present::provider_icon_name;

/// The image widget for a card's title row. Starts hidden; [`set`] fills it in.
pub fn image_at(pixel_size: i32) -> gtk::Image {
    gtk::Image::builder()
        .pixel_size(pixel_size)
        .valign(gtk::Align::Center)
        .visible(false)
        .build()
}

/// The image widget for a card's title row. Starts hidden; [`set`] fills it in.
pub fn image() -> gtk::Image {
    image_at(SIZE)
}

/// Shows `provider`'s mark in `image`, or hides the image if there is no mark for it.
///
/// `provider` is what the wire calls the service: a built-in slug or a plugin's dotted id.
pub fn set(image: &gtk::Image, provider: &str) {
    let name = provider_icon_name(provider).filter(|name| has_icon(image, name));
    match name {
        Some(name) => {
            image.set_icon_name(Some(&name));
            image.set_visible(true);
        }
        None => {
            image.set_icon_name(None);
            image.set_visible(false);
        }
    }
}

/// Shows a sanitized mark received from `InspectPlugin` before the plugin exists in the
/// icon theme.
///
/// `FORCE_SYMBOLIC` matters here for exactly the same reason as the installed icon-name
/// path: a direct texture would keep the SVG's black fill on a dark theme. `BytesIcon`
/// keeps the dry-run a dry run — neither the daemon nor the GUI has to materialize the
/// preview on disk.
pub fn set_preview(image: &gtk::Image, svg: &str) {
    let bytes = gtk::glib::Bytes::from_owned(svg.as_bytes().to_vec());
    let icon = gtk::gio::BytesIcon::new(&bytes);
    let paintable = gtk::IconTheme::for_display(&image.display()).lookup_by_gicon(
        &icon,
        image.pixel_size().max(1),
        image.scale_factor(),
        gtk::TextDirection::None,
        gtk::IconLookupFlags::FORCE_SYMBOLIC,
    );
    // Unlike a named `*-symbolic` theme icon, a BytesIcon carries no symbolic filename for
    // GTK to infer this property from. The lookup flag selects the symbolic loader; the
    // property tells GtkImage to snapshot the resulting SymbolicPaintable with CSS colours.
    paintable.set_property("is-symbolic", true);
    image.set_paintable(Some(&paintable));
    image.set_visible(true);
}

/// Whether the icon theme of the display this widget is on has `name`.
fn has_icon(widget: &impl IsA<gtk::Widget>, name: &str) -> bool {
    gtk::IconTheme::for_display(&widget.as_ref().display()).has_icon(name)
}

/// Adds a directory of installed plugin marks to this display's icon theme.
///
/// Called with the root the daemon publishes in `DataInfo`. After this a plugin mark is
/// found by [`provider_icon_name`] exactly the way a shipped mark is — which is the whole reason
/// the daemon materializes it as a file rather than sending bytes: GTK only recolours a
/// symbolic SVG it loaded *through the icon theme*, and a texture built from bytes would
/// be the black smudge this module's own note describes.
///
/// Idempotent by construction: adding a path the theme already has is a no-op in GTK, so
/// a daemon that republishes its `DataInfo` does not lengthen the search path.
pub fn add_plugin_path(display: &gtk::gdk::Display, root: &std::path::Path) {
    gtk::IconTheme::for_display(display).add_search_path(root);
}

/// Rebuilds GTK's icon database after the daemon changed the contents of a registered
/// plugin theme.
///
/// GTK notices ordinary theme switches, but it does not discover a mark written beneath
/// an application search path that was already empty when the path was registered. There
/// is no rescan operation in GTK 4; assigning the existing search path is its supported
/// invalidation boundary. The list is replaced in place rather than appending the plugin
/// root again on every install.
pub fn refresh(display: &gtk::gdk::Display) {
    let theme = gtk::IconTheme::for_display(display);
    let paths = theme.search_path();
    let paths = paths
        .iter()
        .map(std::path::PathBuf::as_path)
        .collect::<Vec<_>>();
    theme.set_search_path(&paths);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pins the two behaviours of this module that need a real display: a mark
    /// materialized beneath an already-registered search path appears after [`refresh`],
    /// and [`set_preview`] draws inspected bytes symbolically before anything is installed.
    ///
    /// Ignored rather than dropped, because the harness gives every test its own thread
    /// while GTK belongs to one: run alongside the widget tests, this test's display and
    /// icon-theme work collides with theirs — the same boundary `font.rs` names when it
    /// declines its own display-bound assertion. Run it when nothing else in the process
    /// touches GTK: `cargo test -p tidemark -- --ignored`.
    #[test]
    #[ignore = "needs a display and exclusive GTK ownership; run alone with --ignored"]
    fn a_materialized_mark_appears_after_refresh_and_a_preview_draws_symbolically() {
        // `adw::init` alone is not evidence of a display: it succeeds headless, and the
        // first NULL then reaches the icon theme as an assertion, not a skip.
        if adw::init().is_err() || gtk::gdk::Display::default().is_none() {
            eprintln!("skipped: no display is available");
            return;
        }
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("the test clock is after the epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "tidemark-icon-theme-refresh-{}-{nonce}",
            std::process::id()
        ));
        let provider = format!("com.example.refresh{nonce}");
        let name = provider_icon_name(&provider).expect("the plugin id names an icon");
        let path = root
            .join("hicolor/symbolic/apps")
            .join(format!("{name}.svg"));
        std::fs::create_dir_all(path.parent().expect("the icon has a parent"))
            .expect("the temporary icon theme is created");

        let display = gtk::gdk::Display::default().expect("GTK has a display");
        let theme = gtk::IconTheme::for_display(&display);
        add_plugin_path(&display, &root);
        assert!(!theme.has_icon(&name), "the theme starts without the mark");

        std::fs::write(
            &path,
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><path fill="currentColor" d="M0 0h64v64H0z"/></svg>"#,
        )
        .expect("the daemon materializes the mark");
        assert!(
            !theme.has_icon(&name),
            "GTK keeps the theme database it built before the mark existed"
        );

        refresh(&display);

        assert!(
            theme.has_icon(&name),
            "the running client discovers the newly materialized mark"
        );
        let shown = image();
        set(&shown, &provider);
        assert_eq!(shown.icon_name().as_deref(), Some(name.as_str()));
        assert!(shown.is_visible(), "the provider mark is drawn again");

        std::fs::remove_dir_all(root).expect("the temporary icon theme is removed");

        let preview = image();
        set_preview(
            &preview,
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64"><path fill="currentColor" d="M0 0h64v64H0z"/></svg>"#,
        );
        let paintable = preview
            .paintable()
            .expect("the inspected bytes draw without an installed theme icon")
            .downcast::<gtk::IconPaintable>()
            .expect("the preview uses GTK's icon loader");
        assert!(
            paintable.property::<bool>("is-symbolic"),
            "the preview follows the theme colour"
        );
        assert!(preview.is_visible());
    }
}
