//! Tidemark's desktop client, drawn with Slint. It speaks to `tidemarkd` over D-Bus and
//! nothing else.

// A desktop client must not keep a console window: on Windows the GUI subsystem detaches
// it at link time. Gated off tests so failures still print.
#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

mod about;
mod alert;
#[cfg(unix)]
mod application;
mod bus;
mod chart;
#[cfg(windows)]
mod daemon_job;
mod detail;
#[cfg(windows)]
mod file_log;
mod format;
mod frame;
#[cfg(windows)]
mod maintenance;
mod markdown;
mod marks;
mod model;
#[cfg(unix)]
mod portal;
mod preferences;
mod provider_settings;
#[cfg(windows)]
mod registry;
mod release_notes;
#[cfg(windows)]
mod single_instance;
mod tray;
#[cfg(windows)]
mod tray_icon_rgba;
mod update;
mod view;
mod window;

// The code slint-build generates from `ui/`, which does not derive Debug.
#[allow(missing_debug_implementations, clippy::all, clippy::todo)]
mod ui {
    slint::include_modules!();
}
use ui::{
    About, Alert, AlertForm, AlertResponse, AppWindow, CandidateData, CardData, DetailData,
    GaugeData, MenuEntry, NoteBlock, OptionData, PickerRowData, Prefs, PreviewRow, ProviderRowData,
    ProviderSettings, QuotaDetailRow, QuotaDetails, QuotaSectionData, QuotaWindowData,
    ReleaseNotes, RowData, SwitchData, Theme,
};

