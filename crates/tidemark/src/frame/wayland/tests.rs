//! A protocol-only compositor over a socket pair: no desktop, OS window or rendering.
//! This checks the guest connection against an independently dispatched server.

use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use wayland_server::protocol as server;
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use super::{Connection, Proxy, Shadow, Size, State, wl_surface};

#[derive(Default, Debug)]
struct Observed {
    parent: u32,
    child: u32,
    position: Option<(i32, i32)>,
    below: Option<u32>,
    empty_input: bool,
    scale: i32,
    buffers: Vec<(i32, i32, i32)>,
    detaches: usize,
    parent_commits: usize,
    child_destroyed: bool,
}

struct ServerState(Arc<Mutex<Observed>>);

macro_rules! global {
    ($interface:ty) => {
        impl GlobalDispatch<$interface, ()> for ServerState {
            fn bind(
                _: &mut Self,
                _: &DisplayHandle,
                _: &Client,
                resource: New<$interface>,
                _: &(),
                init: &mut DataInit<'_, Self>,
            ) {
                init.init(resource, ());
            }
        }
    };
}
global!(server::wl_compositor::WlCompositor);
global!(server::wl_shm::WlShm);
global!(server::wl_subcompositor::WlSubcompositor);

impl Dispatch<server::wl_compositor::WlCompositor, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &server::wl_compositor::WlCompositor,
        request: server::wl_compositor::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            server::wl_compositor::Request::CreateSurface { id } => {
                let surface = init.init(id, ());
                let mut observed = state.0.lock().unwrap();
                if observed.parent == 0 {
                    observed.parent = surface.id().protocol_id();
                }
            }
            server::wl_compositor::Request::CreateRegion { id } => {
                init.init(id, false);
            }
            _ => {}
        }
    }
}

impl Dispatch<server::wl_subcompositor::WlSubcompositor, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &server::wl_subcompositor::WlSubcompositor,
        request: server::wl_subcompositor::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let server::wl_subcompositor::Request::GetSubsurface {
            id,
            surface,
            parent,
        } = request
        {
            init.init(id, ());
            let mut observed = state.0.lock().unwrap();
            assert_eq!(parent.id().protocol_id(), observed.parent);
            observed.child = surface.id().protocol_id();
        }
    }
}

impl Dispatch<server::wl_subsurface::WlSubsurface, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &server::wl_subsurface::WlSubsurface,
        request: server::wl_subsurface::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let mut observed = state.0.lock().unwrap();
        match request {
            server::wl_subsurface::Request::SetPosition { x, y } => {
                observed.position = Some((x, y))
            }
            server::wl_subsurface::Request::PlaceBelow { sibling } => {
                observed.below = Some(sibling.id().protocol_id())
            }
            server::wl_subsurface::Request::SetDesync => {
                panic!("shadow must commit with its parent")
            }
            _ => {}
        }
    }
}

impl Dispatch<server::wl_surface::WlSurface, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        surface: &server::wl_surface::WlSurface,
        request: server::wl_surface::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        let mut observed = state.0.lock().unwrap();
        match request {
            server::wl_surface::Request::SetInputRegion { region } => {
                observed.empty_input =
                    region.is_some_and(|region| !*region.data::<bool>().unwrap());
            }
            server::wl_surface::Request::SetBufferScale { scale } => observed.scale = scale,
            server::wl_surface::Request::Attach { buffer: None, .. } => observed.detaches += 1,
            server::wl_surface::Request::Attach {
                buffer: Some(buffer),
                ..
            } => buffer.release(),
            server::wl_surface::Request::Commit
                if surface.id().protocol_id() == observed.parent =>
            {
                observed.parent_commits += 1
            }
            server::wl_surface::Request::Destroy
                if surface.id().protocol_id() == observed.child =>
            {
                observed.child_destroyed = true
            }
            _ => {}
        }
    }
}

impl Dispatch<server::wl_shm::WlShm, ()> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &server::wl_shm::WlShm,
        request: server::wl_shm::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let server::wl_shm::Request::CreatePool { id, .. } = request {
            init.init(id, ());
        }
    }
}

