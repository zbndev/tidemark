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
//! # A missing mark is a normal state
//!
//! A provider we have no mark for gets a card without one, and so does an installation that
//! chose not to ship them — Debian's DFSG and Fedora's trademark rules can both refuse
//! third-party artwork, so a build with no marks stays a supported configuration rather
//! than a broken one. That is why the widget is asked whether the icon exists rather than
//! being handed one: `gtk::Image::from_icon_name` on a name the theme does not have draws
//! the broken-image icon, which is worse than nothing.

use gtk::prelude::*;

/// Rendered size of a mark, in logical pixels. The prior art renders the same files at
/// 18×18 and it is the size they survive: at 16 the Codex knot loses the chevron inside it.
const SIZE: i32 = 18;

/// The icon name a provider's mark is installed under, or `None` for a slug that cannot
/// name one.
///
/// The slug arrives over D-Bus from the daemon, so it is not assumed to be one of ours.
/// Anything outside `[a-z0-9-]` gets no mark rather than a lookup for a name we would never
/// have installed.
pub fn icon_name(slug: &str) -> Option<String> {
    let usable = !slug.is_empty()
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    usable.then(|| format!("tidemark-{slug}-symbolic"))
}

/// The image widget for a card's title row. Starts hidden; [`set`] fills it in.
pub fn image() -> gtk::Image {
    gtk::Image::builder()
        .pixel_size(SIZE)
        .valign(gtk::Align::Center)
        .visible(false)
        .build()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_names_the_icon_it_was_installed_as() {
        assert_eq!(
            icon_name("zai").as_deref(),
            Some("tidemark-zai-symbolic"),
            "the name here and the filename in data/icons are the same convention"
        );
        assert_eq!(
            icon_name("antigravity").as_deref(),
            Some("tidemark-antigravity-symbolic")
        );
    }

    #[test]
    fn a_slug_that_is_not_ours_gets_no_mark_rather_than_a_lookup() {
        for slug in ["", "Z.ai", "../../etc", "zai fake", "ZAI"] {
            assert_eq!(icon_name(slug), None, "slug {slug:?} should name no icon");
        }
    }
}
