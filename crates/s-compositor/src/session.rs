//! Session power actions via systemd-logind (`org.freedesktop.login1`).
//!
//! Each request runs a one-shot D-Bus call on a short-lived thread so the UI
//! never blocks. logind enforces its own polkit policy; on a normal single-user
//! seat the active session is allowed these without prompting.

/// A power action the shell can request from logind.
#[derive(Debug, Clone, Copy)]
pub enum PowerAction {
    Suspend,
    Reboot,
    PowerOff,
}

impl PowerAction {
    fn method(self) -> &'static str {
        match self {
            PowerAction::Suspend => "Suspend",
            PowerAction::Reboot => "Reboot",
            PowerAction::PowerOff => "PowerOff",
        }
    }
}

/// Ask logind to perform `action`. Returns immediately; the call happens on a
/// background thread.
pub fn request(action: PowerAction) {
    std::thread::Builder::new()
        .name("s-compositor-session".into())
        .spawn(move || {
            if let Err(err) = zbus::block_on(call(action)) {
                log::warn!("session: {} failed: {err}", action.method());
            }
        })
        .ok();
}

async fn call(action: PowerAction) -> zbus::Result<()> {
    let conn = zbus::Connection::system().await?;
    let manager = zbus::Proxy::new(
        &conn,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .await?;
    // `interactive = false`: don't wait on a polkit agent we don't run.
    manager.call::<_, _, ()>(action.method(), &(false,)).await
}
