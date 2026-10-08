//! The window's own frame on both Linux and Windows.
//!
//! The header already serves as the title bar. Asking for system decorations as well
//! adds a second bar on Windows and on Linux desktops such as GNOME. The window asks
//! for no frame and its header carries the controls. A drag is handed to the system's
//! own move, so snapping behaves as it does for any window. Maximizing, minimizing and
//! the resize border are Slint's (see `app.slint`).

use slint::ComponentHandle;

use crate::AppWindow;

#[cfg(target_os = "linux")]
mod shadow;
#[cfg(target_os = "linux")]
mod wayland;

pub fn install(ui: &AppWindow) {
    #[cfg(target_os = "linux")]
    wayland::install(ui);

    let weak = ui.as_weak();
    // As the system's own close button would, so the window decides once whether a close
    // hides it in the tray or ends the program.
    ui.on_close_window(move || {
        if let Some(ui) = weak.upgrade()
            && let Err(error) = ui
                .window()
                .dispatch_event_with_result(slint::platform::WindowEvent::CloseRequested)
        {
            tracing::warn!(%error, "could not close the window");
        }
    });
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use slint::ComponentHandle;
    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{
        Platform, PlatformError, PointerEventButton, WindowAdapter, WindowEvent,
    };

    use crate::AppWindow;

    struct HeadlessPlatform;

    impl Platform for HeadlessPlatform {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, PlatformError> {
            Ok(MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer))
        }
    }

    fn click(window: &slint::Window, x: f32, y: f32) {
        let position = slint::LogicalPosition::new(x, y);
        window.dispatch_event(WindowEvent::PointerMoved { position });
        window.dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        window.dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
    }

    #[test]
    fn a_card_click_opens_details_but_a_drag_does_not() {
        slint::platform::set_platform(Box::new(HeadlessPlatform)).unwrap();
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::PhysicalSize::new(1000, 640));
        ui.window().dispatch_event(WindowEvent::Resized {
            size: slint::LogicalSize::new(1000.0, 640.0),
        });
        ui.set_page(2);
        ui.set_shown_count(2);
        ui.set_cards(slint::ModelRc::new(slint::VecModel::from(vec![
            crate::CardData {
                shown: true,
                slot: 0,
                badge: "+1".into(),
                check_failed: true,
                footer: "check failed".into(),
                error: "network unavailable".into(),
                ..Default::default()
            },
            crate::CardData {
                shown: true,
                slot: 1,
                ..Default::default()
            },
        ])));
        let opened = Rc::new(Cell::new(0));
        ui.on_open_detail({
            let opened = opened.clone();
            move |index| {
                assert_eq!(index, 0);
                opened.set(opened.get() + 1);
            }
        });
        let failures = Rc::new(Cell::new(0));
        ui.on_card_check_error({
            let failures = failures.clone();
            move |error| {
                assert_eq!(error, "network unavailable");
                failures.set(failures.get() + 1);
            }
        });
        let groups = Rc::new(Cell::new(0));
        ui.on_toggle_group({
            let groups = groups.clone();
            move |_| groups.set(groups.get() + 1)
        });
        // The narrow error link is centred in the footer. The scroller's shortened
        // height leaves 6px above and below it, so the card begins at y65.
        click(ui.window(), 188.0, 267.0);
        assert_eq!(failures.get(), 1);
        click(ui.window(), 334.0, 69.0);
        assert_eq!(groups.get(), 1);
        assert_eq!(
            opened.get(),
            0,
            "the error link and account badge keep their own actions"
        );
        let window = ui.window();
        window.dispatch_event(WindowEvent::PointerPressed {
            position: slint::LogicalPosition::new(20.0, 350.0),
            button: PointerEventButton::Left,
        });
        window.dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(100.0, 120.0),
        });
        window.dispatch_event(WindowEvent::PointerReleased {
            position: slint::LogicalPosition::new(100.0, 120.0),
            button: PointerEventButton::Left,
        });
        assert_eq!(
            opened.get(),
            0,
            "a release over a card without pressing it is not a click"
        );
        click(ui.window(), 100.0, 120.0);
        assert_eq!(
            opened.get(),
            1,
            "a primary card click opens its account details"
        );

        let window = ui.window();
        window.dispatch_event(WindowEvent::PointerPressed {
            position: slint::LogicalPosition::new(100.0, 120.0),
            button: PointerEventButton::Left,
        });
        window.dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(150.0, 120.0),
        });
        window.dispatch_event(WindowEvent::PointerReleased {
            position: slint::LogicalPosition::new(150.0, 120.0),
            button: PointerEventButton::Left,
        });
        assert_eq!(
            opened.get(),
            1,
            "reordering must not open a dialog on release"
        );
    }

    #[test]
    fn the_first_button_click_after_a_native_header_drag_is_not_swallowed() {
        // Dispatch input to the real markup, without an OS window or rendering. A
        // compositor can consume the release ending its move, so it is absent below.
        slint::platform::set_platform(Box::new(HeadlessPlatform)).unwrap();
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::PhysicalSize::new(1000, 640));
        ui.set_connected(true);
        let refreshes = Rc::new(Cell::new(0));
        let count = refreshes.clone();
        ui.on_refresh(move || count.set(count.get() + 1));

        click(ui.window(), 825.0, 23.0);
        assert_eq!(refreshes.get(), 1, "the refresh button must be reachable");

        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position: slint::LogicalPosition::new(400.0, 23.0),
            button: PointerEventButton::Left,
        });
        ui.window().dispatch_event(WindowEvent::PointerMoved {
            position: slint::LogicalPosition::new(420.0, 23.0),
        });

        click(ui.window(), 825.0, 23.0);
        assert_eq!(
            refreshes.get(),
            2,
            "the header must release its pointer grab"
        );
    }
}
