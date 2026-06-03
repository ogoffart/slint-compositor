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

use s_compositor_render::{convert_to_rgba, ShmFormat};

use crate::state::{PopupEntry, WindowEntry};
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
        let mut callbacks = Vec::new();

        // Toplevel window?
        if let Some(entry) = self.windows.get(surface) {
            let (id, decorated) = (entry.id, entry.decorated);
            let buffer = extract_buffer(surface, &mut callbacks);
            let title = read_title(surface);
            self.pending_callbacks.append(&mut callbacks);
            if let Some((width, height, pixels)) = buffer {
                let _ = self.events.send(Event::WindowBuffer {
                    id,
                    width,
                    height,
                    pixels,
                    title,
                    decorated,
                });
            }
            return;
        }

        // Popup (menu, dropdown, tooltip)?
        if let Some(entry) = self.popups.get(surface) {
            let (id, parent, offset) = (entry.id, entry.parent_id, entry.offset);
            let buffer = extract_buffer(surface, &mut callbacks);
            self.pending_callbacks.append(&mut callbacks);
            if let Some((width, height, pixels)) = buffer {
                let _ = self.events.send(Event::PopupBuffer {
                    id,
                    parent,
                    ox: offset.0,
                    oy: offset.1,
                    width,
                    height,
                    pixels,
                });
            }
        }
    }
}

/// Drain the surface's frame callbacks into `callbacks` and, if a new shm buffer
/// was attached, copy its pixels out as tightly-packed RGBA8.
fn extract_buffer(
    surface: &WlSurface,
    callbacks: &mut Vec<WlCallback>,
) -> Option<(u32, u32, Vec<u8>)> {
    with_states(surface, |states| {
        let mut guard = states.cached_state.get::<SurfaceAttributes>();
        let attrs = guard.current();

        callbacks.append(&mut attrs.frame_callbacks);

        // Consume any newly attached buffer (clearing it so we don't reprocess).
        let buffer = match attrs.buffer.take() {
            Some(BufferAssignment::NewBuffer(buffer)) => buffer,
            _ => return None,
        };

        let result = with_buffer_contents(&buffer, read_shm);
        // shm contents are copied within the callback; release for reuse.
        buffer.release();

        match result {
            Ok(Some(frame)) => Some(frame),
            Ok(None) => {
                log::debug!("unsupported shm format");
                None
            }
            Err(err) => {
                log::debug!("non-shm buffer ({err:?})");
                None
            }
        }
    })
}

/// Read the toplevel title from a surface's xdg state.
fn read_title(surface: &WlSurface) -> String {
    with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().unwrap().title.clone())
            .unwrap_or_default()
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

    fn new_popup(&mut self, surface: PopupSurface, positioner: PositionerState) {
        let geometry = positioner.get_geometry();
        surface.with_pending_state(|state| {
            state.geometry = geometry;
        });
        let _ = surface.send_configure();

        let parent_id = surface
            .get_parent_surface()
            .and_then(|parent| self.id_of_surface(&parent));
        let id = self.allocate_window_id();
        self.popups.insert(
            surface.wl_surface().clone(),
            PopupEntry {
                id,
                popup: surface,
                parent_id,
                offset: (geometry.loc.x, geometry.loc.y),
            },
        );
        log::info!(
            "new popup {id:?} (parent {parent_id:?}) at {:?}",
            geometry.loc
        );
    }

    fn popup_destroyed(&mut self, surface: PopupSurface) {
        if let Some(entry) = self.popups.remove(surface.wl_surface()) {
            let _ = self.events.send(Event::PopupRemoved(entry.id));
        }
    }

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
        // s-compositor won't draw any.
        self.set_decoration_mode(&toplevel, mode);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        // No preference -> fall back to server-side.
        self.set_decoration_mode(&toplevel, DecorationMode::ServerSide);
    }
}

impl SlickState {
    /// Apply a decoration mode to a toplevel and record whether s-compositor should
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
