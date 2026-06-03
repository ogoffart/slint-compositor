//! Wayland protocol handler implementations.
//!
//! Each `impl` here satisfies a Smithay handler trait and is registered with the
//! matching `delegate_*!` macro at the bottom. For milestones M0/M1 these are
//! deliberately minimal: enough for clients to connect, bind the core globals,
//! and map a toplevel. Rendering of client buffers and input forwarding arrive
//! in M2/M3.

use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_callback::WlCallback;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Client;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    with_states, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SurfaceAttributes,
};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::shm::{with_buffer_contents, BufferData};

use slick_render::{convert_to_rgba, ShmFormat};
use slick_shell::WindowId;

use crate::state::WindowEntry;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    XdgToplevelSurfaceData,
};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::wayland::shm::{ShmHandler, ShmState};
use smithay::{
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
};

use crate::state::{ClientState, SlickState};
use crate::Event;

impl CompositorHandler for SlickState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("client missing ClientState")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        let Some(entry) = self.windows.get(surface) else {
            return;
        };
        let id = entry.id;
        let decorated = entry.decorated;

        // Collect frame callbacks (fired later, throttled) and the new buffer.
        let mut callbacks = Vec::new();
        let buffer_event = extract_commit(surface, id, decorated, &mut callbacks);
        self.pending_callbacks.append(&mut callbacks);
        if let Some(event) = buffer_event {
            let _ = self.events.send(event);
        }
    }
}

/// Drain the surface's pending frame callbacks into `callbacks`, and, if a new
/// shm buffer was attached, copy its pixels into a `WindowBuffer` event.
fn extract_commit(
    surface: &WlSurface,
    id: WindowId,
    decorated: bool,
    callbacks: &mut Vec<WlCallback>,
) -> Option<Event> {
    with_states(surface, |states| {
        let title = states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().unwrap().title.clone())
            .unwrap_or_default();

        let mut guard = states.cached_state.get::<SurfaceAttributes>();
        let attrs = guard.current();

        callbacks.append(&mut attrs.frame_callbacks);

        // Consume any newly attached buffer (clearing it so we don't reprocess).
        let buffer = match attrs.buffer.take() {
            Some(BufferAssignment::NewBuffer(buffer)) => buffer,
            _ => return None,
        };

        let result = with_buffer_contents(&buffer, read_shm);
        // shm contents are copied within the callback; release the buffer so the
        // client can reuse it.
        buffer.release();

        match result {
            Ok(Some((width, height, pixels))) => Some(Event::WindowBuffer {
                id,
                width,
                height,
                pixels,
                title,
                decorated,
            }),
            Ok(None) => {
                log::debug!("window {id:?}: unsupported shm format");
                None
            }
            Err(err) => {
                log::debug!("window {id:?}: non-shm buffer ({err:?})");
                None
            }
        }
    })
}

/// Read an shm buffer's contents into a tightly-packed RGBA8 buffer.
fn read_shm(ptr: *const u8, len: usize, data: BufferData) -> Option<(u32, u32, Vec<u8>)> {
    let format = match data.format {
        wl_shm::Format::Argb8888 => ShmFormat::Argb8888,
        wl_shm::Format::Xrgb8888 => ShmFormat::Xrgb8888,
        _ => return None,
    };
    if data.width <= 0 || data.height <= 0 {
        return None;
    }
    let offset = data.offset.max(0) as usize;
    if offset > len {
        return None;
    }
    // SAFETY: `ptr` is valid for `len` bytes for the duration of this callback,
    // and we only read within `[offset, len)`.
    let slice = unsafe { std::slice::from_raw_parts(ptr.add(offset), len - offset) };
    let rgba = convert_to_rgba(
        slice,
        data.width as usize,
        data.height as usize,
        data.stride as usize,
        format,
    );
    Some((data.width as u32, data.height as u32, rgba))
}

impl BufferHandler for SlickState {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for SlickState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for SlickState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Send an initial empty configure so the client can map itself.
        surface.send_configure();

        let id = self.allocate_window_id();
        self.workspaces.add_window(id);
        let wl_surface = surface.wl_surface().clone();
        self.windows.insert(
            wl_surface,
            WindowEntry {
                id,
                toplevel: surface,
                decorated: true,
            },
        );
        let _ = self.events.send(Event::WindowAdded(id));
        log::info!("new toplevel -> window {:?}", id);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(entry) = self.windows.remove(surface.wl_surface()) {
            self.workspaces.remove_window(entry.id);
            let _ = self.events.send(Event::WindowRemoved(entry.id));
            log::info!("toplevel destroyed -> window {:?}", entry.id);
        }
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}

    fn grab(
        &mut self,
        _surface: PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
    }

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }
}

impl XdgDecorationHandler for SlickState {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        // Default to server-side; clients that prefer their own decorations will
        // follow up with a request for ClientSide.
        self.set_decoration_mode(&toplevel, DecorationMode::ServerSide);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, mode: DecorationMode) {
        // Honor the client's preference: if it wants client-side decorations,
        // slick won't draw any.
        self.set_decoration_mode(&toplevel, mode);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        // No preference -> fall back to server-side.
        self.set_decoration_mode(&toplevel, DecorationMode::ServerSide);
    }
}

impl SlickState {
    /// Apply a decoration mode to a toplevel and record whether slick should
    /// draw decorations for it.
    fn set_decoration_mode(&mut self, toplevel: &ToplevelSurface, mode: DecorationMode) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(mode);
        });
        toplevel.send_configure();
        let decorated = mode == DecorationMode::ServerSide;
        let id = self.windows.get_mut(toplevel.wl_surface()).map(|entry| {
            entry.decorated = decorated;
            entry.id
        });
        if let Some(id) = id {
            log::info!("window {id:?}: decoration mode {mode:?} (decorated={decorated})");
            let _ = self.events.send(Event::WindowDecorated { id, decorated });
        }
    }
}

impl SeatHandler for SlickState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}

    fn cursor_image(
        &mut self,
        _seat: &Seat<Self>,
        _image: smithay::input::pointer::CursorImageStatus,
    ) {
    }
}

impl SelectionHandler for SlickState {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        _ty: SelectionTarget,
        _source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
    }
}

impl DataDeviceHandler for SlickState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for SlickState {}
impl ServerDndGrabHandler for SlickState {}

impl OutputHandler for SlickState {}

delegate_compositor!(SlickState);
delegate_shm!(SlickState);
delegate_xdg_shell!(SlickState);
smithay::delegate_xdg_decoration!(SlickState);
delegate_seat!(SlickState);
delegate_output!(SlickState);
delegate_data_device!(SlickState);
