//! Tidemark's desktop client, drawn with Slint.
//!
//! A prototype beside the GTK client: the same daemon, the same D-Bus contract, the same
//! decisions about what a card says and where it goes — `model`, `format` and `update` are
//! the GTK client's own files, compiled in unchanged — and a different toolkit drawing them.

mod bus;
mod marks;
mod portal;
mod view;
mod window;

#[allow(dead_code, unused_imports)]
#[path = "../../tidemark/src/format.rs"]
mod format;
#[allow(dead_code, unused_imports)]
#[path = "../../tidemark/src/model.rs"]
mod model;
#[allow(dead_code, unused_imports)]
#[path = "../../tidemark/src/update.rs"]
mod update;

// The code slint-build generates from `ui/`, which does not derive Debug.
#[allow(missing_debug_implementations, clippy::all, clippy::todo)]
mod ui {
    slint::include_modules!();
}
use ui::{AppWindow, CardData, GaugeData, RowData, Theme};

fn main() -> Result<(), slint::PlatformError> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "tidemark_slint=info".into()),
        )
        .init();

    if std::env::args().any(|argument| argument == "--version") {
        println!("tidemark-slint {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // FemtoVG on wgpu: Direct3D 12 on Windows, Vulkan on Linux. Not Skia — Slint's Skia
    // renderer hints each glyph's outline while parley places it at unhinted advances, so
    // small text comes out with letters crowding or drifting apart. SLINT_BACKEND still
    // overrides this for comparisons.
    if std::env::var_os("SLINT_BACKEND").is_none()
        && let Err(error) = slint::BackendSelector::new()
            .renderer_name("femtovg-wgpu".into())
            .select()
    {
        tracing::warn!(%error, "wgpu is unavailable; using Slint's default renderer");
    }

    let ui = AppWindow::new()?;
    let _main = window::MainWindow::start(&ui);
    slint::ComponentHandle::run(&ui)
}
