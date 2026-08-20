//! The little CSS this interface adds to the platform's.
//!
//! Everything that libadwaita already names is used by name — `card`, `title-1`, `heading`,
//! `caption`, `dim-label`, `warning`, `error` — so the window follows the system's accent
//! colour, dark mode and font scaling without being told to. What is left is the two
//! things Adwaita has no class for: the pill around a state chip, and the padding inside a
//! card, which `.card` deliberately does not set because it does not know what is going in
//! it.
//!
//! # The hover
//!
//! `GtkFlowBoxChild` tints its own allocation, which is a square behind a card with rounded
//! corners: the tint shows in the four corners as a hard edge around them. So the child's
//! tint is switched off and the card is raised instead — two pixels and a soft shadow, the
//! platform's own idea of an elevated surface, and nothing that repaints text.
//!
//! The `:hover` is matched on the child rather than on the card, deliberately: the child
//! keeps its allocation while the card inside it moves, so a pointer resting near the lower
//! edge does not fall outside the widget it just raised and start it flickering.
//!
//! It is `translateY` and not `scale` because scaling a widget resamples the text in it, and
//! a card is mostly text. This is the one place where the interface says "you can click
//! this" before Step 14 makes the click do anything.
//!
//! `alpha(currentColor, …)` is what keeps the chip theme-aware: the background is derived
//! from whatever colour the tone class put on the label, so a warning chip and an error
//! chip need no colours of their own.
//!
//! The quota bar gets its fill colour the same way, as the `color` of the drawing area. That
//! is the only route to a themed accent: these three names are what a user's own stylesheet
//! redefines when they change their accent colour, and a bar that asked libadwaita instead
//! would stay blue in a window where everything else had turned grey.

pub(crate) const STYLE: &str = "
.quota-card {
    /* Less room under the footer than over the title: the last line sits low in the card,
       where a timestamp belongs, rather than floating in the middle of its own margin. */
    padding: 16px 16px 10px;
    transition: transform 150ms ease-out, box-shadow 150ms ease-out;
}

/* The grid's children are the cards themselves, so the hover belongs to the card and its
   rounded corners; left alone, GtkFlowBoxChild tints its own square allocation and the
   corners of the card get a visible hard edge around them. */
.quota-grid > flowboxchild:hover {
    background: none;
}

.quota-grid > flowboxchild:hover > .quota-card {
    transform: translateY(-2px);
    box-shadow: 0 2px 4px 0 rgba(0, 0, 0, 0.18), 0 8px 20px 0 rgba(0, 0, 0, 0.34);
}

.quota-bar {
    color: @accent_bg_color;
}

.quota-bar.quota-warning {
    color: @warning_bg_color;
}

.quota-bar.quota-danger {
    color: @error_bg_color;
}

.quota-chip {
    border-radius: 9999px;
    padding: 1px 9px;
    background-color: alpha(currentColor, 0.13);
    font-weight: bold;
}

/* The plan is the same pill without a tone: it says what the account is, not what is wrong
   with it. `alpha(currentColor, …)` is what makes one rule serve both styles — the
   foreground is dark on a light card and light on a dark one, so the pill comes out a step
   darker than the surface in the first case and a step lighter in the second. */
.quota-plan {
    border-radius: 9999px;
    padding: 2px 9px;
    background-color: alpha(currentColor, 0.14);
    opacity: 0.75;
}

/* Quieter than `.dim-label`, and a size below `.caption`. It is the only line on the card
   that is about the reading rather than the quota, and it should be readable without being
   read. */
.quota-footer {
    font-size: 0.9em;
    opacity: 0.45;
}
";

/// Adds the stylesheet to the display, above the theme and below the user's own overrides.
pub fn load() {
    let Some(display) = gtk::gdk::Display::default() else {
        // No display means no window either; whatever failed is about to be reported by
        // something with more to say than this.
        return;
    };

    let provider = gtk::CssProvider::new();
    provider.load_from_string(STYLE);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}
