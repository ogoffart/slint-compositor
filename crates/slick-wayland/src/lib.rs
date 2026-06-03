//! The slick Wayland protocol engine, built on Smithay.
//!
//! This crate is rendering-agnostic: it owns the Wayland display, socket, client
//! state and protocol handlers, and runs them on a `calloop` event loop. It does
//! **not** present anything — Slint owns output/input/GL in the binary crate.
//! State updates flow out to the UI thread over an [`Event`] channel.

use std::sync::mpsc::Sender;

use anyhow::Context as _;
use smithay::input::SeatState;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{EventLoop, Interest, Mode, PostAction};
use smithay::reexports::wayland_server::Display;
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;
use smithay::wayland::socket::ListeningSocketSource;

mod handlers;
mod state;
mod workspace;

pub use slick_shell::WindowId;
pub use state::SlickState;
pub use workspace::Workspaces;

/// Number of virtual desktops created at startup.
pub const WORKSPACE_COUNT: usize = 4;

/// Events emitted by the compositor thread for the UI thread to consume.
#[derive(Debug, Clone)]
pub enum Event {
    /// The compositor is up and accepting clients on the given `WAYLAND_DISPLAY`.
    Ready {
        socket_name: String,
    },
    WindowAdded(WindowId),
    WindowRemoved(WindowId),
    WindowTitleChanged(WindowId, String),
}

/// Run the Wayland compositor event loop. Intended to be called on a dedicated
/// thread; blocks until the loop is torn down.
pub fn run(events: Sender<Event>) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<SlickState> =
        EventLoop::try_new().context("failed to create calloop event loop")?;
    let display: Display<SlickState> = Display::new().context("failed to create wl_display")?;
    let dh = display.handle();

    let compositor_state = CompositorState::new::<SlickState>(&dh);
    let xdg_shell_state = XdgShellState::new::<SlickState>(&dh);
    let shm_state = ShmState::new::<SlickState>(&dh, Vec::new());
    let output_manager_state = OutputManagerState::new_with_xdg_output::<SlickState>(&dh);
    let mut seat_state = SeatState::<SlickState>::new();
    let data_device_state = DataDeviceState::new::<SlickState>(&dh);

    let mut seat = seat_state.new_wl_seat(&dh, "seat0");
    seat.add_keyboard(Default::default(), 200, 25)
        .context("failed to add keyboard to seat")?;
    seat.add_pointer();

    let mut state = SlickState {
        display_handle: dh.clone(),
        loop_signal: event_loop.get_signal(),
        compositor_state,
        xdg_shell_state,
        shm_state,
        output_manager_state,
        seat_state,
        data_device_state,
        seat,
        workspaces: Workspaces::new(WORKSPACE_COUNT),
        next_window_id: 0,
        events: events.clone(),
    };

    // Listen for new clients on an auto-selected wayland socket.
    let source = ListeningSocketSource::new_auto().context("failed to create wayland socket")?;
    let socket_name = source.socket_name().to_string_lossy().into_owned();
    let handle = event_loop.handle();
    handle
        .insert_source(source, move |client_stream, _, state: &mut SlickState| {
            if let Err(err) = state
                .display_handle
                .insert_client(client_stream, state::ClientState::arc())
            {
                log::warn!("failed to accept client: {err}");
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert socket source: {e}"))?;

    // Dispatch client requests when the display fd becomes readable.
    handle
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state: &mut SlickState| {
                // SAFETY: the display is not dropped while the loop runs.
                unsafe {
                    display
                        .get_mut()
                        .dispatch_clients(state)
                        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert display source: {e}"))?;

    log::info!("slick compositor listening on {socket_name}");
    let _ = events.send(Event::Ready {
        socket_name: socket_name.clone(),
    });

    event_loop
        .run(None, &mut state, |state| {
            if let Err(err) = state.display_handle.flush_clients() {
                log::warn!("failed to flush clients: {err}");
            }
        })
        .context("event loop terminated unexpectedly")?;

    Ok(())
}