impl Dispatch<server::wl_shm_pool::WlShmPool, ()> for ServerState {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &server::wl_shm_pool::WlShmPool,
        request: server::wl_shm_pool::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let server::wl_shm_pool::Request::CreateBuffer {
            id,
            width,
            height,
            stride,
            ..
        } = request
        {
            init.init(id, ());
            state
                .0
                .lock()
                .unwrap()
                .buffers
                .push((width, height, stride));
        }
    }
}

impl Dispatch<server::wl_region::WlRegion, bool> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &server::wl_region::WlRegion,
        request: server::wl_region::Request,
        _: &bool,
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        assert!(
            !matches!(request, server::wl_region::Request::Add { .. }),
            "the shadow must not take any input"
        );
    }
}

impl Dispatch<server::wl_buffer::WlBuffer, ()> for ServerState {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &server::wl_buffer::WlBuffer,
        _: server::wl_buffer::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}

#[test]
#[allow(unsafe_code)]
fn the_shadow_is_click_through_and_does_not_modify_the_host_surface() {
    let (client_socket, server_socket) = UnixStream::pair().unwrap();
    let observed = Arc::new(Mutex::new(Observed::default()));
    let mut display = wayland_server::Display::<ServerState>::new().unwrap();
    let mut handle = display.handle();
    handle.create_global::<ServerState, server::wl_compositor::WlCompositor, _>(4, ());
    handle.create_global::<ServerState, server::wl_shm::WlShm, _>(1, ());
    handle.create_global::<ServerState, server::wl_subcompositor::WlSubcompositor, _>(1, ());
    handle.insert_client(server_socket, Arc::new(())).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_server = stop.clone();
    let observed_server = observed.clone();
    let server = std::thread::spawn(move || {
        let mut state = ServerState(observed_server);
        while !stop_server.load(Ordering::Relaxed) {
            display.dispatch_clients(&mut state).unwrap();
            display.flush_clients().unwrap();
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let host = Connection::from_socket(client_socket).unwrap();
    let mut host_queue = host.new_event_queue();
    let mut host_state = State::default();
    host.display().get_registry(&host_queue.handle(), ());
    host_queue.roundtrip(&mut host_state).unwrap();
    let parent = host_state
        .compositor
        .as_ref()
        .unwrap()
        .create_surface(&host_queue.handle(), ());
    // SAFETY: Host display and parent are held until the guest shadow is dropped.
    // The guest only borrows the parent, exactly as it borrows Winit's surface.
    let backend = unsafe {
        wayland_client::backend::Backend::from_foreign_display(host.backend().display_ptr())
    };
    let guest = Connection::from_backend(backend);
    let id = unsafe {
        wayland_client::backend::ObjectId::from_ptr(
            wl_surface::WlSurface::interface(),
            parent.id().as_ptr(),
        )
        .unwrap()
    };
    let borrowed_parent = wl_surface::WlSurface::from_id(&guest, id).unwrap();
    let mut shadow = Shadow::new(guest, &borrowed_parent).unwrap();
    shadow
        .update(Some(Size {
            width: 400,
            height: 300,
            scale: 2,
            active: true,
        }))
        .unwrap();
    host_queue.roundtrip(&mut host_state).unwrap();
    {
        let seen = observed.lock().unwrap();
        assert!(seen.empty_input);
        assert_eq!(seen.position, Some((-32, -32)));
        assert_eq!(seen.below, Some(seen.parent));
        assert_eq!(seen.scale, 2);
        assert_eq!(seen.buffers, [(928, 728, 3712)]);
        assert_eq!(seen.parent_commits, 0);
    }
    shadow.update(None).unwrap(); // Maximize/full-screen: detach the shadow.
    host_queue.roundtrip(&mut host_state).unwrap();
    assert_eq!(observed.lock().unwrap().detaches, 1);
    shadow
        .update(Some(Size {
            width: 400,
            height: 300,
            scale: 1,
            active: false,
        }))
        .unwrap();
    drop(shadow); // Tray hiding: child resources must go before the host window.
    host_queue.roundtrip(&mut host_state).unwrap();
    assert!(observed.lock().unwrap().child_destroyed);
    // The guest must not destroy the host's display or replace its listeners.
    parent.commit();
    host_queue.roundtrip(&mut host_state).unwrap();
    assert_eq!(observed.lock().unwrap().parent_commits, 1);
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
}
