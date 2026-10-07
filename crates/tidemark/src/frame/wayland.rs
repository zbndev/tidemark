//! Client decoration shadow as a separate Wayland subsurface below the content.
//!
//! Winit keeps xdg_surface's geometry equal to the client rectangle. A subsurface
//! can extend outside it without changing snapping, resize hit targets or Slint
//! coordinates. An empty input region makes the entire shadow click-through.

use std::io::Write;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use slint::ComponentHandle;
use slint::winit_030::{WinitWindowAccessor, winit};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_subcompositor,
    wl_subsurface, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, delegate_noop};

use super::shadow::{MARGIN, pixels};

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("could not access the native Wayland handles: {0}")]
    Handle(#[from] winit::raw_window_handle::HandleError),
    #[error("the borrowed Wayland surface is invalid: {0}")]
    Surface(#[from] wayland_client::backend::InvalidId),
    #[error("could not dispatch shadow events: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("the compositor has no {0}")]
    MissingGlobal(&'static str),
    #[error("could not allocate the shadow buffer: {0}")]
    Buffer(#[from] std::io::Error),
    #[error("the shadow buffer dimensions are too large")]
    Dimensions,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Size {
    width: u32,
    height: u32,
    scale: i32,
    active: bool,
}

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    subcompositor: Option<wl_subcompositor::WlSubcompositor>,
    buffers: Vec<wl_buffer::WlBuffer>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_compositor" if version >= 3 => {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()));
                }
                "wl_shm" => state.shm = Some(registry.bind(name, 1, qh, ())),
                "wl_subcompositor" => {
                    state.subcompositor = Some(registry.bind(name, 1, qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_buffer::WlBuffer, ()> for State {
    fn event(
        state: &mut Self,
        buffer: &wl_buffer::WlBuffer,
        event: wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            // Every buffer is immutable. The compositor can retain an old one during
            // resize; destroy it only once it has finished using its storage.
            buffer.destroy();
            state.buffers.retain(|pending| pending != buffer);
        }
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_surface::WlSurface);
delegate_noop!(State: ignore wl_region::WlRegion);
delegate_noop!(State: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(State: ignore wl_subsurface::WlSubsurface);

struct Shadow {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    surface: wl_surface::WlSurface,
    subsurface: wl_subsurface::WlSubsurface,
    size: Option<Size>,
}

impl Shadow {
    fn new(connection: Connection, parent: &wl_surface::WlSurface) -> Result<Self, Error> {
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        let registry = connection.display().get_registry(&qh, ());
        let mut state = State::default();
        queue.roundtrip(&mut state)?;
        let compositor = state
            .compositor
            .as_ref()
            .ok_or(Error::MissingGlobal("wl_compositor v3"))?;
        let subcompositor = state
            .subcompositor
            .as_ref()
            .ok_or(Error::MissingGlobal("wl_subcompositor"))?;
        if state.shm.is_none() {
            return Err(Error::MissingGlobal("wl_shm"));
        }
        let surface = compositor.create_surface(&qh, ());
        let region = compositor.create_region(&qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        let subsurface = subcompositor.get_subsurface(&surface, parent, &qh, ());
        subsurface.set_position(-MARGIN, -MARGIN);
        subsurface.place_below(parent);
        // Synchronized by default: shadow commits apply atomically with Slint's
        // next parent commit, including configure/resize and maximize transitions.
        connection.backend().destroy_object(&registry.id())?;
        Ok(Self {
            connection,
            queue,
            state,
            surface,
            subsurface,
            size: None,
        })
    }

    fn update(&mut self, size: Option<Size>) -> Result<(), Error> {
        self.queue.dispatch_pending(&mut self.state)?;
        let Some(size) = size else {
            if self.size.take().is_some() {
                self.surface.attach(None, 0, 0);
                self.surface.commit();
            }
            return Ok(());
        };
        if self.size == Some(size) || size.width == 0 || size.height == 0 {
            return Ok(());
        }
        let width = i32::try_from(size.width)
            .ok()
            .and_then(|w| w.checked_add(2 * MARGIN))
            .and_then(|w| w.checked_mul(size.scale))
            .ok_or(Error::Dimensions)?;
        let height = i32::try_from(size.height)
            .ok()
            .and_then(|h| h.checked_add(2 * MARGIN))
            .and_then(|h| h.checked_mul(size.scale))
            .ok_or(Error::Dimensions)?;
        let length = width
            .checked_mul(height)
            .and_then(|area| area.checked_mul(4))
            .filter(|length| *length <= 128 * 1024 * 1024)
            .ok_or(Error::Dimensions)?;
        let mut file = tempfile::tempfile()?;
        file.write_all(&pixels(size.width, size.height, size.scale, size.active))?;
        let qh = self.queue.handle();
        let pool = self
            .state
            .shm
            .as_ref()
            .ok_or(Error::MissingGlobal("wl_shm"))?
            .create_pool(file.as_fd(), length, &qh, ());
        let buffer = pool.create_buffer(
            0,
            width,
            height,
            width * 4,
            wl_shm::Format::Argb8888,
            &qh,
            (),
        );
        pool.destroy();
        self.surface.set_buffer_scale(size.scale);
        self.surface.attach(Some(&buffer), 0, 0);
        self.surface.damage(0, 0, i32::MAX, i32::MAX);
        self.surface.commit();
        self.state.buffers.push(buffer);
        self.size = Some(size);
        Ok(())
    }
}

impl Drop for Shadow {
    fn drop(&mut self) {
        self.subsurface.destroy();
        self.surface.destroy();
        for buffer in self.state.buffers.drain(..) {
            buffer.destroy();
        }
        if let Some(subcompositor) = &self.state.subcompositor {
            subcompositor.destroy();
        }
        // wl_compositor v4 and wl_shm v1 have no protocol destructor. The guest
        // backend destroys their local proxies; the host connection remains open.
        let _ = self.connection.flush();
    }
}

#[cfg(test)]
mod tests;

struct NativeShadow {
    decoration: Shadow,
    // Last: the guest connection and its objects must be destroyed before Winit can
    // destroy the host display. RenderingTeardown releases this before tray hiding.
    window: Arc<winit::window::Window>,
}

impl NativeShadow {
    fn new(window: Arc<winit::window::Window>) -> Result<Option<Self>, Error> {
        let Some((connection, parent)) = borrowed::connection_and_surface(&window)? else {
            return Ok(None);
        };
        let decoration = Shadow::new(connection, &parent)?;
        Ok(Some(Self { decoration, window }))
    }

    fn update(&mut self) -> Result<(), Error> {
        let size = if self.window.is_maximized() || self.window.fullscreen().is_some() {
            None
        } else {
            let scale = self.window.scale_factor();
            let logical = self.window.inner_size().to_logical::<f64>(scale);
            Some(Size {
                width: logical.width.ceil() as u32,
                height: logical.height.ceil() as u32,
                scale: scale.ceil() as i32,
                active: self.window.has_focus(),
            })
        };
        self.decoration.update(size)
    }
}

pub(super) fn install(ui: &crate::AppWindow) {
    let weak_ui = ui.as_weak();
    let mut shadow = None;
    let mut unavailable = false;
    if let Err(error) = ui.window().set_rendering_notifier(move |state, _| {
        match state {
            slint::RenderingState::BeforeRendering => {
                if unavailable {
                    return;
                }
                if shadow.is_none() {
                    let Some(ui) = weak_ui.upgrade() else {
                        return;
                    };
                    // A native window exists during rendering. Polling the public
                    // accessor here obtains an Arc without scheduling another task.
                    let future = ui.window().winit_window();
                    let Poll::Ready(Ok(native)) =
                        std::pin::pin!(future).poll(&mut Context::from_waker(Waker::noop()))
                    else {
                        return;
                    };
                    match NativeShadow::new(native) {
                        Ok(Some(created)) => shadow = Some(created),
                        Ok(None) => unavailable = true,
                        Err(error) => {
                            tracing::warn!(%error, "could not create the Wayland window shadow");
                            unavailable = true;
                        }
                    }
                }
                if let Some(shadow) = &mut shadow
                    && let Err(error) = shadow.update()
                {
                    tracing::warn!(%error, "could not update the Wayland window shadow");
                    unavailable = true;
                }
            }
            slint::RenderingState::RenderingTeardown => {
                shadow.take();
                unavailable = false;
            }
            _ => {}
        }
    }) {
        tracing::warn!(%error, "the renderer cannot maintain the Wayland window shadow");
    }
}

// The only unsafe bridge: Winit owns the foreign display and parent surface. Its
// Arc is kept by Shadow until all guest objects and the guest backend are gone.
#[allow(unsafe_code)]
mod borrowed {
    use super::*;
    use winit::raw_window_handle::{
        HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
    };

    pub(super) fn connection_and_surface(
        window: &winit::window::Window,
    ) -> Result<Option<(Connection, wl_surface::WlSurface)>, Error> {
        let RawDisplayHandle::Wayland(display) = window.display_handle()?.as_raw() else {
            return Ok(None);
        };
        let RawWindowHandle::Wayland(surface) = window.window_handle()?.as_raw() else {
            return Ok(None);
        };
        // SAFETY: These handles come from a live Winit Window. The caller retains
        // its Arc for the entire guest backend's lifetime. Guest mode never closes
        // Winit's display and no listener on Winit's parent surface is replaced.
        let backend = unsafe {
            wayland_client::backend::Backend::from_foreign_display(display.display.as_ptr().cast())
        };
        let connection = Connection::from_backend(backend);
        // SAFETY: The parent is Winit's live wl_surface and is borrowed only to
        // establish/stack a child. It is never committed, destroyed or stored by us.
        let id = unsafe {
            wayland_client::backend::ObjectId::from_ptr(
                wl_surface::WlSurface::interface(),
                surface.surface.as_ptr().cast(),
            )?
        };
        let parent = wl_surface::WlSurface::from_id(&connection, id)?;
        Ok(Some((connection, parent)))
    }
}
