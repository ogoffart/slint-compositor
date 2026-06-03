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
use slint::{ComponentHandle, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel};

slint::include_modules!();

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Spawn the Wayland compositor on its own thread.
    let (tx, rx) = channel::<slick_wayland::Event>();
    std::thread::Builder::new()
        .name("slick-wayland".into())
        .spawn(move || {
            if let Err(err) = slick_wayland::run(tx) {
                log::error!("wayland thread exited: {err:?}");
            }
        })?;

    let desktop = Desktop::new()?;

    // Model backing the composited client windows.
    let windows = Rc::new(VecModel::<WindowTile>::default());
    desktop.set_windows(ModelRc::from(windows.clone()));
    // window id -> row index in the model.
    let rows: Rc<RefCell<HashMap<u64, usize>>> = Rc::new(RefCell::new(HashMap::new()));

    // The WAYLAND_DISPLAY our compositor created, learned from Event::Ready.
    let socket_name: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

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
        let socket_name = socket_name.clone();
        let windows = windows.clone();
        let rows = rows.clone();
        move || {
            while let Ok(event) = rx.try_recv() {
                handle_event(event, &windows, &rows, &socket_name);
            }
        }
    });

    desktop.run()?;
    Ok(())
}

fn handle_event(
    event: slick_wayland::Event,
    windows: &Rc<VecModel<WindowTile>>,
    rows: &Rc<RefCell<HashMap<u64, usize>>>,
    socket_name: &Rc<RefCell<Option<String>>>,
) {
    use slick_wayland::Event;
    match event {
        Event::Ready { socket_name: name } => {
            log::info!("compositor ready on WAYLAND_DISPLAY={name}");
            *socket_name.borrow_mut() = Some(name);
        }
        Event::WindowBuffer {
            id,
            width,
            height,
            pixels,
        } => {
            let texture = make_image(width, height, &pixels);
            let mut rows = rows.borrow_mut();
            if let Some(&row) = rows.get(&id.0) {
                if let Some(mut tile) = windows.row_data(row) {
                    tile.texture = texture;
                    tile.width = width as f32;
                    tile.height = height as f32;
                    windows.set_row_data(row, tile);
                }
            } else {
                let row = windows.row_count();
                let offset = 40.0 + row as f32 * 40.0;
                windows.push(WindowTile {
                    id: id.0 as i32,
                    texture,
                    x: offset,
                    y: offset,
                    width: width as f32,
                    height: height as f32,
                });
                rows.insert(id.0, row);
            }
        }
        Event::WindowRemoved(id) => {
            let mut rows = rows.borrow_mut();
            if let Some(removed) = rows.remove(&id.0) {
                windows.remove(removed);
                // Keep the id->row map consistent after the shift.
                for row in rows.values_mut() {
                    if *row > removed {
                        *row -= 1;
                    }
                }
            }
        }
        other => log::info!("compositor event: {other:?}"),
    }
}

/// Build a Slint image from tightly-packed RGBA8 pixels.
fn make_image(width: u32, height: u32, pixels: &[u8]) -> slint::Image {
    let mut buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
    let bytes = buffer.make_mut_bytes();
    let n = bytes.len().min(pixels.len());
    bytes[..n].copy_from_slice(&pixels[..n]);
    slint::Image::from_rgba8(buffer)
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
    if let Some(display) = wayland_display {
        command.env("WAYLAND_DISPLAY", display);
    }

    match command.spawn() {
        Ok(child) => log::info!("launched `{cmd}` (pid {})", child.id()),
        Err(err) => log::error!("failed to launch `{cmd}`: {err}"),
    }
}
