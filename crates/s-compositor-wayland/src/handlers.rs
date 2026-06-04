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
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::Client;
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    with_states, BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SurfaceAttributes,
};
use smithay::wayland::output::OutputHandler;
use smithay::wayland::shell::wlr_layer::{
    Anchor, Layer, LayerSurface, LayerSurfaceCachedState, Margins, WlrLayerShellHandler,
    WlrLayerShellState,
};
use smithay::wayland::shm::{with_buffer_contents, BufferData};

use s_compositor_render::{convert_to_rgba, ShmFormat};

use crate::state::{LayerEntry, PopupEntry, WindowEntry};
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::wayland::shell::xdg::decoration::XdgDecorationHandler;
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler,
    XdgShellState, XdgToplevelSurfaceData,
};
use smithay::utils::{Logical, Rectangle};
use smithay::reexports::wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode;
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
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
        // XWayland's client is created by smithay with its own data type, not our
        // ClientState; handle both so the compositor doesn't panic when X11 apps
        // (via XWayland) connect.
        if let Some(state) = client.get_data::<smithay::xwayland::XWaylandClientData>() {
            return &state.compositor_state;
        }
        &client
            .get_data::<ClientState>()
            .expect("client missing ClientState")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // A commit may arrive on a subsurface (desync mode) rather than the
        // window's root surface. Normalize to the root so we always re-flatten
        // the whole tree and answer every surface's frame callbacks — otherwise
        // multi-surface clients (GTK, Firefox) stall waiting on callbacks we'd
        // never fire.
        let root = root_surface(surface);
        let mut callbacks = Vec::new();

        // Toplevel window?
        if let Some(entry) = self.windows.get(&root) {
            let (id, decorated) = (entry.id, entry.decorated);
            // GPU (dmabuf) buffer? Hand the planes to the UI thread to import.
            if self.dmabuf_enabled {
                if let Some(event) = take_dmabuf(&root, id, &mut callbacks) {
                    self.pending_callbacks.append(&mut callbacks);
                    let _ = self.events.send(event);
                    return;
                }
            }
            let title = read_title(&root);
            let app_id = read_app_id(&root);
            let mut cache = std::mem::take(&mut self.surface_pixels);
            let buffer = composite_tree(&root, &mut cache, &mut callbacks);
            self.surface_pixels = cache;
            self.pending_callbacks.append(&mut callbacks);
            if let Some(buffer) = buffer {
                // Crop to the client's declared window geometry so we drop the
                // transparent client-side-decoration shadow margin (otherwise it
                // shows as a big box around the window). Record the crop offset so
                // pointer input maps back to surface-local coordinates.
                let geometry = window_geometry(&root);
                let ((width, height, pixels), offset) = crop_to_geometry(buffer, geometry);
                if let Some(entry) = self.windows.get_mut(&root) {
                    entry.geometry_offset = offset;
                }
                let _ = self.events.send(Event::WindowBuffer {
                    id,
                    width,
                    height,
                    pixels,
                    title,
                    app_id,
                    decorated,
                });
            }
            return;
        }

        // Popup (menu, dropdown, tooltip)?
        if let Some(entry) = self.popups.get(&root) {
            let (id, parent, offset) = (entry.id, entry.parent_id, entry.offset);
            let mut cache = std::mem::take(&mut self.surface_pixels);
            let buffer = composite_tree(&root, &mut cache, &mut callbacks);
            self.surface_pixels = cache;
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
            return;
        }

        // Layer-shell surface (bar, wallpaper, notification)?
        if let Some(entry) = self.layer_surfaces.get(&root) {
            let (id, layer) = (entry.id, entry.layer);
            let mut cache = std::mem::take(&mut self.surface_pixels);
            let buffer = composite_tree(&root, &mut cache, &mut callbacks);
            self.surface_pixels = cache;
            self.pending_callbacks.append(&mut callbacks);
            if let Some((width, height, pixels)) = buffer {
                let (anchor, margin) = with_states(&root, |states| {
                    let mut guard = states.cached_state.get::<LayerSurfaceCachedState>();
                    let state = guard.current();
                    (state.anchor, state.margin)
                });
                let (x, y) =
                    layer_position(self.current_output_size, anchor, margin, width as i32, height as i32);
                let _ = self.events.send(Event::LayerBuffer {
                    id,
                    layer: layer_to_u8(layer),
                    x,
                    y,
                    width,
                    height,
                    pixels,
                });
            }
            return;
        }

        // X11 (XWayland) window?
        if let Some(entry) = self.x11_windows.get(&root) {
            let id = entry.id;
            let title = entry.surface.title();
            let app_id = entry.surface.class();
            let decorated = !entry.surface.is_override_redirect();
            let mut cache = std::mem::take(&mut self.surface_pixels);
            let buffer = composite_tree(&root, &mut cache, &mut callbacks);
            self.surface_pixels = cache;
            self.pending_callbacks.append(&mut callbacks);
            if let Some((width, height, pixels)) = buffer {
                let _ = self.events.send(Event::WindowBuffer {
                    id,
                    width,
                    height,
                    pixels,
                    title,
                    app_id,
                    decorated,
                });
            }
            return;
        }

        // Unknown root (e.g. a surface that hasn't taken an xdg/layer role yet,
        // or an orphan subsurface). Still drain this surface's frame callbacks so
        // the client isn't left blocked.
        with_states(surface, |states| {
            let mut guard = states.cached_state.get::<SurfaceAttributes>();
            callbacks.append(&mut guard.current().frame_callbacks);
        });
        self.pending_callbacks.append(&mut callbacks);
    }
}

