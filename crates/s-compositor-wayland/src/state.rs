//! The compositor's global state and per-client data.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use smithay::input::{Seat, SeatState};
use smithay::output::Output;
use smithay::reexports::calloop::LoopSignal;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::protocol::wl_callback::WlCallback;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::decoration::XdgDecorationState;
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgShellState};
use smithay::wayland::shm::ShmState;

use s_compositor_shell::WindowId;

use crate::workspace::Workspaces;
use crate::Event;

/// The single state value that calloop hands to every protocol handler.
pub struct SlickState {
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub xdg_decoration_state: XdgDecorationState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<SlickState>,
    pub data_device_state: DataDeviceState,
    pub seat: Seat<SlickState>,
    pub output: Output,

    pub workspaces: Workspaces,
    pub next_window_id: u64,

    /// Mapped toplevel windows, keyed by their root `wl_surface`.
    pub windows: HashMap<WlSurface, WindowEntry>,

    /// Start of the compositor, used to timestamp `wl_callback.done`.
    pub start_time: Instant,

    /// Frame callbacks awaiting completion. Drained on a ~60Hz timer rather
    /// than fired on commit, so clients don't busy-loop rendering.
    pub pending_callbacks: Vec<WlCallback>,

    /// Outbound channel used to notify the UI thread of shell events.
    pub events: std::sync::mpsc::Sender<Event>,
}

/// A tracked toplevel window.
pub struct WindowEntry {
    pub id: WindowId,
    pub toplevel: ToplevelSurface,
    /// Whether s-compositor draws server-side decorations for this window. False when
    /// the client requested client-side decorations via xdg-decoration.
    pub decorated: bool,
}

impl SlickState {
    /// Allocate the next stable window id.
    pub fn allocate_window_id(&mut self) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id += 1;
        id
    }

    /// Milliseconds since startup, for frame-callback timestamps.
    pub fn millis_since_start(&self) -> u32 {
        self.start_time.elapsed().as_millis() as u32
    }

    /// The `wl_surface` of the window with the given id, if mapped.
    pub fn surface_for(&self, id: WindowId) -> Option<WlSurface> {
        self.windows
            .values()
            .find(|e| e.id == id)
            .map(|e| e.toplevel.wl_surface().clone())
    }
}

/// Per-client data stored by the Wayland backend.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

impl ClientState {
    pub fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }
}