fn main() -> Result<(), slint::PlatformError> {
    // Startup metadata must be captured/cleared while the process is still single
    // threaded. It is forwarded to the existing client or used by the first window.
    #[cfg(unix)]
    let launch = application::Launch::from_env_at_startup();

    #[cfg(windows)]
    let sink = file_log::init()
        .map(file_log::Sink::File)
        .unwrap_or(file_log::Sink::Stderr);
    let subscriber = tracing_subscriber::fmt().with_env_filter(
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "tidemark=info".into()),
    );
    // No console on Windows (GUI subsystem above): without the file the client would be
    // mute anywhere.
    #[cfg(windows)]
    let subscriber = subscriber.with_writer(sink).with_ansi(false);
    subscriber.init();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|argument| argument == "--version") {
        println!("tidemark {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    // The session's autostart: the window stays hidden, and the process stays only once a
    // tray icon can bring it back (CONTEXT.md § Interface).
    let background = args.iter().any(|argument| argument == "--background");

    #[cfg(windows)]
    if maintenance::active().map_err(|error| slint::PlatformError::from(error.to_string()))? {
        return Ok(());
    }

    #[cfg(windows)]
    if let Err(error) = single_instance::wait_for_restart() {
        tracing::error!(%error, "could not wait for the previous desktop client");
        return Err(format!("could not restart Tidemark: {error}").into());
    }

    #[cfg(unix)]
    let instance = match async_io::block_on(application::claim(!background, &launch)) {
        Ok(application::Claim::First(connection)) => Some(connection),
        Ok(application::Claim::Running) => return Ok(()),
        Err(error) => {
            tracing::warn!(%error, "no session bus to single-instance on; starting unguarded");
            None
        }
    };

    // No session bus on Windows, so single-instance activation uses the daemon: a second
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

    // FemtoVG: Slint's Skia renderer hints each glyph's outline while parley places it at
    // unhinted advances, so small text comes out with letters crowding or drifting apart.
    // On Windows it runs on wgpu (Direct3D 12). On Linux on OpenGL: Wayland cannot hide a
    // window, only destroy it, and NVIDIA's Vulkan driver crashes creating the swapchain
    // for the window brought back from the tray. SLINT_BACKEND still overrides this for
    // comparisons; the platform is selected either way so the app ID below can be set.
    // On Windows, through DirectComposition first (see `composited`), unless SLINT_BACKEND
    // or WGPU_BACKEND asks for something else.
    #[cfg(windows)]
    let composited = std::env::var_os("SLINT_BACKEND").is_none()
        && std::env::var_os("WGPU_BACKEND").is_none()
        && slint::BackendSelector::new()
            .with_winit_window_attributes_hook(|attributes| {
                surface(window_attributes(attributes), true)
            })
            .renderer_name(RENDERER.into())
            .require_wgpu_30(composited())
            .select()
            .inspect_err(|error| {
                tracing::warn!(%error, "drawing on an opaque surface, without rounded corners");
            })
            .is_ok();
    #[cfg(not(windows))]
    let composited = false;
    #[cfg(target_os = "linux")]
    let launch = std::cell::RefCell::new(launch);
    let renderer = if composited {
        format!("{RENDERER} (Direct3D 12 through DirectComposition)")
    } else {
        let selector =
            slint::BackendSelector::new().with_winit_window_attributes_hook(move |attributes| {
                let attributes = window_attributes(attributes);
                #[cfg(windows)]
                let attributes = surface(attributes, false);
                #[cfg(target_os = "linux")]
                let attributes = launch.borrow_mut().window_attributes(attributes);
                attributes
            });
        match std::env::var("SLINT_BACKEND") {
            Ok(backend) => match selector.select() {
                Ok(()) => format!("SLINT_BACKEND={backend}"),
                Err(error) => format!("Slint's default (SLINT_BACKEND={backend} failed: {error})"),
            },
            Err(_) => match selector.renderer_name(RENDERER.into()).select() {
                Ok(()) => RENDERER.to_owned(),
                Err(error) => {
                    tracing::warn!(%error, renderer = RENDERER, "using Slint's default renderer");
                    format!("Slint's default ({RENDERER} failed: {error})")
                }
            },
        }
    };
    // The Wayland app ID and X11 class: the desktop file's name, so compositors' rules
    // and the dock treat this window as Tidemark.
    if let Err(error) = slint::set_xdg_app_id(tidemark_types::ids::APP_ID) {
        tracing::warn!(%error, "could not set the application ID");
    }

    let ui = AppWindow::new()?;
    // Windows 11 rounds the window as well, at 8px, so the window's own corners follow
    // its curve. An opaque surface has none of its own to show.
    #[cfg(windows)]
    ui.set_corner_radius(if composited { 8.0 } else { 0.0 });
    #[cfg(windows)]
    maintenance::watch_stop().map_err(|error| slint::PlatformError::from(error.to_string()))?;
    frame::install(&ui);
    about::install(&ui);
    release_notes::install(&ui);
    let _main = window::MainWindow::start(&ui, renderer, background);
    #[cfg(unix)]
    if let Some(connection) = &instance
        && let Err(error) = async_io::block_on(application::serve(connection, &ui))
    {
        tracing::warn!(%error, "a second launch will not be able to raise this window");
    }
    tracing::info!(background, "starting desktop client");
    if !background {
        slint::ComponentHandle::show(&ui)?;
    }
    // Until told to quit, not until the last window is hidden: a window closed to the tray
    // is hidden, and the program is still running.
    slint::run_event_loop_until_quit()
}

#[cfg(windows)]
const RENDERER: &str = "femtovg-wgpu";
#[cfg(not(windows))]
const RENDERER: &str = "femtovg";

/// A window that draws its own frame still wants the system's shadow and, on Windows 11,
/// its rounded corners around it. Windows 10 has no rounded corners and ignores the
/// preference.
fn window_attributes(
    attributes: slint::winit_030::winit::window::WindowAttributes,
) -> slint::winit_030::winit::window::WindowAttributes {
    #[cfg(windows)]
    let attributes = {
        use slint::winit_030::winit::platform::windows::{
            CornerPreference, WindowAttributesExtWindows,
        };
        let icon = slint::winit_030::winit::window::Icon::from_rgba(
            tray_icon_rgba::ICON_RGBA.to_vec(),
            32,
            32,
        )
        .ok();
        attributes
            .with_undecorated_shadow(true)
            .with_corner_preference(CornerPreference::Round)
            .with_window_icon(icon)
    };
    attributes
}

/// Direct3D 12 presenting through a DirectComposition visual. A swap chain on the window
/// itself, and Vulkan, which wgpu would otherwise try first, are opaque on Windows: the
/// transparent corners come out in whatever colour the driver leaves there. A visual
/// keeps the scene's alpha, so the corners show the desktop on Windows 10 as well.
#[cfg(windows)]
fn composited() -> slint::wgpu_30::WGPUConfiguration {
    use slint::wgpu_30::wgpu;
    let mut settings = slint::wgpu_30::WGPUSettings::default();
    settings.backends = wgpu::Backends::DX12;
    settings.backend_options.dx12.presentation_system = wgpu::Dx12SwapchainKind::DxgiFromVisual;
    slint::wgpu_30::WGPUConfiguration::Automatic(settings)
}

/// Whether the window's surface keeps its alpha. A visual covers the window, so it has no
/// redirection bitmap of its own to show through the corners. Otherwise the window is not
/// transparent: Slint asks for that by default, and DWM would only blur behind it.
#[cfg(windows)]
fn surface(
    attributes: slint::winit_030::winit::window::WindowAttributes,
    composited: bool,
) -> slint::winit_030::winit::window::WindowAttributes {
    use slint::winit_030::winit::platform::windows::WindowAttributesExtWindows;
    if composited {
        attributes.with_no_redirection_bitmap(true)
    } else {
        attributes.with_transparent(false)
    }
}