/// Walk up the subsurface parent chain to the root `wl_surface` of a tree.
fn root_surface(surface: &WlSurface) -> WlSurface {
    use smithay::wayland::compositor::get_parent;
    let mut current = surface.clone();
    while let Some(parent) = get_parent(&current) {
        current = parent;
    }
    current
}

/// Flatten a surface tree (a root plus its subsurfaces) into a single
/// tightly-packed RGBA8 buffer sized to the root's buffer, while draining every
/// surface's frame callbacks into `callbacks`.
///
/// `cache` holds the last-known pixels of each surface so subsurfaces that don't
/// re-attach a buffer on every parent commit stay visible. Dead surfaces are
/// pruned from it.
fn composite_tree(
    root: &WlSurface,
    cache: &mut std::collections::HashMap<WlSurface, (u32, u32, Vec<u8>)>,
    callbacks: &mut Vec<WlCallback>,
) -> Option<(u32, u32, Vec<u8>)> {
    use smithay::reexports::wayland_server::Resource;
    use smithay::wayland::compositor::{
        with_surface_tree_downward, SubsurfaceCachedState, TraversalAction,
    };

    cache.retain(|s, _| s.is_alive());

    // Collect the draw order: (location, surface) for every surface that has
    // pixels to show, top-of-tree first (render order is parent before child).
    let mut draw: Vec<((i32, i32), WlSurface)> = Vec::new();

    with_surface_tree_downward(
        root,
        (0i32, 0i32),
        |_surface, states, &location| {
            // The location passed to children is this surface's location plus its
            // own subsurface offset (0 for the root toplevel).
            let off = states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location;
            TraversalAction::DoChildren((location.0 + off.x, location.1 + off.y))
        },
        |surface, states, &location| {
            // This surface's own position (parent base + its subsurface offset).
            let off = states
                .cached_state
                .get::<SubsurfaceCachedState>()
                .current()
                .location;
            let pos = (location.0 + off.x, location.1 + off.y);

            // Drain frame callbacks for this surface.
            let new_buffer = {
                let mut guard = states.cached_state.get::<SurfaceAttributes>();
                let attrs = guard.current();
                callbacks.append(&mut attrs.frame_callbacks);
                attrs.buffer.take()
            };

            match new_buffer {
                Some(BufferAssignment::NewBuffer(buffer)) => {
                    if let Ok(Some(frame)) = with_buffer_contents(&buffer, read_shm) {
                        cache.insert(surface.clone(), frame);
                    }
                    buffer.release();
                }
                Some(BufferAssignment::Removed) => {
                    cache.remove(surface);
                }
                None => {}
            }

            if cache.contains_key(surface) {
                draw.push((pos, surface.clone()));
            }
        },
        |_, _, _| true,
    );

    // Canvas size is the root surface's own buffer size.
    let (cw, ch, _) = cache.get(root)?;
    let (cw, ch) = (*cw as usize, *ch as usize);
    let mut canvas = vec![0u8; cw * ch * 4];
    for (pos, surface) in &draw {
        if let Some((w, h, pixels)) = cache.get(surface) {
            s_compositor_render::blit_over(
                &mut canvas,
                cw,
                ch,
                pos.0,
                pos.1,
                pixels,
                *w as usize,
                *h as usize,
            );
        }
    }
    Some((cw as u32, ch as u32, canvas))
}

