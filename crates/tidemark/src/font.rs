//! The Windows text stack: which rasterizer draws, and which font it draws with.
//!
//! Two things have to be settled before the first label exists. The interface is
//! typeset in Rubik, which the installer carries beside the GTK runtime rather than
//! installing into the user's font collection, so Pango has to be handed the file.
//! And Pango has to be told which of its two Windows back ends to use, because the
//! default one cannot read the font it is being handed.
//!
//! # Why not the default back end
//!
//! `PangoCairoWin32FontMap` is what `pango_cairo_font_map_get_default()` returns on
//! Windows, and it goes through GDI. Rubik ships as a single variable font with a
//! `wght` axis from 300 to 900, and GDI predates that idea: it reads the default
//! instance's `hmtx` and never looks at `HVAR`, the table that says how advances move
//! along the axis. Measured on this font at 26pt, GDI drew heavier and heavier glyphs
//! while handing out one fixed set of advances:
//!
//! ```text
//!               advances       ink of each glyph
//!   Light       20, 14, 26     20, 12, 24
//!   Bold        20, 14, 26     23, 16, 27
//!   Heavy       20, 14, 26     25, 19, 29
//! ```
//!
//! Every glyph past Medium is wider than the box it is placed in, so the headline
//! percentage on a card — `.title-1`, which libadwaita sets at weight 800 — came out
//! with its digits overlapping. Nothing in the stylesheet can fix that: the advances
//! are wrong before CSS is consulted.
//!
//! The same back end also declines to fall back. GDI substitution is arranged around
//! installed font families, and a font added privately to the process is not one, so
//! any character Rubik does not have — `→` in a provider's credential hint, `⚠`, a
//! check mark — was drawn as the missing-glyph box instead of being borrowed from a
//! system font. Families Windows knows about fall back normally, which is what made
//! this look like a Rubik problem rather than a back-end one.
//!
//! `PangoCairoFcFontMap` — cairo's FreeType font type, fontconfig for matching — has
//! neither fault, and it is the same code path the Linux build has always used. It
//! reads `HVAR`, so advances grow with weight; it falls back through fontconfig, so
//! `→` arrives from Arial; and it hints through FreeType at the hint style GTK asks
//! for, which is why small text stops looking washed. One call at startup buys all
//! three, and the price is the fontconfig configuration in `etc/fonts` that
//! `data/packaging/windows/stage-gtk-runtime.sh` now stages beside the DLLs.
//!
//! # Why the metrics are left unhinted
//!
//! Which rasterizer draws is half of it; what GTK asks that rasterizer for is the other
//! half, and it comes from `gtk-font-rendering`. Upstream that setting is `automatic`.
//! MSYS2's gtk4 package patches the default to `manual`
//! (`mingw-w64-gtk4/001-fix-font-rendering.patch`), which is the right patch for the GDI
//! back end it was written against and the wrong one for this process.
//!
//! `manual` is GTK honouring the low-level settings, and one of them —
//! `gtk-hint-font-metrics` — defaults to true. It rounds every glyph advance to a whole
//! pixel while leaving the outline the advance belongs to exactly where the font put it,
//! and it turns subpixel positioning off with it. The two then disagree, by up to a whole
//! pixel, at the sizes the smallest labels are drawn: a card footer and a preference row's
//! subtitle land near 12px on a 96 dpi display, and measured over "checked 4 minutes ago"
//! and "io.github.zbndev.Tidemark.ProviderKey" at 12px, 7 of 60 glyphs inked past the
//! advance they were given. Ink that runs past its own advance is ink in the next
//! letter's space; the rounding that put it there is also why one line's word gaps come
//! out uneven. Left alone, no glyph in that sample overruns at any size from 10 to 14px.
//!
//! Asking for `automatic` by name at startup undoes the packaging patch for this process
//! and no other. It is one setting rather than four because it pins the four together —
//! grayscale antialiasing, slight hinting, unhinted metrics, subpixel positions — and
//! that set is the configuration the Linux build has always run under.

use std::path::{Path, PathBuf};

use gtk::prelude::*;

/// Puts Pango on the FreeType back end, before anything asks it for a font.
///
/// Must be called before GTK builds its first Pango context: this replaces the
/// process-wide default font map, and a context already made from the old one keeps
/// it. `main` calls it first thing, ahead of the `Application` it then runs.
///
/// A failure here is a working interface with the old defects rather than no
/// interface: cairo without a FreeType back end is not a configuration this project
/// ships, but if one is ever built, it should still start.
pub(crate) fn use_freetype_rasterizer() {
    let Some(map) = freetype_font_map() else {
        tracing::warn!(
            "cairo has no FreeType back end; text falls back to GDI, where the bundled \
             variable font mis-spaces its heavier weights and cannot borrow missing glyphs"
        );
        return;
    };
    pangocairo::FontMap::set_default(Some(&map));
}

