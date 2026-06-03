//! s-compositor — a Rust/Slint Wayland desktop shell.
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

mod file_dialog;
mod gl_bridge;
mod portal;
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
    /// The currently focused window.
    focused: Option<u64>,
}

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Spawn the Wayland compositor on its own thread.
    let (tx, rx) = channel::<s_compositor_wayland::Event>();
    let (cmd_tx, cmd_rx) = s_compositor_wayland::command_channel();
    std::thread::Builder::new()
        .name("s-compositor-wayland".into())
        .spawn(move || {
            if let Err(err) = s_compositor_wayland::run(tx, cmd_rx) {
                log::error!("wayland thread exited: {err:?}");
            }
        })?;

    let desktop = Desktop::new()?;

    let model = Rc::new(VecModel::<WindowTile>::default());
    desktop.set_windows(ModelRc::from(model.clone()));
    let windows = Rc::new(RefCell::new(Windows::default()));
    let bridge = Rc::new(RefCell::new(GlBridge::default()));
    // (WAYLAND_DISPLAY, XDG_RUNTIME_DIR) of s-compositor's compositor, learned from Ready.
    let wayland_env: Rc<RefCell<Option<(String, String)>>> = Rc::new(RefCell::new(None));

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
            let _ = cmd_tx.send(s_compositor_wayland::Command::CloseWindow(
                s_compositor_wayland::WindowId(id as u64),
            ));
        }
    });

    // Reusable file dialog (also used by the portal backend).
    let fd_items = Rc::new(VecModel::<FileItem>::default());
    desktop.set_file_dialog_items(ModelRc::from(fd_items.clone()));
    let file_dialog: file_dialog::SharedController = Rc::new(RefCell::new(
        file_dialog::Controller::new(desktop.as_weak(), fd_items),
    ));

    // Expose the file dialog to other apps via the XDG FileChooser portal.
    let (portal_tx, portal_rx) = async_channel::unbounded::<portal::Request>();
    portal::spawn(portal_tx);

    desktop.on_change_background({
        let file_dialog = file_dialog.clone();
        let weak = desktop.as_weak();
        move || {
            let weak = weak.clone();
            file_dialog.borrow_mut().open(
                "Select background image",
                file_dialog::home_dir(),
                true,
                Box::new(move |path| {
                    if let (Some(path), Some(d)) = (path, weak.upgrade()) {
                        match slint::Image::load_from_path(&path) {
                            Ok(image) => d.set_background_image(image),
                            Err(err) => log::error!("failed to load image {path:?}: {err}"),
                        }
                    }
                }),
            );
        }
    });
    desktop.on_fd_entry_clicked({
        let file_dialog = file_dialog.clone();
        move |idx| file_dialog.borrow_mut().entry_clicked(idx)
    });
    desktop.on_fd_go_up({
        let file_dialog = file_dialog.clone();
        move || file_dialog.borrow_mut().go_up()
    });
    desktop.on_fd_accept({
        let file_dialog = file_dialog.clone();
        move || file_dialog.borrow_mut().accept()
    });
    desktop.on_fd_cancel({
        let file_dialog = file_dialog.clone();
        move || file_dialog.borrow_mut().cancel()
    });

    // Launcher: run an arbitrary command, pointed at our compositor socket.
    desktop.on_launch({
        let wayland_env = wayland_env.clone();
        move |cmd| {
            let env = wayland_env.borrow();
            let env = env.as_ref().map(|(d, r)| (d.as_str(), r.as_str()));
            spawn_command(cmd.as_str(), env);
        }
    });

    // Raise + focus a window (taskbar click or clicking the window).
    desktop.on_activate_window({
        let cmd_tx = cmd_tx.clone();
        let model = model.clone();
        let windows = windows.clone();
        move |id| {
            raise_and_focus(&model, &windows, id as u64);
            let _ = cmd_tx.send(s_compositor_wayland::Command::FocusWindow(
                s_compositor_wayland::WindowId(id as u64),
            ));
        }
    });

    // Move a window by dragging its title bar.
    desktop.on_move_window({
        let model = model.clone();
        let windows = windows.clone();
        move |id, dx, dy| {
            let windows = windows.borrow();
            if let Some(&row) = windows.rows.get(&(id as u64)) {
                if let Some(mut tile) = model.row_data(row) {
                    tile.x += dx;
                    tile.y += dy;
                    model.set_row_data(row, tile);
                }
            }
        }
    });

    // Resize a window via the resize grip (asks the client to reconfigure).
    desktop.on_resize_window({
        let cmd_tx = cmd_tx.clone();
        let model = model.clone();
        let windows = windows.clone();
        move |id, dw, dh| {
            let windows = windows.borrow();
            if let Some(&row) = windows.rows.get(&(id as u64)) {
                if let Some(tile) = model.row_data(row) {
                    let _ = cmd_tx.send(s_compositor_wayland::Command::ResizeWindow {
                        id: s_compositor_wayland::WindowId(id as u64),
                        width: (tile.width + dw).max(1.0) as i32,
                        height: (tile.height + dh).max(1.0) as i32,
                    });
                }
            }
        }
    });

    // Pointer events over a client window.
    desktop.on_pointer_window({
        let cmd_tx = cmd_tx.clone();
        move |id, x, y, kind, btn| {
            let id = s_compositor_wayland::WindowId(id as u64);
            let cmd = match kind {
                1 | 2 => s_compositor_wayland::Command::PointerButton {
                    id,
                    button: evdev_button(btn),
                    pressed: kind == 1,
                },
                _ => s_compositor_wayland::Command::PointerMotion {
                    id,
                    x: x as f64,
                    y: y as f64,
                },
            };
            let _ = cmd_tx.send(cmd);
        }
    });

    desktop.on_scroll_window({
        let cmd_tx = cmd_tx.clone();
        move |id, dx, dy| {
            // Wayland axis is positive-down; Slint scroll delta is positive-up.
            let _ = cmd_tx.send(s_compositor_wayland::Command::PointerAxis {
                id: s_compositor_wayland::WindowId(id as u64),
                dx: -dx as f64,
                dy: -dy as f64,
            });
        }
    });

    // Keyboard: forward each press as a (modifier-wrapped) key tap.
    desktop.on_key_window({
        let cmd_tx = cmd_tx.clone();
        move |_id, text, pressed, ctrl, alt, shift| {
            if pressed {
                forward_key(&cmd_tx, text.as_str(), ctrl, alt, shift);
            }
        }
    });

    // Clock: refresh once a second.
    let update_clock = {
        let weak = desktop.as_weak();
        move || {
            if let Some(d) = weak.upgrade() {
                d.set_clock_time(s_compositor_shell::format_clock(Local::now()).into());
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
        let wayland_env = wayland_env.clone();
        let file_dialog = file_dialog.clone();
        move || {
            let mut dirty = false;
            while let Ok(event) = rx.try_recv() {
                dirty |= handle_event(event, &model, &windows, &wayland_env);
            }
            // Serve pending portal file-open requests with the same dialog.
            while let Ok(request) = portal_rx.try_recv() {
                let reply = request.reply;
                file_dialog.borrow_mut().open(
                    &request.title,
                    file_dialog::home_dir(),
                    false,
                    Box::new(move |path| {
                        let _ = reply.try_send(path);
                    }),
                );
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
    event: s_compositor_wayland::Event,
    model: &Rc<VecModel<WindowTile>>,
    windows: &Rc<RefCell<Windows>>,
    wayland_env: &Rc<RefCell<Option<(String, String)>>>,
) -> bool {
    use s_compositor_wayland::Event;
    match event {
        Event::Ready {
            socket_name,
            runtime_dir,
        } => {
            log::info!(
                "compositor ready on WAYLAND_DISPLAY={socket_name} (XDG_RUNTIME_DIR={runtime_dir})"
            );
            *wayland_env.borrow_mut() = Some((socket_name, runtime_dir));
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
                    focused: false,
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

/// Raise a window to the top of the stack and mark it focused. Rebuilds the
/// id->row map from the resulting model order.
fn raise_and_focus(model: &Rc<VecModel<WindowTile>>, windows: &Rc<RefCell<Windows>>, id: u64) {
    let mut w = windows.borrow_mut();
    w.focused = Some(id);
    if let Some(&row) = w.rows.get(&id) {
        if row + 1 != model.row_count() {
            if let Some(tile) = model.row_data(row) {
                model.remove(row);
                model.push(tile);
            }
        }
    }
    for i in 0..model.row_count() {
        if let Some(mut tile) = model.row_data(i) {
            w.rows.insert(tile.id as u64, i);
            let want = tile.id as u64 == id;
            if tile.focused != want {
                tile.focused = want;
                model.set_row_data(i, tile);
            }
        }
    }
}

/// Map a Slint pointer-button index (1=left, 2=right, 3=middle) to an evdev code.
fn evdev_button(btn: i32) -> u32 {
    match btn {
        2 => 0x111, // BTN_RIGHT
        3 => 0x112, // BTN_MIDDLE
        _ => 0x110, // BTN_LEFT
    }
}

/// Forward one key press as a modifier-wrapped tap to the focused window.
fn forward_key(
    cmd_tx: &s_compositor_wayland::CommandSender<s_compositor_wayland::Command>,
    text: &str,
    ctrl: bool,
    alt: bool,
    shift_mod: bool,
) {
    let Some((keycode, needs_shift)) = evdev_keycode(text) else {
        return;
    };
    let mut mods = Vec::new();
    if ctrl {
        mods.push(29); // KEY_LEFTCTRL
    }
    if alt {
        mods.push(56); // KEY_LEFTALT
    }
    if needs_shift || shift_mod {
        mods.push(42); // KEY_LEFTSHIFT
    }
    let key = |keycode: u32, pressed: bool| {
        let _ = cmd_tx.send(s_compositor_wayland::Command::Key { keycode, pressed });
    };
    for m in &mods {
        key(*m, true);
    }
    key(keycode, true);
    key(keycode, false);
    for m in mods.iter().rev() {
        key(*m, false);
    }
}

/// Best-effort mapping of Slint key text to a US-layout evdev keycode plus
/// whether Shift is needed. Returns `None` for unmapped keys.
fn evdev_keycode(text: &str) -> Option<(u32, bool)> {
    let mut chars = text.chars();
    let c = chars.next()?;
    // Named keys (Slint encodes these as specific control/private-use chars).
    match c {
        '\u{000a}' | '\r' => return Some((28, false)), // Return
        '\u{0008}' => return Some((14, false)),        // Backspace
        '\u{0009}' => return Some((15, false)),        // Tab
        '\u{001b}' => return Some((1, false)),         // Escape
        '\u{007f}' => return Some((111, false)),       // Delete
        '\u{f700}' => return Some((103, false)),       // Up
        '\u{f701}' => return Some((108, false)),       // Down
        '\u{f702}' => return Some((105, false)),       // Left
        '\u{f703}' => return Some((106, false)),       // Right
        ' ' => return Some((57, false)),               // Space
        _ => {}
    }
    // Beyond here we only handle single printable characters.
    if chars.next().is_some() {
        return None;
    }
    if c.is_ascii_alphabetic() {
        let code = match c.to_ascii_lowercase() {
            'a' => 30,
            'b' => 48,
            'c' => 46,
            'd' => 32,
            'e' => 18,
            'f' => 33,
            'g' => 34,
            'h' => 35,
            'i' => 23,
            'j' => 36,
            'k' => 37,
            'l' => 38,
            'm' => 50,
            'n' => 49,
            'o' => 24,
            'p' => 25,
            'q' => 16,
            'r' => 19,
            's' => 31,
            't' => 20,
            'u' => 22,
            'v' => 47,
            'w' => 17,
            'x' => 45,
            'y' => 21,
            'z' => 44,
            _ => return None,
        };
        return Some((code, c.is_ascii_uppercase()));
    }
    let mapped = match c {
        '1' => (2, false),
        '2' => (3, false),
        '3' => (4, false),
        '4' => (5, false),
        '5' => (6, false),
        '6' => (7, false),
        '7' => (8, false),
        '8' => (9, false),
        '9' => (10, false),
        '0' => (11, false),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        '\\' => (43, false),
        '|' => (43, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        _ => return None,
    };
    Some(mapped)
}

/// Spawn a shell command detached, pointed at s-compositor's compositor. `wayland` is
/// `(WAYLAND_DISPLAY, XDG_RUNTIME_DIR)` of s-compositor's own socket.
fn spawn_command(cmd: &str, wayland: Option<(&str, &str)>) {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return;
    }

    let mut command = std::process::Command::new("/bin/sh");
    command.arg("-c").arg(cmd);
    // Children must connect to s-compositor. Remove any inherited WAYLAND_SOCKET (an fd
    // s-compositor received from its own host): libwayland prefers it over
    // WAYLAND_DISPLAY, which would make the child target the wrong compositor and
    // fail with "permission denied". Also drop DISPLAY so toolkits don't fall
    // back to X.
    command.env_remove("WAYLAND_SOCKET");
    command.env_remove("DISPLAY");
    if let Some((display, runtime_dir)) = wayland {
        command.env("WAYLAND_DISPLAY", display);
        command.env("XDG_RUNTIME_DIR", runtime_dir);
        let socket_path = format!("{runtime_dir}/{display}");
        let exists = std::path::Path::new(&socket_path).exists();
        log::info!(
            "launching with WAYLAND_DISPLAY={display} XDG_RUNTIME_DIR={runtime_dir} \
             (socket {socket_path} exists={exists})"
        );
    } else {
        log::warn!(
            "launching `{cmd}` but s-compositor's compositor socket is not ready yet; \
             the child will inherit the host's environment"
        );
    }

    match command.spawn() {
        Ok(child) => log::info!("launched `{cmd}` (pid {})", child.id()),
        Err(err) => log::error!("failed to launch `{cmd}`: {err}"),
    }
}
