//! Tidemark's desktop client, drawn with Slint.
//!
//! The successor to the GTK client: the same daemon, the same D-Bus contract, the same
//! decisions about what a card says and where it goes — `model`, `format` and `update`
//! started as copies of the GTK client's — and a different toolkit drawing them.

// A desktop client must not keep a console window: on Windows the GUI subsystem detaches
// it at link time. Gated off tests so failures still print.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod alert;
mod bus;
#[cfg(windows)]
mod daemon_job;
#[cfg(windows)]
mod file_log;
mod frame;
// Carried over from the GTK client whole; the parts nothing reads yet are the dialogs'.
#[allow(dead_code)]
mod format;
mod marks;
#[allow(dead_code)]
mod model;
#[cfg(unix)]
mod portal;
mod provider_settings;
#[cfg(windows)]
mod registry;
#[cfg(windows)]
mod single_instance;
#[allow(dead_code)]
mod update;
mod view;
mod window;

// The code slint-build generates from `ui/`, which does not derive Debug.
#[allow(missing_debug_implementations, clippy::all, clippy::todo)]
mod ui {
    slint::include_modules!();
}
use ui::{
    Alert, AlertForm, AlertResponse, AppWindow, CandidateData, CardData, DetailData, GaugeData,
    MenuEntry, OptionData, PickerRowData, PreviewRow, ProviderRowData, ProviderSettings, RowData,
    SwitchData, Theme,
};

fn main() -> Result<(), slint::PlatformError> {
    #[cfg(windows)]
    let sink = file_log::init()
        .map(file_log::Sink::File)
        .unwrap_or(file_log::Sink::Stderr);
    let subscriber = tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "tidemark_slint=info".into()),
    );
    // No console on Windows (GUI subsystem above): without the file the client would be
    // mute anywhere.
    #[cfg(windows)]
    let subscriber = subscriber.with_writer(sink).with_ansi(false);
    subscriber.init();

    if std::env::args().any(|argument| argument == "--version") {
        println!("tidemark-slint {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // No session bus on Windows, so nothing like GApplication's single instance: a second
    // launch asks the running window to come forward through the daemon and leaves.
    // The guard lives until `main` returns; the kernel releases it if the process dies.
    #[cfg(windows)]
    let _singleton = match single_instance::Guard::acquire() {
        Ok(Some(guard)) => Some(guard),
        Ok(None) => {
            if let Err(error) = async_io::block_on(bus::request_activation()) {
                tracing::warn!(%error, "could not ask the running window to come forward");
            }
            return Ok(());
        }
        Err(error) => {
            tracing::warn!(%error, "the client singleton mutex is unusable; starting unguarded");
            None
        }
    };

    // FemtoVG on wgpu: Direct3D 12 on Windows, Vulkan on Linux. Not Skia — Slint's Skia
    // renderer hints each glyph's outline while parley places it at unhinted advances, so
    // small text comes out with letters crowding or drifting apart. SLINT_BACKEND still
    // overrides this for comparisons.
    if std::env::var_os("SLINT_BACKEND").is_none()
        && let Err(error) = slint::BackendSelector::new()
            .renderer_name("femtovg-wgpu".into())
            .with_winit_window_attributes_hook(window_attributes)
            .select()
    {
        tracing::warn!(%error, "wgpu is unavailable; using Slint's default renderer");
    }
    // The Wayland app ID and X11 class: the desktop file's name, so compositors' rules
    // and the dock treat this window as Tidemark.
    if let Err(error) = slint::set_xdg_app_id(tidemark_types::ids::APP_ID) {
        tracing::warn!(%error, "could not set the application ID");
    }

    let ui = AppWindow::new()?;
    frame::install(&ui);
    let _main = window::MainWindow::start(&ui);
    slint::ComponentHandle::run(&ui)
}

/// A window that draws its own frame still wants the system's shadow around it.
fn window_attributes(
    attributes: slint::winit_030::winit::window::WindowAttributes,
) -> slint::winit_030::winit::window::WindowAttributes {
    #[cfg(windows)]
    let attributes = {
        use slint::winit_030::winit::platform::windows::WindowAttributesExtWindows;
        attributes.with_undecorated_shadow(frame::CUSTOM)
    };
    attributes
}