fn layer_to_u8(layer: Layer) -> u8 {
    match layer {
        Layer::Background => 0,
        Layer::Bottom => 1,
        Layer::Top => 2,
        Layer::Overlay => 3,
    }
}

/// Position a layer surface against the output edges per its anchors+margins.
/// `(out_w, out_h)` is the current output size in logical pixels.
fn layer_position(
    (out_w, out_h): (i32, i32),
    anchor: Anchor,
    margin: Margins,
    w: i32,
    h: i32,
) -> (i32, i32) {
    let x = if anchor.contains(Anchor::RIGHT) && !anchor.contains(Anchor::LEFT) {
        out_w - w - margin.right
    } else {
        margin.left
    };
    let y = if anchor.contains(Anchor::BOTTOM) && !anchor.contains(Anchor::TOP) {
        out_h - h - margin.bottom
    } else {
        margin.top
    };
    (x, y)
}

impl SlickState {
    /// Resize the primary output to `width`x`height` (logical px). Updates the
    /// advertised `wl_output` mode so clients learn the new screen size, and
    /// re-fills layer surfaces that span the output (e.g. bars, wallpapers).
    pub fn resize_output(&mut self, width: i32, height: i32) {
        let size = (width.max(1), height.max(1));
        if self.current_output_size == size {
            return;
        }
        self.current_output_size = size;

        let mode = smithay::output::Mode {
            size: size.into(),
            refresh: 60_000,
        };
        self.output.change_current_state(Some(mode), None, None, None);
        self.output.set_preferred(mode);

        // Re-configure layer surfaces that asked to span the output (a 0 in a
        // dimension means "fill"), so bars/wallpapers track the new size.
        let surfaces: Vec<LayerSurface> = self
            .layer_surfaces
            .values()
            .map(|e| e.surface.clone())
            .collect();
        for surface in surfaces {
            let desired = with_states(surface.wl_surface(), |states| {
                states
                    .cached_state
                    .get::<LayerSurfaceCachedState>()
                    .current()
                    .size
            });
            if desired.w > 0 && desired.h > 0 {
                continue;
            }
            let w = if desired.w > 0 { desired.w } else { size.0 };
            let h = if desired.h > 0 { desired.h } else { size.1 };
            surface.with_pending_state(|state| {
                state.size = Some((w, h).into());
            });
            surface.send_configure();
        }
    }
}

/// The client's declared window geometry (`xdg_surface.set_window_geometry`),
/// in surface-logical coordinates, if any.
fn window_geometry(surface: &WlSurface) -> Option<Rectangle<i32, Logical>> {
    with_states(surface, |states| {
        states
            .cached_state
            .get::<SurfaceCachedState>()
            .current()
            .geometry
    })
}

