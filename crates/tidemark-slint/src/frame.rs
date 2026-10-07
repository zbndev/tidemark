//! The window's own frame, where the system's would be wrong.
//!
//! The header bar is libadwaita's, and on Windows the system adds its title bar above it:
//! two bars, one with the wrong buttons. There the window asks for no frame and the header
//! takes over: it carries the window controls, and a drag on it is handed to the system's
//! own move, so snapping behaves as it does for any window. Maximizing, minimizing and the
//! resize border are Slint's (see `app.slint`).
//!
//! Linux keeps the compositor's decision: a tiling compositor draws nothing and a
//! floating one draws what its user chose.

use slint::ComponentHandle;
use slint::winit_030::WinitWindowAccessor;

use crate::AppWindow;

/// Whether this platform gets the window's own frame.
pub const CUSTOM: bool = cfg!(windows);

pub fn install(ui: &AppWindow) {
    ui.set_custom_frame(CUSTOM);
    if !CUSTOM {
        return;
    }

    let weak = ui.as_weak();
    ui.on_move_window(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        ui.window().with_winit_window(|window| {
            if let Err(error) = window.drag_window() {
                tracing::warn!(%error, "the system would not move the window");
            }
        });
    });

    let weak = ui.as_weak();
    ui.on_close_window(move || {
        if let Some(ui) = weak.upgrade()
            && let Err(error) = ui.window().hide()
        {
            tracing::warn!(%error, "could not close the window");
        }
    });
}
