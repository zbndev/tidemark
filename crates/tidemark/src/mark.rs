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
pub use tidemark_types::present::icon_name;

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

/// Shows `slug`'s mark in `image`, or hides the image if there is no mark for it.
pub fn set(image: &gtk::Image, slug: &str) {
    let name = icon_name(slug).filter(|name| has_icon(image, name));
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

/// Whether the icon theme of the display this widget is on has `name`.
fn has_icon(widget: &impl IsA<gtk::Widget>, name: &str) -> bool {
    gtk::IconTheme::for_display(&widget.as_ref().display()).has_icon(name)
}
