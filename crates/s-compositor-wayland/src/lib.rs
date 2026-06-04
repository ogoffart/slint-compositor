//! The s-compositor Wayland protocol engine, built on Smithay.
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
use smithay::reexports::wayland_server::{BindError, ListeningSocket};
use smithay::wayland::compositor::CompositorState;
use smithay::wayland::output::OutputManagerState;
use smithay::wayland::selection::data_device::DataDeviceState;
use smithay::wayland::shell::xdg::XdgShellState;
use smithay::wayland::shm::ShmState;

mod handlers;
mod input;
mod state;
mod workspace;
mod xwayland;

pub use s_compositor_shell::WindowId;
pub use state::SlickState;
pub use workspace::Workspaces;

/// Number of virtual desktops created at startup.
pub const WORKSPACE_COUNT: usize = 4;

/// Events emitted by the compositor thread for the UI thread to consume.
pub enum Event {
    /// The compositor is up and accepting clients on the given `WAYLAND_DISPLAY`.
    Ready {
        socket_name: String,
        /// The directory the socket lives in; launched apps need this as their
        /// `XDG_RUNTIME_DIR` to find it.
        runtime_dir: String,
        /// The output (monitor) layout the compositor advertises.
        outputs: Vec<OutputInfo>,
    },
    WindowAdded(WindowId),
    WindowRemoved(WindowId),
    WindowTitleChanged(WindowId, String),
    /// XWayland is up; X11 apps should be launched with `DISPLAY=:{display}`.
    XwaylandReady {
        display: u32,
    },
    /// The client asked to (un)maximize itself; the shell decides the geometry.
    WindowMaximizeRequested {
        id: WindowId,
        maximized: bool,
    },
    /// The window's decoration mode changed (false = client draws its own).
    WindowDecorated {
        id: WindowId,
        decorated: bool,
    },
    /// A window committed a new frame: tightly-packed RGBA8 of `width`x`height`.
    WindowBuffer {
        id: WindowId,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
        title: String,
        /// The toplevel app-id, used to resolve a window icon.
        app_id: String,
        /// Whether s-compositor should draw server-side decorations for this window.
        decorated: bool,
    },
    /// A window committed a GPU (dmabuf) buffer instead of shm. The planes carry
    /// owned fds for the UI thread to import as an EGLImage-backed GL texture.
    /// Only emitted when dmabuf is enabled (`S_COMPOSITOR_DMABUF`).
    WindowDmabuf {
        id: WindowId,
        width: u32,
        height: u32,
        /// DRM FourCC format code.
        fourcc: u32,
        /// DRM format modifier.
        modifier: u64,
        planes: Vec<DmabufPlane>,
    },
    /// A popup committed a frame, to be drawn at offset `(ox, oy)` from `parent`.
    PopupBuffer {
        id: WindowId,
        parent: Option<WindowId>,
        ox: i32,
        oy: i32,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    PopupRemoved(WindowId),
    /// A layer-shell surface committed a frame, to be drawn at `(x, y)`.
    /// `layer` is 0=background, 1=bottom, 2=top, 3=overlay.
    LayerBuffer {
        id: WindowId,
        layer: u8,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        pixels: Vec<u8>,
    },
    LayerRemoved(WindowId),
}

/// Output size in logical pixels, used to anchor layer-shell surfaces.
pub const OUTPUT_W: i32 = 1280;
pub const OUTPUT_H: i32 = 800;

/// One plane of a dmabuf: an owned file descriptor plus its offset and stride.
#[derive(Debug)]
pub struct DmabufPlane {
    pub fd: std::os::fd::OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// One output (monitor) in the layout: a name and a position + size in the
/// global compositor coordinate space.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputInfo {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

/// The output layout. Read from `S_COMPOSITOR_OUTPUTS` (e.g.
/// `1280x800+0+0;1280x800+1280+0` for two side-by-side monitors); defaults to a
/// single `OUTPUT_W`x`OUTPUT_H` output at the origin.
pub fn output_layout() -> Vec<OutputInfo> {
    if let Ok(spec) = std::env::var("S_COMPOSITOR_OUTPUTS") {
        let outs: Vec<OutputInfo> = spec
            .split([';', ','])
            .enumerate()
            .filter_map(|(i, part)| parse_output(part.trim(), i))
            .collect();
        if !outs.is_empty() {
            return outs;
        }
    }
    vec![OutputInfo {
        name: "s-compositor-0".into(),
        x: 0,
        y: 0,
        w: OUTPUT_W,
        h: OUTPUT_H,
    }]
}

/// Parse one `WxH+X+Y` (or bare `WxH`) output specification.
fn parse_output(spec: &str, idx: usize) -> Option<OutputInfo> {
    let (size, pos) = match spec.split_once('+') {
        Some((s, p)) => (s, Some(p)),
        None => (spec, None),
    };
    let (w, h) = size.split_once('x')?;
    let (w, h) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    let (x, y) = match pos {
        Some(p) => {
            let (x, y) = p.split_once('+')?;
            (x.trim().parse().ok()?, y.trim().parse().ok()?)
        }
        None => (0, 0),
    };
    Some(OutputInfo {
        name: format!("s-compositor-{idx}"),
        x,
        y,
        w,
        h,
    })
}

/// Commands sent from the UI thread to the compositor thread.
#[derive(Debug, Clone)]
pub enum Command {
    /// Ask a window to close (sends `xdg_toplevel.close`).
    CloseWindow(WindowId),
    /// Give keyboard focus to a window (e.g. taskbar click).
    FocusWindow(WindowId),
    /// Pointer moved to surface-local `(x, y)` over the given window.
    PointerMotion { id: WindowId, x: f64, y: f64 },
    /// Pointer button (evdev code) pressed/released over the given window.
    PointerButton {
        id: WindowId,
        button: u32,
        pressed: bool,
    },
    /// Pointer left all client windows.
    PointerLeave,
    /// Vertical/horizontal scroll over the given window.
    PointerAxis { id: WindowId, dx: f64, dy: f64 },
    /// A key (evdev keycode) pressed/released for the focused window.
    Key { keycode: u32, pressed: bool },
    /// Resize a window to the given size (sends an xdg configure).
    ResizeWindow {
        id: WindowId,
        width: i32,
        height: i32,
    },
    /// Dismiss all open popups (e.g. a click landed outside them).
    DismissPopups,
}

/// Re-exported so the UI crate can hold the sending half.
pub use smithay::reexports::calloop::channel::Sender as CommandSender;

/// Create the command channel. The [`CommandSender`] stays on the UI thread;
/// the [`Channel`](smithay::reexports::calloop::channel::Channel) is handed to
/// [`run`].
pub fn command_channel() -> (
    CommandSender<Command>,
    smithay::reexports::calloop::channel::Channel<Command>,
) {
    smithay::reexports::calloop::channel::channel()
}

impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Event::Ready {
                socket_name,
                runtime_dir,
                outputs,
            } => f
                .debug_struct("Ready")
                .field("socket_name", socket_name)
                .field("runtime_dir", runtime_dir)
                .field("outputs", outputs)
                .finish(),
            Event::WindowDmabuf {
                id, width, height, ..
            } => f
                .debug_struct("WindowDmabuf")
                .field("id", id)
                .field("width", width)
                .field("height", height)
                .finish_non_exhaustive(),
            Event::WindowAdded(id) => f.debug_tuple("WindowAdded").field(id).finish(),
            Event::WindowRemoved(id) => f.debug_tuple("WindowRemoved").field(id).finish(),
            Event::XwaylandReady { display } => f
                .debug_struct("XwaylandReady")
                .field("display", display)
                .finish(),
            Event::WindowMaximizeRequested { id, maximized } => f
                .debug_struct("WindowMaximizeRequested")
                .field("id", id)
                .field("maximized", maximized)
                .finish(),
            Event::WindowTitleChanged(id, t) => f
                .debug_tuple("WindowTitleChanged")
                .field(id)
                .field(t)
                .finish(),
            Event::WindowDecorated { id, decorated } => f
                .debug_struct("WindowDecorated")
                .field("id", id)
                .field("decorated", decorated)
                .finish(),
            // Don't dump the pixel buffer.
            Event::WindowBuffer {
                id,
                width,
                height,
                title,
                app_id,
                decorated,
                ..
            } => f
                .debug_struct("WindowBuffer")
                .field("id", id)
                .field("width", width)
                .field("height", height)
                .field("title", title)
                .field("app_id", app_id)
                .field("decorated", decorated)
                .finish(),
            Event::PopupBuffer {
                id,
                parent,
                width,
                height,
                ..
            } => f
                .debug_struct("PopupBuffer")
                .field("id", id)
                .field("parent", parent)
                .field("width", width)
                .field("height", height)
                .finish(),
            Event::PopupRemoved(id) => f.debug_tuple("PopupRemoved").field(id).finish(),
            Event::LayerBuffer {
                id,
                layer,
                x,
                y,
                width,
                height,
                ..
            } => f
                .debug_struct("LayerBuffer")
                .field("id", id)
                .field("layer", layer)
                .field("x", x)
                .field("y", y)
                .field("width", width)
                .field("height", height)
                .finish(),
            Event::LayerRemoved(id) => f.debug_tuple("LayerRemoved").field(id).finish(),
        }
    }
}

