//! The About dialog: who wrote this, which version is running, and where to report it.
//!
//! `AdwAboutDialog` is the whole of it. Every part of the standard layout — the icon over
//! the name, the version pill, Details, Report an Issue, Legal — is a property rather than
//! a widget we place, so the dialog looks like every other GNOME application's and follows
//! the platform when libadwaita changes its mind about the arrangement.
//!
//! The licence text is not written out here either. `gtk::License::MitX11` is what makes
//! the Legal page say the application comes with absolutely no warranty and link the MIT
//! licence; spelling that sentence ourselves would be a second copy of a legal notice to
//! keep in agreement with the one GTK already translates.
//!
//! The one thing that is ours is the troubleshooting page. It answers the questions every
//! bug report about this program starts with — which daemon is on the other end, whether
//! the panel took the icon, which toolkit is actually loaded — and it answers them from the
//! running process rather than from what the reporter remembers installing.

use adw::prelude::*;
use tidemark_types::ids;

/// Where **Details** goes.
const WEBSITE_URL: &str = "https://github.com/zbndev/tidemark";
/// Where **Report an Issue** goes. `/new/choose` rather than the issue list, because the
/// repository has templates and a report that skips them is a report that has to be asked
/// for the version, the desktop and the provider all over again.
const ISSUES_URL: &str = "https://github.com/zbndev/tidemark/issues/new/choose";
/// The file the troubleshooting page's save button offers.
const DEBUG_INFO_FILENAME: &str = "tidemark-debug-info.txt";

/// Shows the About dialog over `parent`.
///
/// `AdwDialog` is modal within the window, so there is no second one to guard against: the
/// menu button that activates this is behind it while it is up.
pub fn present(parent: &impl IsA<gtk::Widget>, debug_info: &str) {
    let dialog = adw::AboutDialog::builder()
        .application_icon(ids::APP_ID)
        .application_name("Tidemark")
        .developer_name("zbndev")
        .version(env!("CARGO_PKG_VERSION"))
        // The summary the desktop file and the metainfo already use. It is what turns the
        // website link into a **Details** page rather than a bare row.
        .comments("Track AI provider quota limits.")
        .website(WEBSITE_URL)
        .issue_url(ISSUES_URL)
        .copyright("© 2026 zbndev")
        .license_type(gtk::License::MitX11)
        .debug_info(debug_info)
        .debug_info_filename(DEBUG_INFO_FILENAME)
        .build();
    dialog.present(Some(parent));
}

/// What the troubleshooting page shows, and what its copy button puts on the clipboard.
///
/// `daemon` is the version `tidemarkd` reported, absent when nothing answered on the bus;
/// `tray` is whether a status-notifier host accepted the icon, which is the difference
/// between a close button that hides the window and one that ends the program.
///
/// The toolkit versions are the ones the process loaded, not the ones it was built
/// against. Those are the same number often enough that stating the compiled floor would
/// look like an answer while being no evidence at all about the machine the bug is on.
pub fn debug_info(daemon: Option<&str>, tray: bool) -> String {
    compose(
        daemon,
        tray,
        &format!(
            "{}.{}.{}",
            gtk::major_version(),
            gtk::minor_version(),
            gtk::micro_version()
        ),
        &format!(
            "{}.{}.{}",
            adw::major_version(),
            adw::minor_version(),
            adw::micro_version()
        ),
    )
}

/// The page, given the facts. Separate from the two calls above because those assert that
/// GTK has been initialised, which a unit test has no way to arrange and no need to.
fn compose(daemon: Option<&str>, tray: bool, gtk_version: &str, adw_version: &str) -> String {
    let client = env!("CARGO_PKG_VERSION");
    let daemon = daemon.unwrap_or("not running");
    let tray = if tray {
        "accepted"
    } else {
        "no status-notifier host"
    };
    format!(
        "Tidemark: {client}\n\
         tidemarkd: {daemon}\n\
         GTK: {gtk_version}\n\
         libadwaita: {adw_version}\n\
         Desktop: {}\n\
         Session: {}\n\
         Tray: {tray}\n",
        environment("XDG_CURRENT_DESKTOP"),
        environment("XDG_SESSION_TYPE"),
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
        let info = compose(None, false, "4.22.4", "1.9.3");
        assert!(info.contains("tidemarkd: not running"), "{info}");
        assert!(info.contains("Tray: no status-notifier host"), "{info}");
    }

    #[test]
    fn a_connected_daemon_reports_its_own_version() {
        let info = compose(Some("0.2.0"), true, "4.22.4", "1.9.3");
        assert!(info.contains("tidemarkd: 0.2.0"), "{info}");
        assert!(info.contains("Tray: accepted"), "{info}");
        assert!(info.contains("GTK: 4.22.4"), "{info}");
    }
}
