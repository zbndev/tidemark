//! Tidemark's desktop client.
//!
//! A viewer, and nothing else. It holds no credentials, makes no network request and opens
//! no database: everything on screen arrived from `tidemarkd` over the session bus, and
//! `scripts/check-layering.sh` is what keeps that true as the program grows.
//!
//! The pieces: `bus` talks to the daemon, `window` owns the order the cards are in,
//! `grid` lays them out and lets the user drag them into a different one, `card` draws one
//! account, `provider_settings` is the dialog that adds, edits and removes them,
//! `about` is the primary menu's dialog, `bar` draws the quota bar and its pace mark,
//! `mark` finds the provider's own logo, `model` decides what order things go in,
//! `format` decides what they say, and `style` adds the handful of CSS rules libadwaita
//! does not already provide.

mod about;
mod bar;
mod bus;
mod card;
mod chart;
mod detail;
mod format;
mod grid;
mod mark;
mod model;
mod preferences;
mod provider_settings;
mod style;
mod tray;
#[cfg(windows)]
mod tray_icon_rgba;
mod update;
mod window;

use adw::prelude::*;
use gtk::glib;
use tidemark_types::ids;

use crate::window::MainWindow;

fn background_requested<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    args.into_iter()
        .any(|argument| argument.as_ref() == "--background")
}

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tidemark=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|argument| argument == "--version") {
        println!("tidemark {}", env!("CARGO_PKG_VERSION"));
        return glib::ExitCode::SUCCESS;
    }
    let background = background_requested(&args);

    let app = adw::Application::builder()
        .application_id(ids::APP_ID)
        .build();

    app.connect_startup(|_| style::load());
    app.connect_activate(move |app| {
        // A second `tidemark` on an already-running instance raises the window it has
        // rather than opening another one onto the same daemon.
        if let Some(existing) = app.active_window() {
            existing.present();
            return;
        }
        tracing::info!(app_id = ids::APP_ID, background, "starting desktop client");
        MainWindow::present(app, background);
    });

    // GTK's own argument parsing is not wanted: this program's two flags are handled above,
    // and anything else on the command line is a mistake worth ignoring rather than a
    // reason to refuse to start.
    app.run_with_args::<&str>(&[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_flag_requests_a_hidden_start() {
        assert!(background_requested(["tidemark", "--background"]));
    }

    #[test]
    fn ordinary_launch_requests_a_visible_start() {
        assert!(!background_requested(["tidemark"]));
    }
}
