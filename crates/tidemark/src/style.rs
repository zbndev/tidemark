//! The little CSS this interface adds to the platform's.
//!
//! Everything that libadwaita already names is used by name — `card`, `title-1`, `heading`,
//! `caption`, `dim-label`, `warning`, `error` — so the window follows the system's accent
//! colour, dark mode and font scaling without being told to. What is left is the two
//! things Adwaita has no class for: the pill around a state chip, and the padding inside a
//! card, which `.card` deliberately does not set because it does not know what is going in
//! it.
//!
//! `alpha(currentColor, …)` is what keeps the chip theme-aware: the background is derived
//! from whatever colour the tone class put on the label, so a warning chip and an error
//! chip need no colours of their own.
//!
//! The quota bar gets its fill colour the same way, as the `color` of the drawing area. That
//! is the only route to a themed accent: these three names are what a user's own stylesheet
//! redefines when they change their accent colour, and a bar that asked libadwaita instead
//! would stay blue in a window where everything else had turned grey.

const STYLE: &str = "
.quota-card {
    padding: 16px;
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
