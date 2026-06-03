//! Wi-Fi via NetworkManager over D-Bus (system bus).
//!
//! Runs on its own thread using zbus's executor. The UI sends [`NetCommand`]s
//! (scan / toggle radio / connect with a password) and receives [`NetEvent`]s
//! (the access-point list and radio state) which it renders in quick settings.
//!
//! Dynamic (untyped) proxies are used to avoid hand-writing the many
//! NetworkManager interface definitions.

use std::collections::HashMap;

use zbus::zvariant::{OwnedObjectPath, Value};
use zbus::{Connection, Proxy};

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";

/// One scanned access point.
#[derive(Debug, Clone)]
pub struct WifiNet {
    pub ssid: String,
    pub strength: u8,
    pub secure: bool,
    pub active: bool,
}

/// State pushed to the UI thread.
#[derive(Debug, Clone)]
pub enum NetEvent {
    Networks(Vec<WifiNet>),
    Enabled(bool),
}

/// Requests from the UI thread.
#[derive(Debug, Clone)]
pub enum NetCommand {
    Scan,
    Toggle,
    Connect { ssid: String, password: String },
}

/// Start the NetworkManager worker. Returns the event receiver (drained on the
/// UI thread) and command sender, or `None` if the thread can't spawn.
pub fn spawn() -> Option<(
    async_channel::Receiver<NetEvent>,
    async_channel::Sender<NetCommand>,
)> {
    let (ev_tx, ev_rx) = async_channel::unbounded::<NetEvent>();
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<NetCommand>();
    std::thread::Builder::new()
        .name("s-compositor-net".into())
        .spawn(move || {
            if let Err(err) = zbus::block_on(serve(ev_tx, cmd_rx)) {
                log::warn!("network: worker exited: {err}");
            }
        })
        .ok()?;
    Some((ev_rx, cmd_tx))
}

async fn serve(
    ev_tx: async_channel::Sender<NetEvent>,
    cmd_rx: async_channel::Receiver<NetCommand>,
) -> zbus::Result<()> {
    let conn = Connection::system().await?;
    let nm = Proxy::new(&conn, NM, NM_PATH, NM).await?;

    // Initial snapshot.
    let _ = ev_tx
        .send(NetEvent::Enabled(wireless_enabled(&nm).await))
        .await;
    if let Ok(nets) = scan(&conn, &nm).await {
        let _ = ev_tx.send(NetEvent::Networks(nets)).await;
    }

    while let Ok(cmd) = cmd_rx.recv().await {
        match cmd {
            NetCommand::Scan => {
                request_scan(&conn, &nm).await;
                if let Ok(nets) = scan(&conn, &nm).await {
                    let _ = ev_tx.send(NetEvent::Networks(nets)).await;
                }
            }
            NetCommand::Toggle => {
                let now = wireless_enabled(&nm).await;
                let _ = nm.set_property("WirelessEnabled", !now).await;
                let _ = ev_tx.send(NetEvent::Enabled(!now)).await;
            }
            NetCommand::Connect { ssid, password } => {
                if let Err(err) = connect(&conn, &nm, &ssid, &password).await {
                    log::warn!("network: connect to {ssid:?} failed: {err}");
                }
                if let Ok(nets) = scan(&conn, &nm).await {
                    let _ = ev_tx.send(NetEvent::Networks(nets)).await;
                }
            }
        }
    }
    Ok(())
}

async fn wireless_enabled(nm: &Proxy<'_>) -> bool {
    nm.get_property::<bool>("WirelessEnabled")
        .await
        .unwrap_or(false)
}

/// The first Wi-Fi device object path (DeviceType 2 == NM_DEVICE_TYPE_WIFI).
async fn wifi_device(conn: &Connection, nm: &Proxy<'_>) -> zbus::Result<OwnedObjectPath> {
    let devices: Vec<OwnedObjectPath> = nm.call("GetDevices", &()).await?;
    for dev in devices {
        let p = Proxy::new(
            conn,
            NM,
            dev.clone(),
            "org.freedesktop.NetworkManager.Device",
        )
        .await?;
        if p.get_property::<u32>("DeviceType").await.unwrap_or(0) == 2 {
            return Ok(dev);
        }
    }
    Err(zbus::Error::Failure("no wifi device".into()))
}

