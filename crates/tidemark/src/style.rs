//! The little CSS this interface adds to the platform's.
//!
//! Everything that libadwaita already names is used by name — `card`, `title-1`, `heading`,
//! `caption`, `dim-label`, `warning`, `error` — so the window follows the system's accent
//! colour, dark mode and font scaling without being told to. What is left is the three
//! things Adwaita has no class for: the pill around a state chip, the padding inside a
//! card, which `.card` deliberately does not set because it does not know what is going in
//! it, and the padding around the credential pill, for the same reason.
//!
//! # The hover, and the lift
//!
//! A card raises on hover — two pixels and a soft shadow, the platform's own idea of an
//! elevated surface, and nothing that repaints text. It is `translateY` and not `scale`
//! because scaling a widget resamples the text in it, and a card is mostly text.
//!
//! **The `:hover` is matched on the slot around the card and applied to the card inside
//! it.** A CSS transform moves what GTK picks, so a card that lifted itself out from under
//! the pointer would flicker; the slot keeps its allocation while the card inside it moves.
//! That slot used to be a `GtkFlowBoxChild`, which also had to be told not to tint its own
//! square allocation behind a card with rounded corners. An `AdwBin` paints nothing, so
//! only the hover rule is left.
//!
//! A card being dragged is lifted further and holds still: the hover transform is taken off
//! it, because it is already off the surface and because the grid is moving it by
//! allocation rather than by transform.
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

/* The slot keeps the pointer while the card inside it moves. */
.quota-grid > .quota-slot:hover > .quota-card {
    transform: translateY(-2px);
    box-shadow: 0 2px 4px 0 rgba(0, 0, 0, 0.18), 0 8px 20px 0 rgba(0, 0, 0, 0.34);
}

/* Held by the pointer: opaque, further off the surface, and not offset, because the grid is
   already carrying it.

   The background is the point. `.card` takes `@card_bg_color`, which in the dark style is
   8% white *over whatever is behind it* — correct for a card lying on the window, and wrong
   for one being carried over its neighbours, which then show through it. A lifted card is
   opaque, and `@popover_bg_color` is the platform's own name for a surface floating above
   the content rather than a colour of our invention. The foreground is deliberately left
   alone: the bar's track and its pace mark take the inherited text colour, and changing it
   would make them shift tone for the length of a drag. */
.quota-grid > .quota-slot.dragging > .quota-card,
.quota-grid > .quota-slot:hover.dragging > .quota-card {
    background-color: @popover_bg_color;
    transform: none;
    box-shadow: 0 4px 8px 0 rgba(0, 0, 0, 0.24), 0 16px 36px 0 rgba(0, 0, 0, 0.44);
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

/* The credential pill sits in a preferences row of its own so that it gets the full width
   of the group — a header suffix ellipsized both of its labels, and neither `Tidemark
   login` nor `Claude Code login` can be shortened without becoming a guess. A plain
   AdwPreferencesRow has no padding of its own, so the pill would otherwise touch the four
   edges of the card it sits in. */
.credential-choice {
    padding: 8px;
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