/// Crop a tightly-packed RGBA8 `(w, h, pixels)` buffer to `geometry` (clamped to
/// the buffer), returning the cropped buffer and the top-left offset used.
///
/// Client-side-decorated apps (weston-terminal, GTK) pad their buffer with a
/// transparent shadow/resize margin and report the real window rectangle via
/// `set_window_geometry`. Cropping to it drops that margin so the shell shows
/// only the window, not a big box around it. A missing or full-buffer geometry
/// is a no-op (offset `(0, 0)`).
fn crop_to_geometry(
    buffer: (u32, u32, Vec<u8>),
    geometry: Option<Rectangle<i32, Logical>>,
) -> ((u32, u32, Vec<u8>), (i32, i32)) {
    let (w, h, pixels) = buffer;
    let Some(rect) = geometry else {
        return ((w, h, pixels), (0, 0));
    };
    let (cw, ch) = (w as i32, h as i32);
    let x0 = rect.loc.x.clamp(0, cw);
    let y0 = rect.loc.y.clamp(0, ch);
    let x1 = (rect.loc.x + rect.size.w).clamp(0, cw);
    let y1 = (rect.loc.y + rect.size.h).clamp(0, ch);
    let nw = x1 - x0;
    let nh = y1 - y0;
    // Degenerate or already full-size: nothing to crop.
    if nw <= 0 || nh <= 0 || (x0 == 0 && y0 == 0 && nw == cw && nh == ch) {
        return ((w, h, pixels), (0, 0));
    }
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    let row_bytes = (nw * 4) as usize;
    for row in 0..nh {
        let src = (((y0 + row) * cw + x0) * 4) as usize;
        let dst = (row * nw * 4) as usize;
        out[dst..dst + row_bytes].copy_from_slice(&pixels[src..src + row_bytes]);
    }
    ((nw as u32, nh as u32, out), (x0, y0))
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

/// Read the toplevel app-id (used to resolve a window icon).
fn read_app_id(surface: &WlSurface) -> String {
    with_states(surface, |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().unwrap().app_id.clone())
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
                // Assume the client draws its own decorations unless it explicitly
                // negotiates server-side ones via xdg-decoration (see
                // `new_decoration`). This avoids double title bars on clients that
                // always draw CSD but don't speak the decoration protocol (GTK,
                // weston toytoolkit).
                decorated: false,
                geometry_offset: (0, 0),
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

    fn move_request(
        &mut self,
        surface: ToplevelSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: smithay::utils::Serial,
    ) {
        // The shell (UI thread) owns window positions, so it drives the move.
        // Forward the request; the UI follows the pointer until release. This is
        // how client-side-decorated toplevels (weston-terminal, GTK CSD) get
        // moved, since they draw their own title bar instead of using ours.
        if let Some(entry) = self.windows.get(surface.wl_surface()) {
            let _ = self
                .events
                .send(Event::WindowMoveRequested { id: entry.id });
        }
    }

    fn maximize_request(&mut self, surface: ToplevelSurface) {
        // The shell (UI thread) owns geometry, so it picks the work area that
        // excludes the panel; just flag the state and let it drive the resize.
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Maximized);
        });
        surface.send_configure();
        if let Some(entry) = self.windows.get(surface.wl_surface()) {
            let _ = self.events.send(Event::WindowMaximizeRequested {
                id: entry.id,
                maximized: true,
            });
        }
    }

    fn unmaximize_request(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.states.unset(xdg_toplevel::State::Maximized);
        });
        surface.send_configure();
        if let Some(entry) = self.windows.get(surface.wl_surface()) {
            let _ = self.events.send(Event::WindowMaximizeRequested {
                id: entry.id,
                maximized: false,
            });
        }
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

    /// A Wayland client set (or cleared) a selection: mirror it to X so X11 apps
    /// can paste it.
    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        if let Some(xwm) = self.xwm.as_mut() {
            let mimes = source.map(|s| s.mime_types());
            if let Err(err) = xwm.new_selection(ty, mimes) {
                log::warn!("xwayland: failed to advertise selection to X: {err}");
            }
        }
    }

    /// A Wayland client is reading a selection that X owns: ask X to write it.
    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        let handle = self.loop_handle.clone();
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.send_selection(ty, mime_type, fd, handle) {
                log::warn!("xwayland: failed to send X selection to Wayland: {err}");
            }
        }
    }
}