async fn request_scan(conn: &Connection, nm: &Proxy<'_>) {
    let Ok(dev) = wifi_device(conn, nm).await else {
        return;
    };
    if let Ok(wifi) = Proxy::new(
        conn,
        NM,
        dev,
        "org.freedesktop.NetworkManager.Device.Wireless",
    )
    .await
    {
        let empty: HashMap<&str, Value> = HashMap::new();
        let _ = wifi.call::<_, _, ()>("RequestScan", &(empty,)).await;
    }
}

async fn scan(conn: &Connection, nm: &Proxy<'_>) -> zbus::Result<Vec<WifiNet>> {
    let dev = wifi_device(conn, nm).await?;
    let wifi = Proxy::new(
        conn,
        NM,
        dev,
        "org.freedesktop.NetworkManager.Device.Wireless",
    )
    .await?;
    let active = wifi
        .get_property::<OwnedObjectPath>("ActiveAccessPoint")
        .await
        .ok();
    let aps: Vec<OwnedObjectPath> = wifi.call("GetAllAccessPoints", &()).await?;

    let mut nets: Vec<WifiNet> = Vec::new();
    for ap in aps {
        let p = Proxy::new(
            conn,
            NM,
            ap.clone(),
            "org.freedesktop.NetworkManager.AccessPoint",
        )
        .await?;
        let ssid_bytes = p.get_property::<Vec<u8>>("Ssid").await.unwrap_or_default();
        if ssid_bytes.is_empty() {
            continue;
        }
        let ssid = String::from_utf8_lossy(&ssid_bytes).into_owned();
        let strength = p.get_property::<u8>("Strength").await.unwrap_or(0);
        let wpa = p.get_property::<u32>("WpaFlags").await.unwrap_or(0);
        let rsn = p.get_property::<u32>("RsnFlags").await.unwrap_or(0);
        let secure = wpa != 0 || rsn != 0;
        let is_active = active.as_ref().map(|a| a == &ap).unwrap_or(false);

        // De-duplicate by SSID, keeping the strongest.
        if let Some(existing) = nets.iter_mut().find(|n| n.ssid == ssid) {
            if strength > existing.strength {
                existing.strength = strength;
            }
            existing.active |= is_active;
            existing.secure |= secure;
        } else {
            nets.push(WifiNet {
                ssid,
                strength,
                secure,
                active: is_active,
            });
        }
    }
    nets.sort_by(|a, b| b.active.cmp(&a.active).then(b.strength.cmp(&a.strength)));
    Ok(nets)
}

/// Add and activate a Wi-Fi connection for `ssid` (WPA-PSK when a password is
/// given, open otherwise).
async fn connect(
    conn: &Connection,
    nm: &Proxy<'_>,
    ssid: &str,
    password: &str,
) -> zbus::Result<()> {
    let dev = wifi_device(conn, nm).await?;

    let mut settings: HashMap<&str, HashMap<&str, Value>> = HashMap::new();
    let mut connection = HashMap::new();
    connection.insert("id", Value::from(ssid));
    connection.insert("type", Value::from("802-11-wireless"));
    settings.insert("connection", connection);

    let mut wireless = HashMap::new();
    wireless.insert("ssid", Value::from(ssid.as_bytes().to_vec()));
    wireless.insert("mode", Value::from("infrastructure"));
    settings.insert("802-11-wireless", wireless);

    if !password.is_empty() {
        let mut security = HashMap::new();
        security.insert("key-mgmt", Value::from("wpa-psk"));
        security.insert("psk", Value::from(password));
        settings.insert("802-11-wireless-security", security);
    }

    let specific = OwnedObjectPath::try_from("/").expect("root path");
    let _: (OwnedObjectPath, OwnedObjectPath) = nm
        .call("AddAndActivateConnection", &(settings, &dev, &specific))
        .await?;
    Ok(())
}
