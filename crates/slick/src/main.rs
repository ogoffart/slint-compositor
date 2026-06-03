//! slick — a Rust/Slint Wayland desktop shell.
//!
//! Slint owns the main thread (UI + output + input + GL); the Smithay-based
//! Wayland protocol engine runs on its own thread and reports state changes back
//! over a channel which we drain on the UI thread.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::channel;
use std::time::Duration;

use chrono::Local;
use slint::{ComponentHandle, Model, ModelRc, VecModel};

mod gl_bridge;
use gl_bridge::{Frame, GlBridge};

slint::include_modules!();

/// Shared UI-thread state touched by both the event pump and the rendering
/// notifier.
#[derive(Default)]
struct Windows {
    /// window id -> row index in the model.
    rows: HashMap<u64, usize>,
    /// Latest frame per window awaiting GPU upload (drained in the notifier).
    pending: HashMap<u64, Frame>,
    /// Windows removed since the last frame, whose textures must be freed.
    closed: Vec<u64>,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Spawn the Wayland compositor on its own thread.
    let (tx, rx) = channel::<slick_wayland::Event>();
    let (cmd_tx, cmd_rx) = slick_wayland::command_channel();
    std::thread::Builder::new()
        .name("slick-wayland".into())
        .spawn(move || {
            if let Err(err) = slick_wayland::run(tx, cmd_rx) {
                log::error!("wayland thread exited: {err:?}");
            }
        })?;

    let desktop = Desktop::new()?;

    let model = Rc::new(VecModel::<WindowTile>::default());
    desktop.set_windows(ModelRc::from(model.clone()));
    let windows = Rc::new(RefCell::new(Windows::default()));
    let bridge = Rc::new(RefCell::new(GlBridge::default()));
    let socket_name: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    // Upload client frames into shared GL textures during rendering, where the
    // GL context is current, and hand them to Slint as borrowed textures.
    desktop
        .window()
        .set_rendering_notifier({
            let bridge = bridge.clone();
            let windows = windows.clone();
            let model = model.clone();
            move |state, graphics_api| match state {
                slint::RenderingState::RenderingSetup => {
                    if let slint::GraphicsAPI::NativeOpenGL { get_proc_address } = graphics_api {
                        bridge.borrow_mut().init(get_proc_address);
                    }
                }
                slint::RenderingState::BeforeRendering => {
                    let mut bridge = bridge.borrow_mut();
                    if !bridge.ready() {
                        return;
                    }
                    let mut windows = windows.borrow_mut();
                    for id in std::mem::take(&mut windows.closed) {
                        bridge.remove(id);
                    }
                    let frames: Vec<(u64, Frame)> = windows.pending.drain().collect();
                    for (id, frame) in frames {
                        let Some(image) = bridge.upload(id, &frame) else {
                            continue;
                        };
                        if let Some(&row) = windows.rows.get(&id) {
                            if let Some(mut tile) = model.row_data(row) {
                                tile.texture = image;
                                tile.width = frame.width as f32;
                                tile.height = frame.height as f32;
                                model.set_row_data(row, tile);
                            }
                        }
                    }
                }
                _ => {}
            }
        })
        .unwrap_or_else(|err| log::error!("could not set rendering notifier: {err:?}"));

    // Close button on a window's server-side decoration.
    desktop.on_close_window({
        let cmd_tx = cmd_tx.clone();
        move |id| {
            let _ = cmd_tx.send(slick_wayland::Command::CloseWindow(
                slick_wayland::WindowId(id as u64),
            ));
        }
    });

    // Launcher: run an arbitrary command, pointed at our compositor socket.
    desktop.on_launch({
        let socket_name = socket_name.clone();
        move |cmd| spawn_command(cmd.as_str(), socket_name.borrow().as_deref())
    });

    // Clock: refresh once a second.
    let update_clock = {
        let weak = desktop.as_weak();
        move || {
            if let Some(d) = weak.upgrade() {
                d.set_clock_time(slick_shell::format_clock(Local::now()).into());
            }
        }
    };
    update_clock();
    let clock_timer = slint::Timer::default();
    clock_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(1),
        update_clock,
    );

    // Drain compositor events on the UI thread.
    let event_timer = slint::Timer::default();
    event_timer.start(slint::TimerMode::Repeated, Duration::from_millis(16), {
        let weak = desktop.as_weak();
        let windows = windows.clone();
        let model = model.clone();
        let socket_name = socket_name.clone();
        move || {
            let mut dirty = false;
            while let Ok(event) = rx.try_recv() {
                dirty |= handle_event(event, &model, &windows, &socket_name);
            }
            if dirty {
                if let Some(d) = weak.upgrade() {
                    d.window().request_redraw();
                }
            }
        }
    });

    desktop.run()?;
    Ok(())
}

