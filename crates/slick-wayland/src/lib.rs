//! The slick Wayland protocol engine, built on Smithay.
//!
//! This crate is rendering-agnostic: it owns the Wayland display, socket, client
//! state and protocol handlers, and runs them on a `calloop` event loop. It does
//! **not** present anything — Slint owns output/input/GL in the binary crate.
//! State updates flow out to the UI thread over an [`Event`] channel.

use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::Context as _;
use smithay::input::SeatState;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
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
#[derive(Clone)]
pub enum Event {
    /// The compositor is up and accepting clients on the given `WAYLAND_DISPLAY`.
    Ready {
        socket_name: String,
    },
    WindowAdded(WindowId),
    WindowRemoved(WindowId),
    WindowTitleChanged(WindowId, String),
    /// A window committed a new frame: tightly-packed RGBA8 of `width`x`height`.
    WindowBuffer {
        id: WindowId,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
}

impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Event::Ready { socket_name } => f
                .debug_struct("Ready")
                .field("socket_name", socket_name)
                .finish(),
            Event::WindowAdded(id) => f.debug_tuple("WindowAdded").field(id).finish(),
            Event::WindowRemoved(id) => f.debug_tuple("WindowRemoved").field(id).finish(),
            Event::WindowTitleChanged(id, t) => f
                .debug_tuple("WindowTitleChanged")
                .field(id)
                .field(t)
                .finish(),
            // Don't dump the pixel buffer.
            Event::WindowBuffer {
                id, width, height, ..
            } => f
                .debug_struct("WindowBuffer")
                .field("id", id)
                .field("width", width)
                .field("height", height)
                .finish(),
        }
    }
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
        windows: std::collections::HashMap::new(),
        start_time: std::time::Instant::now(),
        pending_callbacks: Vec::new(),
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
                        .map_err(std::io::Error::other)?;
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert display source: {e}"))?;

    // Fire queued frame callbacks at ~60Hz so clients pace their rendering to
    // roughly display rate instead of busy-looping.
    handle
        .insert_source(
            Timer::from_duration(Duration::from_millis(16)),
            |_, _, state: &mut SlickState| {
                let now = state.millis_since_start();
                for callback in state.pending_callbacks.drain(..) {
                    callback.done(now);
                }
                if let Err(err) = state.display_handle.flush_clients() {
                    log::warn!("failed to flush clients: {err}");
                }
                TimeoutAction::ToDuration(Duration::from_millis(16))
            },
        )
        .map_err(|e| anyhow::anyhow!("failed to insert frame timer: {e}"))?;

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
