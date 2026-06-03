//! slick — a Rust/Slint Wayland desktop shell.
//!
//! Slint owns the main thread (UI + output + input + GL); the Smithay-based
//! Wayland protocol engine runs on its own thread and reports state changes back
//! over a channel which we drain on the UI thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::channel;
use std::time::Duration;

use chrono::Local;
use slint::ComponentHandle;

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

    // The WAYLAND_DISPLAY our compositor created, learned from Event::Ready.
    // Shared between the event pump and the launch callback (both on this thread).
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
    event_timer.start(slint::TimerMode::Repeated, Duration::from_millis(200), {
        let socket_name = socket_name.clone();
        move || {
            while let Ok(event) = rx.try_recv() {
                match event {
                    slick_wayland::Event::Ready { socket_name: name } => {
                        log::info!("compositor ready on WAYLAND_DISPLAY={name}");
                        *socket_name.borrow_mut() = Some(name);
                    }
                    other => log::info!("compositor event: {other:?}"),
                }
            }
        }
    });

    desktop.run()?;
    Ok(())
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
