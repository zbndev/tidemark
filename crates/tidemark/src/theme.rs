//! The user's appearance choice, applied to libadwaita's process-wide style manager.

use tidemark_types::Preferences;

/// Applies the stored appearance choice to every current and future application surface.
pub(crate) fn apply(theme: &str) {
    adw::StyleManager::default().set_color_scheme(color_scheme(theme));
}

/// The libadwaita policy corresponding to one stored choice.
pub(crate) fn color_scheme(theme: &str) -> adw::ColorScheme {
    match theme {
        Preferences::THEME_LIGHT => adw::ColorScheme::ForceLight,
        Preferences::THEME_DARK => adw::ColorScheme::ForceDark,
        Preferences::THEME_SYSTEM => adw::ColorScheme::Default,
        _ => adw::ColorScheme::Default,
    }
}
