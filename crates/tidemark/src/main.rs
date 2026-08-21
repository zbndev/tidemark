//! Tidemark's desktop client.
//!
//! A viewer, and nothing else. It holds no credentials, makes no network request and opens
//! no database: everything on screen arrived from `tidemarkd` over the session bus, and
//! `scripts/check-layering.sh` is what keeps that true as the program grows.
//!
//! The pieces: `bus` talks to the daemon, `window` owns the grid, `card` draws one account,
//! `provider_settings` is the dialog that adds, edits and removes them,
//! `bar` draws the quota bar and its pace mark, `mark` finds the provider's own logo,
//! `model` decides what order things go in, `format` decides what they say, and `style`
//! adds the handful of CSS rules libadwaita does not already provide.

mod bar;
mod bus;
mod card;
mod format;
mod mark;
mod model;
mod provider_settings;
mod style;
mod window;

use adw::prelude::*;
use gtk::glib;
use tidemark_types::ids;

use crate::window::MainWindow;

fn main() -> glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tidemark=info".into()),
        )
        .init();

    if std::env::args().any(|a| a == "--version") {
        println!("tidemark {}", env!("CARGO_PKG_VERSION"));
        return glib::ExitCode::SUCCESS;
    }

    let app = adw::Application::builder()
        .application_id(ids::APP_ID)
        .build();

    app.connect_startup(|_| style::load());
    app.connect_activate(|app| {
        // A second `tidemark` on an already-running instance raises the window it has
        // rather than opening another one onto the same daemon.
        if let Some(existing) = app.active_window() {
            existing.present();
            return;
        }
        tracing::info!(app_id = ids::APP_ID, "presenting main window");
        MainWindow::present(app);
    });

    // GTK's own argument parsing is not wanted: the only flag this program has is handled
    // above, and anything else on the command line is a mistake worth ignoring rather than
    // a reason to refuse to start.
    app.run_with_args::<&str>(&[])
}