/// Apply a compositor event. Returns true if a redraw is needed.
fn handle_event(
    event: slick_wayland::Event,
    model: &Rc<VecModel<WindowTile>>,
    windows: &Rc<RefCell<Windows>>,
    socket_name: &Rc<RefCell<Option<String>>>,
) -> bool {
    use slick_wayland::Event;
    match event {
        Event::Ready { socket_name: name } => {
            log::info!("compositor ready on WAYLAND_DISPLAY={name}");
            *socket_name.borrow_mut() = Some(name);
            false
        }
        Event::WindowBuffer {
            id,
            width,
            height,
            pixels,
            title,
            decorated,
        } => {
            let mut windows = windows.borrow_mut();
            if let Some(&row) = windows.rows.get(&id.0) {
                if let Some(mut tile) = model.row_data(row) {
                    tile.title = title.into();
                    tile.decorated = decorated;
                    model.set_row_data(row, tile);
                }
            } else {
                let row = model.row_count();
                let offset = 40.0 + row as f32 * 40.0;
                model.push(WindowTile {
                    id: id.0 as i32,
                    texture: slint::Image::default(),
                    title: title.into(),
                    decorated,
                    x: offset,
                    y: offset,
                    width: width as f32,
                    height: height as f32,
                });
                windows.rows.insert(id.0, row);
            }
            windows.pending.insert(
                id.0,
                Frame {
                    width,
                    height,
                    pixels,
                },
            );
            true
        }
        Event::WindowDecorated { id, decorated } => {
            let windows = windows.borrow();
            if let Some(&row) = windows.rows.get(&id.0) {
                if let Some(mut tile) = model.row_data(row) {
                    tile.decorated = decorated;
                    model.set_row_data(row, tile);
                }
            }
            true
        }
        Event::WindowRemoved(id) => {
            let mut windows = windows.borrow_mut();
            windows.pending.remove(&id.0);
            if let Some(removed) = windows.rows.remove(&id.0) {
                model.remove(removed);
                for row in windows.rows.values_mut() {
                    if *row > removed {
                        *row -= 1;
                    }
                }
                windows.closed.push(id.0);
            }
            true
        }
        other => {
            log::info!("compositor event: {other:?}");
            false
        }
    }
}

/// Spawn a shell command detached, with `WAYLAND_DISPLAY` pointed at our
/// compositor so launched apps connect to us.
fn spawn_command(cmd: &str, wayland_display: Option<&str>) {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return;
    }

    let mut command = std::process::Command::new("/bin/sh");
    command.arg("-c").arg(cmd);
    // Children must connect to slick via WAYLAND_DISPLAY. Remove any inherited
    // WAYLAND_SOCKET (an fd slick received from its own host): libwayland prefers
    // it over WAYLAND_DISPLAY, which would make the child target the wrong
    // compositor and fail with "permission denied". Also drop DISPLAY so GUI
    // toolkits don't silently fall back to X.
    command.env_remove("WAYLAND_SOCKET");
    command.env_remove("DISPLAY");
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
    if let Some(display) = wayland_display {
        command.env("WAYLAND_DISPLAY", display);
        // Point the child at the same runtime dir slick created its socket in,
        // so it cannot end up looking in a different (inaccessible) directory.
        if !runtime_dir.is_empty() {
            command.env("XDG_RUNTIME_DIR", &runtime_dir);
        }
        let socket_path = format!("{runtime_dir}/{display}");
        let exists = std::path::Path::new(&socket_path).exists();
        log::info!(
            "launching with WAYLAND_DISPLAY={display} XDG_RUNTIME_DIR={runtime_dir} \
             (socket {socket_path} exists={exists})"
        );
    } else {
        log::warn!(
            "launching `{cmd}` but slick's WAYLAND_DISPLAY is not known yet; \
             the child will inherit the host's environment"
        );
    }

    match command.spawn() {
        Ok(child) => log::info!("launched `{cmd}` (pid {})", child.id()),
        Err(err) => log::error!("failed to launch `{cmd}`: {err}"),
    }
}
