//! Tidemark's desktop client.
//!
//! Scaffolding only: this opens an empty libadwaita window so that the toolchain, the
//! GTK 4 / libadwaita bindings and the runtime initialisation are all proven before the
//! interface described in `CONTEXT.md` is built on top of them.

use adw::prelude::*;
use gtk::glib;
use tidemark_types::ids;

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
    app.connect_activate(build_window);
    app.run_with_args::<&str>(&[])
}

fn build_window(app: &adw::Application) {
    let page = adw::StatusPage::builder()
        .title("Tidemark")
        .description("No providers configured yet.")
        .build();

    let view = adw::ToolbarView::builder().content(&page).build();
    view.add_top_bar(&adw::HeaderBar::new());

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Tidemark")
        .default_width(900)
        .default_height(600)
        .content(&view)
        .build();

    tracing::info!(app_id = ids::APP_ID, "presenting main window");
    window.present();
}
