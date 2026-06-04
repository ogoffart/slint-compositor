//! Headless smoke test for the Wayland protocol path.
//!
//! Spawns the real compositor event loop on a thread (no GPU/display needed —
//! that lives in the UI binary), then connects a genuine Wayland client and
//! enumerates the advertised globals, just like `wayland-info`. If the client
//! handshake hangs, this reproduces "no clients show" without any hardware.
//!
//! Run with: `cargo run -p s-compositor-wayland --example headless_client`

use std::time::Duration;

use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};

#[derive(Default)]
struct State {
    globals: Vec<(u32, String, u32)>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            state.globals.push((name, interface, version));
        }
    }
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Start the compositor's event loop on its own thread.
    let (tx, rx) = std::sync::mpsc::channel::<s_compositor_wayland::Event>();
    let (_cmd_tx, cmd_rx) = s_compositor_wayland::command_channel();
    std::thread::spawn(move || {
        if let Err(err) = s_compositor_wayland::run(tx, cmd_rx, Vec::new()) {
            eprintln!("compositor thread error: {err:?}");
        }
    });

    // Wait for it to announce its socket.
    let (socket, runtime) = loop {
        match rx.recv().expect("compositor exited before Ready") {
            s_compositor_wayland::Event::Ready {
                socket_name,
                runtime_dir,
                ..
            } => break (socket_name, runtime_dir),
            _ => {}
        }
    };
    println!("compositor ready: WAYLAND_DISPLAY={socket} XDG_RUNTIME_DIR={runtime}");

    // Keep draining compositor events so its channel never backs up.
    std::thread::spawn(move || while rx.recv().is_ok() {});

    std::env::set_var("WAYLAND_DISPLAY", &socket);
    std::env::set_var("XDG_RUNTIME_DIR", &runtime);

    // A watchdog: if the handshake hangs (the bug), fail loudly instead of
    // blocking forever.
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(8));
        eprintln!("TIMEOUT: client handshake did not complete in 8s — bug reproduced");
        std::process::exit(2);
    });

    let conn = Connection::connect_to_env().expect("client failed to connect");
    let display = conn.display();
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    let _registry = display.get_registry(&qh, ());

    let mut state = State::default();
    println!("client connected; requesting globals…");
    queue.roundtrip(&mut state).expect("roundtrip failed");

    println!("\nGOT {} globals:", state.globals.len());
    for (name, interface, version) in &state.globals {
        println!("  [{name}] {interface} v{version}");
    }
    if state.globals.is_empty() {
        eprintln!("NO globals received — handshake is broken");
        std::process::exit(1);
    }
    println!("\nOK: registry handshake works.");
    std::process::exit(0);
}