impl DataDeviceHandler for SlickState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for SlickState {}
impl ServerDndGrabHandler for SlickState {}

impl smithay::wayland::selection::primary_selection::PrimarySelectionHandler for SlickState {
    fn primary_selection_state(
        &self,
    ) -> &smithay::wayland::selection::primary_selection::PrimarySelectionState {
        &self.primary_selection_state
    }
}

impl OutputHandler for SlickState {}

impl WlrLayerShellHandler for SlickState {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: LayerSurface,
        _output: Option<WlOutput>,
        layer: Layer,
        _namespace: String,
    ) {
        // Size the surface from its requested size, filling 0 dimensions.
        let desired = with_states(surface.wl_surface(), |states| {
            states
                .cached_state
                .get::<LayerSurfaceCachedState>()
                .current()
                .size
        });
        let (out_w, out_h) = self.current_output_size;
        let w = if desired.w > 0 { desired.w } else { out_w };
        let h = if desired.h > 0 { desired.h } else { out_h };
        surface.with_pending_state(|state| {
            state.size = Some((w, h).into());
        });
        surface.send_configure();

        let id = self.allocate_window_id();
        let wl = surface.wl_surface().clone();
        self.layer_surfaces
            .insert(wl, LayerEntry { id, surface, layer });
        log::info!("new layer surface {id:?} ({layer:?}) {w}x{h}");
    }

    fn layer_destroyed(&mut self, surface: LayerSurface) {
        if let Some(entry) = self.layer_surfaces.remove(surface.wl_surface()) {
            let _ = self.events.send(Event::LayerRemoved(entry.id));
        }
    }
}

delegate_compositor!(SlickState);
delegate_shm!(SlickState);

impl smithay::wayland::dmabuf::DmabufHandler for SlickState {
    fn dmabuf_state(&mut self) -> &mut smithay::wayland::dmabuf::DmabufState {
        &mut self.dmabuf_state
    }
    fn dmabuf_imported(
        &mut self,
        _global: &smithay::wayland::dmabuf::DmabufGlobal,
        _dmabuf: smithay::backend::allocator::dmabuf::Dmabuf,
        notifier: smithay::wayland::dmabuf::ImportNotifier,
    ) {
        // The real EGLImage import happens on the render thread (which owns the
        // GL context) when the buffer is committed; accept it here optimistically.
        let _ = notifier.successful::<SlickState>();
    }
}
smithay::delegate_dmabuf!(SlickState);

/// If the root surface's pending buffer is a dmabuf, take it, drain its frame
/// callbacks, dup its plane fds into a [`Event::WindowDmabuf`], and release it.
/// Returns `None` (leaving the buffer in place for shm compositing) otherwise.
fn take_dmabuf(
    root: &WlSurface,
    id: crate::WindowId,
    callbacks: &mut Vec<smithay::reexports::wayland_server::protocol::wl_callback::WlCallback>,
) -> Option<Event> {
    use smithay::backend::allocator::Buffer;
    use smithay::wayland::dmabuf::get_dmabuf;
    with_states(root, |states| {
        let mut guard = states.cached_state.get::<SurfaceAttributes>();
        let attrs = guard.current();
        let is_dmabuf = matches!(
            &attrs.buffer,
            Some(BufferAssignment::NewBuffer(b)) if get_dmabuf(b).is_ok()
        );
        if !is_dmabuf {
            return None;
        }
        callbacks.append(&mut attrs.frame_callbacks);
        let buffer = match attrs.buffer.take() {
            Some(BufferAssignment::NewBuffer(b)) => b,
            _ => return None,
        };
        let dmabuf = get_dmabuf(&buffer).ok()?;
        let planes: Vec<crate::DmabufPlane> = dmabuf
            .handles()
            .zip(dmabuf.offsets())
            .zip(dmabuf.strides())
            .filter_map(|((fd, offset), stride)| {
                fd.try_clone_to_owned()
                    .ok()
                    .map(|fd| crate::DmabufPlane { fd, offset, stride })
            })
            .collect();
        let format = dmabuf.format();
        let event = Event::WindowDmabuf {
            id,
            width: dmabuf.width(),
            height: dmabuf.height(),
            fourcc: format.code as u32,
            modifier: u64::from(format.modifier),
            planes,
        };
        buffer.release();
        Some(event)
    })
}
delegate_xdg_shell!(SlickState);
smithay::delegate_xdg_decoration!(SlickState);
smithay::delegate_layer_shell!(SlickState);
delegate_seat!(SlickState);
delegate_output!(SlickState);
delegate_data_device!(SlickState);
smithay::delegate_primary_selection!(SlickState);