/// A font map that rasterizes through FreeType and matches through fontconfig.
///
/// Separate from the call that installs it so the two properties this module exists
/// for — advances that follow the weight axis, and a fallback that reaches the
/// machine's own fonts — can be measured without moving the process-wide default.
fn freetype_font_map() -> Option<pangocairo::FontMap> {
    pangocairo::FontMap::for_font_type(gtk::cairo::FontType::FontTypeFt)?
        // pango_cairo_font_map_new_for_font_type returns a PangoCairoFontMap or nothing.
        .downcast::<pangocairo::FontMap>()
        .ok()
}

/// Asks GTK for the font rendering it chooses everywhere else.
///
/// Needs a display, because `GtkSettings` belongs to one, and has to run before the first
/// widget builds its Pango context — so `startup`, not beside `use_freetype_rasterizer`.
/// Every context GTK makes afterwards is measured with unhinted metrics and positioned at
/// subpixel offsets, which is what keeps the smallest labels off each other. Measured on
/// the packaged runtime: a fresh `gtk::Label` reports `is_round_glyph_positions()` true
/// before this call and false after it. That assertion is not a test here, because it
/// needs a display, and a third test in this binary that initializes GTK collides with
/// the two that already do — `gtk::init` belongs to one thread and the harness gives
/// every test its own.
///
/// A failure here is the crowded small text described above rather than no interface.
pub(crate) fn use_automatic_font_rendering() {
    let Some(settings) = gtk::Settings::default() else {
        tracing::warn!(
            "no GTK settings on this display; small text keeps the packaged runtime's \
             hinted metrics, which round each advance off its own glyph"
        );
        return;
    };
    settings.set_gtk_font_rendering(gtk::FontRendering::Automatic);
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// The font this interface is typeset in, from the tree rather than an install.
    fn rubik() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../data/fonts/rubik/Rubik%5Bwght%5D.ttf")
    }

    /// A context on a private FreeType map with Rubik added, or `None` where cairo has
    /// no FreeType back end. Deliberately not the process-wide default: these
    /// measurements must not depend on, or disturb, whatever else the test binary is
    /// drawing. No display is needed — a font map is not a window.
    fn typeset() -> Option<pango::Context> {
        let map = freetype_font_map()?;
        register(map.upcast_ref::<pango::FontMap>(), &rubik()).expect("Rubik loads");
        Some(map.create_context())
    }

    /// The advances one string is drawn with at one weight, in Pango units.
    fn advances(context: &pango::Context, weight: pango::Weight, text: &str) -> Vec<i32> {
        let mut description = pango::FontDescription::from_string("Rubik 26");
        description.set_weight(weight);
        context.set_font_description(&description);
        let layout = pango::Layout::new(context);
        layout.set_text(text);

        let mut runs = layout.iter();
        let mut widths = Vec::new();
        loop {
            if let Some(run) = runs.run_readonly() {
                widths.extend(
                    run.glyph_string()
                        .glyph_info()
                        .iter()
                        .map(|glyph| glyph.geometry().width()),
                );
            }
            if !runs.next_run() {
                break;
            }
        }
        widths
    }

    /// The bug the headline percentage showed: GDI hands out the light instance's
    /// advances whatever weight it draws, so `.title-1` at weight 800 overlapped its
    /// own digits. A back end that reads the variable font's `HVAR` gives every weight
    /// its own, and they grow.
    #[test]
    fn a_heavier_weight_is_given_more_room() {
        let Some(context) = typeset() else {
            eprintln!("skipped: cairo has no FreeType back end");
            return;
        };
        let light = advances(&context, pango::Weight::Light, "41%");
        let heavy = advances(&context, pango::Weight::Heavy, "41%");

        assert_eq!(light.len(), 3, "one advance per digit and the sign");
        assert_eq!(heavy.len(), light.len());
        assert!(
            std::iter::zip(&light, &heavy).all(|(light, heavy)| heavy > light),
            "every glyph must widen with the weight axis, got {light:?} then {heavy:?}"
        );
    }

    /// The same bug stated as what the user saw: at the weight the card's percentage is
    /// drawn in, no glyph may be wider than the room it is placed in. Measured one
    /// glyph at a time, because a total that fits can still hide a collision.
    #[test]
    fn the_heaviest_digits_fit_the_room_they_are_given() {
        let Some(context) = typeset() else {
            eprintln!("skipped: cairo has no FreeType back end");
            return;
        };
        let mut description = pango::FontDescription::from_string("Rubik 26");
        description.set_weight(pango::Weight::Heavy);
        context.set_font_description(&description);

        for glyph in ["4", "1", "%"] {
            let layout = pango::Layout::new(&context);
            layout.set_text(glyph);
            let (ink, logical) = layout.extents();
            assert!(
                ink.width() <= logical.width(),
                "{glyph:?} inks {} into an advance of {}",
                ink.width(),
                logical.width()
            );
        }
    }

    /// Rubik has no U+2192, and a provider's credential hint uses one: "Deepgram
    /// console → API keys". Under GDI a privately added font gets no substitution and
    /// the arrow came out as the missing-glyph box; fontconfig borrows it from a font
    /// the machine already has.
    #[test]
    fn a_character_rubik_lacks_is_borrowed_from_another_font() {
        let Some(context) = typeset() else {
            eprintln!("skipped: cairo has no FreeType back end");
            return;
        };
        context.set_font_description(&pango::FontDescription::from_string("Rubik 12"));
        let layout = pango::Layout::new(&context);
        layout.set_text("console \u{2192} keys");

        let mut runs = layout.iter();
        loop {
            if let Some(run) = runs.run_readonly() {
                for glyph in run.glyph_string().glyph_info() {
                    // PANGO_GLYPH_UNKNOWN_FLAG marks the box drawn in place of a glyph
                    // no font in the chain had.
                    assert_eq!(
                        glyph.glyph() & 0x1000_0000,
                        0,
                        "no character in the hint may be drawn as a missing-glyph box"
                    );
                }
            }
            if !runs.next_run() {
                break;
            }
        }
    }

    /// The size the small labels are actually drawn at, and the room each glyph is given
    /// there. Metric hinting rounds an advance to a whole pixel and leaves the outline it
    /// belongs to where the font put it, so the ink of a letter ends up in the next
    /// letter's space: measured over these two strings at 12px, 7 of their 60 glyphs
    /// overran their own advance, by up to a full pixel, under the packaged runtime's
    /// `manual` default. Under the rendering `use_automatic_font_rendering` asks for, none
    /// do, at any size the interface draws small text at.
    #[test]
    fn small_text_is_given_room_for_its_own_ink() {
        let Some(context) = typeset() else {
            eprintln!("skipped: cairo has no FreeType back end");
            return;
        };
        let mut options = gtk::cairo::FontOptions::new().expect("cairo font options");
        options.set_antialias(gtk::cairo::Antialias::Gray);
        options.set_hint_style(gtk::cairo::HintStyle::Slight);
        options.set_hint_metrics(gtk::cairo::HintMetrics::Off);
        pangocairo::functions::context_set_font_options(&context, Some(&options));
        context.set_round_glyph_positions(false);

        // A card footer and a preference row's subtitle land in this range on a 96 dpi
        // display, whichever way the user has scaled the system font.
        for size in 10..=14 {
            let mut description = pango::FontDescription::from_string("Rubik");
            description.set_absolute_size(f64::from(size) * f64::from(pango::SCALE));
            context.set_font_description(&description);

            for text in [
                "checked 4 minutes ago",
                "io.github.zbndev.Tidemark.ProviderKey",
            ] {
                let layout = pango::Layout::new(&context);
                layout.set_text(text);

                let mut runs = layout.iter();
                loop {
                    if let Some(run) = runs.run_readonly() {
                        let font = run.item().analysis().font();
                        for glyph in run.glyph_string().glyph_info() {
                            let (ink, _) = font.glyph_extents(glyph.glyph());
                            assert!(
                                ink.width() <= glyph.geometry().width(),
                                "at {size}px {text:?} inks {} into an advance of {}",
                                ink.width(),
                                glyph.geometry().width()
                            );
                        }
                    }
                    if !runs.next_run() {
                        break;
                    }
                }
            }
        }
    }

    /// That the map measured above is the one the program will draw with. The default
    /// font map is process-wide and this test moves it — the same move `main` makes
    /// before GTK starts, so a widget test that runs after this one sits nearer to
    /// production than one that runs before it.
    #[test]
    fn the_freetype_map_becomes_the_default() {
        let Some(expected) = freetype_font_map() else {
            eprintln!("skipped: cairo has no FreeType back end");
            return;
        };
        use_freetype_rasterizer();

        assert_eq!(
            pangocairo::FontMap::default().type_(),
            expected.type_(),
            "GTK must build its contexts from the FreeType font map"
        );
    }
}
