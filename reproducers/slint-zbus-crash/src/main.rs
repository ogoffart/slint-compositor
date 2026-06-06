//! Reproducer (via Slint): a minimal Slint winit app panics at startup when
//! zbus's `tokio` feature is enabled in the dependency tree and there is no
//! Tokio runtime on the UI thread.
//!
//!     thread 'main' panicked at zbus .../abstractions/executor.rs:190:
//!     there is no reactor running, must be called from the context of a Tokio 1.x runtime
//!     ...
//!     i_slint_backend_winit::xdg_color_scheme::watch
//!     i_slint_backend_winit::winitwindowadapter::WinitWindowAdapter::spawn_xdg_settings_watcher
//!
//! Slint's winit backend spawns an XDG colour-scheme watcher that uses zbus.
//! With zbus built in `tokio` mode (here via the direct `zbus` dep; in a larger
//! workspace via feature unification from e.g. `system-tray`) that watcher calls
//! `tokio::spawn_blocking`, which panics without a Tokio runtime — taking down
//! the whole window with a blank screen.
//!
//! Run under a display, e.g.:  `Xvfb :99 & DISPLAY=:99 cargo run`
//! Workaround: enter a Tokio runtime on the main thread before `run_event_loop`.
slint::slint! {
    export component Win inherits Window {
        width: 240px;
        height: 120px;
        Text { text: "no crash: the XDG watcher survived"; }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let win = Win::new()?;
    // If no panic occurs, quit shortly so this doesn't hang the (success) case.
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::SingleShot,
        std::time::Duration::from_secs(2),
        || {
            let _ = slint::quit_event_loop();
        },
    );
    win.show()?;
    slint::run_event_loop()?;
    println!("exited cleanly — no zbus/tokio panic");
    Ok(())
}