/// Run the Wayland compositor event loop. Intended to be called on a dedicated
/// thread; blocks until the loop is torn down.
pub fn run(
    events: Sender<Event>,
    commands: smithay::reexports::calloop::channel::Channel<Command>,
) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<SlickState> =
        EventLoop::try_new().context("failed to create calloop event loop")?;
    let display: Display<SlickState> = Display::new().context("failed to create wl_display")?;
    let dh = display.handle();

    let compositor_state = CompositorState::new::<SlickState>(&dh);
    let xdg_shell_state = XdgShellState::new::<SlickState>(&dh);
    let xdg_decoration_state =
        smithay::wayland::shell::xdg::decoration::XdgDecorationState::new::<SlickState>(&dh);
    let layer_shell_state =
        smithay::wayland::shell::wlr_layer::WlrLayerShellState::new::<SlickState>(&dh);
    let shm_state = ShmState::new::<SlickState>(&dh, Vec::new());
    let output_manager_state = OutputManagerState::new_with_xdg_output::<SlickState>(&dh);
    let mut seat_state = SeatState::<SlickState>::new();
    let data_device_state = DataDeviceState::new::<SlickState>(&dh);
    let primary_selection_state =
        smithay::wayland::selection::primary_selection::PrimarySelectionState::new::<SlickState>(
            &dh,
        );
    let xwayland_shell_state =
        smithay::wayland::xwayland_shell::XWaylandShellState::new::<SlickState>(&dh);

    let mut seat = seat_state.new_wl_seat(&dh, "seat0");
    seat.add_keyboard(Default::default(), 200, 25)
        .context("failed to add keyboard to seat")?;
    seat.add_pointer();

    // Advertise the configured output layout (at least one; many clients refuse
    // to map without an output). Each entry becomes a `wl_output` global the
    // compositor positions at its place in the global coordinate space.
    let layout = output_layout();
    let mut outputs = Vec::new();
    for cfg in &layout {
        let output = smithay::output::Output::new(
            cfg.name.clone(),
            smithay::output::PhysicalProperties {
                size: (0, 0).into(),
                subpixel: smithay::output::Subpixel::Unknown,
                make: "s-compositor".into(),
                model: "virtual".into(),
            },
        );
        output.create_global::<SlickState>(&dh);
        let mode = smithay::output::Mode {
            size: (cfg.w, cfg.h).into(),
            refresh: 60_000,
        };
        output.change_current_state(Some(mode), None, None, Some((cfg.x, cfg.y).into()));
        output.set_preferred(mode);
        outputs.push(output);
    }
    // The first output is the primary, used where a single output is expected.
    let output = outputs[0].clone();

    // dmabuf (GPU buffers): off unless explicitly enabled. Experimental — the
    // import path needs a real GPU to validate.
    let dmabuf_enabled = std::env::var_os("S_COMPOSITOR_DMABUF").is_some();
    let dmabuf_state = smithay::wayland::dmabuf::DmabufState::new();

    let mut state = SlickState {
        display_handle: dh.clone(),
        loop_signal: event_loop.get_signal(),
        loop_handle: event_loop.handle(),
        compositor_state,
        xdg_shell_state,
        xdg_decoration_state,
        layer_shell_state,
        shm_state,
        output_manager_state,
        seat_state,
        data_device_state,
        primary_selection_state,
        seat,
        output,
        outputs,
        dmabuf_state,
        dmabuf_global: None,
        dmabuf_enabled,
        workspaces: Workspaces::new(WORKSPACE_COUNT),
        next_window_id: 0,
        windows: std::collections::HashMap::new(),
        popups: std::collections::HashMap::new(),
        layer_surfaces: std::collections::HashMap::new(),
        xwm: None,
        xwayland_shell_state,
        x11_windows: std::collections::HashMap::new(),
        surface_pixels: std::collections::HashMap::new(),
        start_time: std::time::Instant::now(),
        pending_callbacks: Vec::new(),
        events: events.clone(),
    };

    if state.dmabuf_enabled {
        use smithay::backend::allocator::{Format, Fourcc, Modifier};
        // Advertise common 32-bit formats with implicit/linear modifiers. The
        // real set a GPU can import is unknown on this (UI-less) thread, so the
        // import is attempted on the render thread and may fail there.
        let formats: Vec<Format> = [Fourcc::Argb8888, Fourcc::Xrgb8888]
            .into_iter()
            .flat_map(|code| {
                [Modifier::Invalid, Modifier::Linear]
                    .into_iter()
                    .map(move |modifier| Format { code, modifier })
            })
            .collect();
        let global = state
            .dmabuf_state
            .create_global::<SlickState>(&dh, formats);
        state.dmabuf_global = Some(global);
        log::info!("dmabuf: advertising zwp_linux_dmabuf_v1 (experimental)");
    }

    // Create the listening socket, falling back to a private directory if
    // XDG_RUNTIME_DIR is not writable (e.g. sandboxes, unusual sessions).
    let (socket, socket_name, runtime_dir) =
        bind_socket().context("failed to create wayland socket")?;
    let handle = event_loop.handle();
    handle
        .insert_source(
            Generic::new(socket, Interest::READ, Mode::Level),
            move |_, socket, state: &mut SlickState| {
                while let Some(stream) = socket.accept()? {
                    if let Err(err) = state
                        .display_handle
                        .insert_client(stream, state::ClientState::arc())
                    {
                        log::warn!("failed to accept client: {err}");
                    }
                }
                Ok::<_, std::io::Error>(PostAction::Continue)
            },
        )
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

    // Apply commands coming from the UI thread.
    handle
        .insert_source(commands, |event, _, state: &mut SlickState| {
            if let smithay::reexports::calloop::channel::Event::Msg(command) = event {
                state.handle_command(command);
            }
        })
        .map_err(|e| anyhow::anyhow!("failed to insert command source: {e}"))?;

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

    // Start XWayland so X11 apps can run (best-effort; needs the Xwayland binary).
    xwayland::setup(&handle, &dh);

    log::info!("s-compositor listening on {runtime_dir}/{socket_name}");
    let _ = events.send(Event::Ready {
        socket_name: socket_name.clone(),
        runtime_dir: runtime_dir.clone(),
        outputs: layout.clone(),
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

/// Bind the compositor's listening socket. Tries `XDG_RUNTIME_DIR` first, then
/// falls back to a private `s-compositor-<uid>` directory under the temp dir if that is
/// not writable. Returns the socket plus its name and directory.
fn bind_socket() -> anyhow::Result<(ListeningSocket, String, String)> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::PathBuf;

    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(&dir);
        if path.is_absolute() {
            candidates.push(path);
        }
    }
    let uid = std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(0);
    let fallback = std::env::temp_dir().join(format!("s-compositor-{uid}"));
    candidates.push(fallback.clone());

    let mut last_err: Option<String> = None;
    for dir in candidates {
        if dir == fallback {
            if let Err(err) = std::fs::create_dir_all(&dir) {
                last_err = Some(format!("create {}: {err}", dir.display()));
                continue;
            }
            let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
        }
        for n in 1..33u32 {
            let name = format!("wayland-{n}");
            match ListeningSocket::bind_absolute(dir.join(&name)) {
                Ok(socket) => {
                    return Ok((socket, name, dir.to_string_lossy().into_owned()));
                }
                Err(BindError::AlreadyInUse) => continue,
                Err(err) => {
                    last_err = Some(format!("{}: {err}", dir.display()));
                    break;
                }
            }
        }
    }
    anyhow::bail!(
        "no writable runtime directory for the wayland socket ({})",
        last_err.unwrap_or_else(|| "unknown".into())
    )
}
