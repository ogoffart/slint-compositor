//! Battery status via UPower (`org.freedesktop.UPower`) on the system bus.
//!
//! A worker thread polls UPower's display device every few seconds and pushes
//! the battery state to the UI, which shows it in quick settings.

use std::sync::mpsc::Receiver;
use std::time::Duration;

use zbus::zvariant::OwnedObjectPath;
use zbus::{Connection, Proxy};

const UPOWER: &str = "org.freedesktop.UPower";

/// Battery state for the UI.
#[derive(Debug, Clone, Copy)]
pub struct Battery {
    pub present: bool,
    pub percent: f64,
    pub charging: bool,
}

/// Start the UPower poller. Returns the battery receiver, or `None` if the
/// thread can't be spawned.
pub fn spawn() -> Option<Receiver<Battery>> {
    let (tx, rx) = std::sync::mpsc::channel::<Battery>();
    std::thread::Builder::new()
        .name("s-compositor-power".into())
        .spawn(move || {
            let conn = match zbus::block_on(Connection::system()) {
                Ok(c) => c,
                Err(err) => {
                    log::warn!("power: no system bus: {err}");
                    return;
                }
            };
            let device = match zbus::block_on(display_device(&conn)) {
                Ok(d) => d,
                Err(err) => {
                    log::warn!("power: no display device: {err}");
                    return;
                }
            };
            loop {
                if let Ok(battery) = zbus::block_on(read_battery(&conn, &device)) {
                    if tx.send(battery).is_err() {
                        break; // UI gone
                    }
                }
                std::thread::sleep(Duration::from_secs(8));
            }
        })
        .ok()?;
    Some(rx)
}

async fn display_device(conn: &Connection) -> zbus::Result<OwnedObjectPath> {
    let upower = Proxy::new(conn, UPOWER, "/org/freedesktop/UPower", UPOWER).await?;
    upower.call("GetDisplayDevice", &()).await
}

async fn read_battery(conn: &Connection, device: &OwnedObjectPath) -> zbus::Result<Battery> {
    let dev = Proxy::new(
        conn,
        UPOWER,
        device.clone(),
        "org.freedesktop.UPower.Device",
    )
    .await?;
    let present = dev.get_property::<bool>("IsPresent").await.unwrap_or(false);
    let percent = dev.get_property::<f64>("Percentage").await.unwrap_or(0.0);
    // UPower State: 1 = charging, 4 = fully charged (both "on AC").
    let state = dev.get_property::<u32>("State").await.unwrap_or(0);
    Ok(Battery {
        present,
        percent,
        charging: state == 1 || state == 4,
    })
}