#[cfg(test)]
mod crop_tests {
    use super::crop_to_geometry;
    use smithay::utils::Rectangle;

    /// Build a `w`x`h` RGBA8 buffer whose every pixel's R channel is its column
    /// and G channel is its row, so a crop is easy to verify positionally.
    fn ramp(w: i32, h: i32) -> (u32, u32, Vec<u8>) {
        let mut px = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&[x as u8, y as u8, 0, 255]);
            }
        }
        (w as u32, h as u32, px)
    }

    fn pixel(buf: &(u32, u32, Vec<u8>), x: i32, y: i32) -> [u8; 4] {
        let i = ((y * buf.0 as i32 + x) * 4) as usize;
        buf.2[i..i + 4].try_into().unwrap()
    }

    #[test]
    fn no_geometry_is_identity() {
        let (out, off) = crop_to_geometry(ramp(4, 4), None);
        assert_eq!((out.0, out.1), (4, 4));
        assert_eq!(off, (0, 0));
    }

    #[test]
    fn full_geometry_is_identity() {
        let geo = Some(Rectangle::new((0, 0).into(), (4, 4).into()));
        let (out, off) = crop_to_geometry(ramp(4, 4), geo);
        assert_eq!((out.0, out.1), (4, 4));
        assert_eq!(off, (0, 0));
    }

    #[test]
    fn crops_away_shadow_margin() {
        // A 10x10 buffer with the real window a 6x6 rect at (2,2): the
        // surrounding 2px is the CSD shadow that must be dropped.
        let geo = Some(Rectangle::new((2, 2).into(), (6, 6).into()));
        let (out, off) = crop_to_geometry(ramp(10, 10), geo);
        assert_eq!((out.0, out.1), (6, 6));
        assert_eq!(off, (2, 2));
        // Top-left of the crop is the original (2,2) pixel.
        assert_eq!(pixel(&out, 0, 0), [2, 2, 0, 255]);
        // Bottom-right of the crop is the original (7,7) pixel.
        assert_eq!(pixel(&out, 5, 5), [7, 7, 0, 255]);
    }

    #[test]
    fn geometry_is_clamped_to_buffer() {
        // Geometry larger than the buffer (or partly outside) is clamped, never
        // panics or reads OOB.
        let geo = Some(Rectangle::new((-3, -3).into(), (100, 100).into()));
        let (out, off) = crop_to_geometry(ramp(8, 8), geo);
        assert_eq!((out.0, out.1), (8, 8));
        assert_eq!(off, (0, 0));
    }

    #[test]
    fn degenerate_geometry_is_identity() {
        let geo = Some(Rectangle::new((4, 4).into(), (0, 0).into()));
        let (out, off) = crop_to_geometry(ramp(8, 8), geo);
        assert_eq!((out.0, out.1), (8, 8));
        assert_eq!(off, (0, 0));
    }
}
