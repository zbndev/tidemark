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

// A desktop client must not keep a console window: on Windows the GUI subsystem
// detaches it at link time. Gated off tests so failures still print.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod about;
mod bar;
mod bus;
mod card;
mod chart;
#[cfg(windows)]
mod daemon_job;
mod detail;
#[cfg(windows)]
mod file_log;
#[cfg(windows)]
mod font;
mod format;
mod grid;
mod mark;
mod model;
mod preferences;
mod provider_settings;
#[cfg(windows)]
mod single_instance;
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
    // Before GTK exists, because it replaces the font map every later Pango
    // context is made from. See `font` for what the Windows default gets wrong.
    #[cfg(windows)]
    font::use_freetype_rasterizer();

    #[cfg(windows)]
    let sink = file_log::init()
        .map(file_log::Sink::File)
        .unwrap_or(file_log::Sink::Stderr);
    let subscriber = tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "tidemark=info".into()),
    );
    // No console on Windows (GUI subsystem above): without the file the client
    // would be mute anywhere.
    #[cfg(windows)]
    let subscriber = subscriber.with_writer(sink).with_ansi(false);
    subscriber.init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|argument| argument == "--version") {
        println!("tidemark {}", env!("CARGO_PKG_VERSION"));
        return glib::ExitCode::SUCCESS;
    }
    let background = background_requested(&args);

    // No session bus on Windows, so no toolkit single-instance: a second Start
    // launch raises the running window through the daemon and leaves instead of
    // opening a second window with a second tray icon.
    #[cfg(windows)]
    let singleton: Option<single_instance::Guard> = match single_instance::Guard::acquire() {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => {
            glib::MainContext::default().block_on(async {
                if let Err(error) = crate::bus::request_activation().await {
                    tracing::warn!(%error, "could not ask the running window to come forward");
                }
            });
            // On the GNU Windows runtime an ordinary return from `main` can leave
            // GIO's startup machinery alive long enough to construct another
            // application instance. Nothing in this duplicate owns user state;
            // exit immediately after best-effort activation forwarding.
            std::process::exit(0);
        }
        Err(error) => {
            tracing::warn!(%error, "the client singleton mutex is unusable; starting unguarded");
            None
        }
    };
    // GTK's Windows runtime can leave the process serving windows after its
    // `Application::run` call returns, so a lexical RAII scope is too short for
    // this process-wide invariant. The OS closes this kernel handle at process
    // exit (including crash termination), which is exactly when another client
    // may take the mutex.
    #[cfg(windows)]
    std::mem::forget(singleton);

    let app = adw::Application::builder()
        .application_id(ids::APP_ID)
        .build();

    app.connect_startup(|_| {
        // GTK has created its display-wide Pango map, but no application
        // widgets exist yet. Register the packaged font before CSS selects it.
        #[cfg(windows)]
        font::configure();
        style::load();
    });
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

    #[cfg(windows)]
    use std::path::{Path, PathBuf};

    #[test]
    fn background_flag_requests_a_hidden_start() {
        assert!(background_requested(["tidemark", "--background"]));
    }

    #[test]
    fn ordinary_launch_requests_a_visible_start() {
        assert!(!background_requested(["tidemark"]));
    }

    #[cfg(windows)]
    #[test]
    fn the_bundled_rubik_font_file_is_found_beside_the_installed_executable() {
        assert_eq!(
            crate::font::bundled_font_file(Path::new(
                r"C:\\Users\\Ada\\AppData\\Local\\Programs\\tidemark\\tidemark.exe"
            )),
            PathBuf::from(
                r"C:\\Users\\Ada\\AppData\\Local\\Programs\\tidemark\\share\\fonts\\Rubik%5Bwght%5D.ttf"
            )
        );
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires a process-global GTK display; run explicitly on Windows"]
    fn the_bundled_rubik_font_is_visible_to_a_later_pango_context() {
        let font = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/fonts/rubik/Rubik%5Bwght%5D.ttf");

        if adw::init().is_err() {
            eprintln!("skipped Pango inspection: no display is available");
            return;
        }
        let label = gtk::Label::new(None);
        let map = label
            .pango_context()
            .font_map()
            .expect("a GTK label has a Pango font map");
        crate::font::register(&map, &font).expect("Pango should load the bundled Rubik font");

        let later_label = gtk::Label::new(None);
        let description = gtk::pango::FontDescription::from_string("Rubik 11");
        let resolved = later_label
            .pango_context()
            .load_font(&description)
            .expect("Pango should resolve Rubik after the bundled font is added");
        assert!(
            resolved.describe().to_string().starts_with("Rubik"),
            "Pango must use Rubik for newly created GTK widgets, got {}",
            resolved.describe()
        );
    }
}
