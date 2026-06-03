//! slick — a Rust/Slint Wayland desktop shell.
//!
//! Slint owns the main thread (UI + output + input + GL); the Smithay-based
//! Wayland protocol engine runs on its own thread and reports state changes back
//! over a channel which we drain on the UI thread.

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

    // Drain compositor events on the UI thread (M0: just log them).
    let event_timer = slint::Timer::default();
    event_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(200),
        move || {
            while let Ok(event) = rx.try_recv() {
                log::info!("compositor event: {event:?}");
            }
        },
    );

    desktop.run()?;
    Ok(())
}
