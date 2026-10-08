//! The About dialog: who wrote this, which version is running, and where to report it.
//!
//! The icon over the name, the version pill, Details, Report an Issue and Legal.
//!
//! The one thing that is ours is the troubleshooting page. It answers the questions every
//! bug report about this program starts with — which daemon is on the other end, which
//! renderer is drawing, which desktop and session — and it answers them from the running
//! process rather than from what the reporter remembers installing.

use std::path::PathBuf;

use slint::ComponentHandle;

use crate::window::spawn;
use crate::{About, AppWindow};

/// Where **Website** goes.
const WEBSITE_URL: &str = "https://github.com/zbndev/tidemark";
/// Where **Report an Issue** goes. `/new/choose` rather than the issue list, because the
/// repository has templates and a report that skips them is a report that has to be asked
/// for the version, the desktop and the provider all over again.
const ISSUES_URL: &str = "https://github.com/zbndev/tidemark/issues/new/choose";
/// The MIT licence text linked from Legal.
const LICENSE_URL: &str = "https://opensource.org/licenses/mit";
/// The file the troubleshooting page's save button offers.
const DEBUG_INFO_FILENAME: &str = "tidemark-debug-info.txt";

/// Wires the dialog's links and its save button, once, for the life of the window.
pub fn install(ui: &AppWindow) {
    let about = ui.global::<About>();
    about.set_version(env!("CARGO_PKG_VERSION").into());
    // The summary the desktop file and the metainfo already use.
    about.set_comments("Track AI provider quota limits.".into());
    about.set_developer("zbndev".into());
    about.set_copyright("© 2026 zbndev".into());

    let weak = ui.as_weak();
    about.on_close(move || {
        if let Some(ui) = weak.upgrade() {
            ui.global::<About>().set_open(false);
        }
    });
    about.on_open_link(|link| {
        let url = match link.as_str() {
            "website" => WEBSITE_URL,
            "issues" => ISSUES_URL,
            _ => LICENSE_URL,
        };
        if let Err(error) = webbrowser::open(url) {
            tracing::warn!(%error, url, "could not open a link from the About dialog");
        }
    });
    let weak = ui.as_weak();
    about.on_save_debug_info(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let text = ui.global::<About>().get_debug_info().to_string();
        let weak = weak.clone();
        spawn(async move {
            let Some(path) = choose_destination().await else {
                return;
            };
            let message = match std::fs::write(&path, text) {
                Ok(()) => "Debugging information saved".to_owned(),
                Err(error) => format!("Could not save: {error}"),
            };
            if let Some(ui) = weak.upgrade() {
                ui.global::<About>().invoke_show_toast(message.into());
            }
        });
    });
}

async fn choose_destination() -> Option<PathBuf> {
    rfd::AsyncFileDialog::new()
        .set_title("Save Debugging Information")
        .set_file_name(DEBUG_INFO_FILENAME)
        .save_file()
        .await
        .map(|handle| handle.path().to_path_buf())
}

/// Opens the dialog on its summary, with the troubleshooting page filled in as of now.
pub fn present(ui: &AppWindow, daemon: Option<&str>, renderer: &str, tray: bool) {
    let about = ui.global::<About>();
    about.set_debug_info(debug_info(daemon, renderer, tray).into());
    about.set_page(0);
    about.set_open(true);
}

/// What the troubleshooting page shows, and what its copy button puts on the clipboard.
///
/// `daemon` is the version `tidemarkd` reported, absent when nothing answered on the bus;
/// `renderer` is the one the window is actually drawn with, which falls back when the
/// preferred one cannot start; `tray` is whether a status-notifier host accepted the icon,
/// which is the difference between a close button that hides the window and one that ends
/// the program.
fn debug_info(daemon: Option<&str>, renderer: &str, tray: bool) -> String {
    compose(
        daemon,
        renderer,
        tray,
        &environment("XDG_CURRENT_DESKTOP"),
        &environment("XDG_SESSION_TYPE"),
    )
}

fn compose(
    daemon: Option<&str>,
    renderer: &str,
    tray: bool,
    desktop: &str,
    session: &str,
) -> String {
    let client = env!("CARGO_PKG_VERSION");
    let slint = env!("TIDEMARK_SLINT_VERSION");
    let daemon = daemon.unwrap_or("not running");
    let os = std::env::consts::OS;
    let tray = if tray {
        "accepted"
    } else {
        "no status-notifier host"
    };
    format!(
        "Tidemark: {client}\n\
         tidemarkd: {daemon}\n\
         Slint: {slint}\n\
         Renderer: {renderer}\n\
         OS: {os}\n\
         Desktop: {desktop}\n\
         Session: {session}\n\
         Tray: {tray}\n"
    )
}

/// One environment variable, with "unset" rather than an empty line: a report has to
/// distinguish a desktop that says nothing from a field this program failed to fill in.
fn environment(key: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| "unset".to_owned())
}

#[cfg(test)]
mod tests {
    use super::compose;

    #[test]
    fn a_missing_daemon_is_stated_rather_than_left_blank() {
        let info = compose(None, "femtovg-wgpu", false, "Hyprland", "wayland");
        assert!(info.contains("tidemarkd: not running"), "{info}");
        assert!(info.contains("Tray: no status-notifier host"), "{info}");
        assert!(info.contains("Renderer: femtovg-wgpu"), "{info}");
    }

    #[test]
    fn a_connected_daemon_reports_its_own_version() {
        let info = compose(Some("0.2.0"), "femtovg-wgpu", true, "GNOME", "x11");
        assert!(info.contains("tidemarkd: 0.2.0"), "{info}");
        assert!(info.contains("Tray: accepted"), "{info}");
        assert!(info.contains("Desktop: GNOME"), "{info}");
        assert!(info.contains("Session: x11"), "{info}");
    }
}
