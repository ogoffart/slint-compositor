//! The compositor's global state and per-client data.

use std::sync::Arc;

use smithay::input::{Seat, SeatState};
use smithay::reexports::calloop::LoopSignal;
use smithay::reexports::wayland_server::backend::{ClientData, ClientId, DisconnectReason};
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::compositor::{CompositorClientState, CompositorState};
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;

use slick_shell::WindowId;

use crate::workspace::Workspaces;
use crate::Event;

/// The single state value that calloop hands to every protocol handler.
pub struct SlickState {
    pub display_handle: DisplayHandle,
    pub loop_signal: LoopSignal,

    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<SlickState>,
    pub data_device_state: DataDeviceState,
    pub seat: Seat<SlickState>,

    pub workspaces: Workspaces,
    pub next_window_id: u64,

    /// Outbound channel used to notify the UI thread of shell events.
    pub events: std::sync::mpsc::Sender<Event>,
}

impl SlickState {
    /// Allocate the next stable window id.
    pub fn allocate_window_id(&mut self) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id += 1;
        id
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
