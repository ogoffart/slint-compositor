//! XDG desktop portal backend: `org.freedesktop.impl.portal.FileChooser`.
//!
//! This lets *other* applications open files through s-compositor's own file
//! dialog. `xdg-desktop-portal` (the `org.freedesktop.portal.Desktop` frontend)
//! routes `FileChooser` requests to this backend, which raises the same dialog
//! used in-shell and returns the chosen file as a `file://` URI.
//!
//! The DBus side runs on its own thread; each request is forwarded to the UI
//! thread (which owns the dialog) over an async channel, and the reply travels
//! back the same way.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use zbus::zvariant::{ObjectPath, OwnedValue, Structure, Value};

/// A file-open request from the portal, to be served on the UI thread.
pub struct Request {
    pub title: String,
    pub reply: async_channel::Sender<Option<PathBuf>>,
}

/// Appearance settings published over `org.freedesktop.appearance`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Appearance {
    /// 0 = no preference, 1 = prefer dark, 2 = prefer light.
    pub scheme: u32,
    /// Accent colour as RGB components in 0..1.
    pub accent: (f64, f64, f64),
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            scheme: 1,
            accent: (0.54, 0.71, 0.98),
        }
    }
}

const APPEARANCE_NS: &str = "org.freedesktop.appearance";

fn scheme_value(scheme: u32) -> OwnedValue {
    Value::from(scheme).try_into().expect("u32 -> OwnedValue")
}

fn accent_value(accent: (f64, f64, f64)) -> OwnedValue {
    let structure = Structure::from((accent.0, accent.1, accent.2));
    Value::from(structure)
        .try_into()
        .expect("(ddd) -> OwnedValue")
}

/// The well-known DBus name and object path of our portal backend.
const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.scompositor";
const OBJECT_PATH: &str = "/org/freedesktop/portal/desktop";

struct FileChooser {
    requests: async_channel::Sender<Request>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.FileChooser")]
impl FileChooser {
    /// Ask the user to pick a file (or directory). Returns `(response, results)`
    /// where response 0 = success, 1 = cancelled, and `results["uris"]` holds the
    /// chosen `file://` URIs.
    #[allow(clippy::too_many_arguments)]
    async fn open_file(
        &self,
        _handle: ObjectPath<'_>,
        _app_id: String,
        _parent_window: String,
        title: String,
        _options: HashMap<String, Value<'_>>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let (reply_tx, reply_rx) = async_channel::bounded(1);
        if self
            .requests
            .send(Request {
                title: if title.is_empty() {
                    "Open File".into()
                } else {
                    title
                },
                reply: reply_tx,
            })
            .await
            .is_err()
        {
            return (2, HashMap::new()); // backend gone
        }

        match reply_rx.recv().await {
            Ok(Some(path)) => {
                let uri = path_to_uri(&path);
                let mut results = HashMap::new();
                if let Ok(value) = OwnedValue::try_from(Value::from(vec![uri])) {
                    results.insert("uris".to_string(), value);
                }
                (0, results)
            }
            _ => (1, HashMap::new()),
        }
    }
}

/// Implements `org.freedesktop.impl.portal.Settings`, publishing the accent
/// colour and light/dark scheme so other apps can match the shell.
struct Settings {
    appearance: Arc<Mutex<Appearance>>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Settings")]
impl Settings {
    async fn read_all(
        &self,
        _namespaces: Vec<String>,
    ) -> HashMap<String, HashMap<String, OwnedValue>> {
        let appearance = *self.appearance.lock().unwrap();
        let mut values = HashMap::new();
        values.insert("color-scheme".to_string(), scheme_value(appearance.scheme));
        values.insert("accent-color".to_string(), accent_value(appearance.accent));
        HashMap::from([(APPEARANCE_NS.to_string(), values)])
    }

    async fn read(&self, namespace: String, key: String) -> zbus::fdo::Result<OwnedValue> {
        let appearance = *self.appearance.lock().unwrap();
        match (namespace.as_str(), key.as_str()) {
            (APPEARANCE_NS, "color-scheme") => Ok(scheme_value(appearance.scheme)),
            (APPEARANCE_NS, "accent-color") => Ok(accent_value(appearance.accent)),
            _ => Err(zbus::fdo::Error::Failed(format!(
                "no such setting {namespace}/{key}"
            ))),
        }
    }
}

/// Spawn the portal backend on its own thread. Failures (e.g. no session bus)
/// are logged and otherwise ignored — the shell keeps running.
pub fn spawn(
    requests: async_channel::Sender<Request>,
    appearance: Arc<Mutex<Appearance>>,
    updates: async_channel::Receiver<Appearance>,
) {
    std::thread::Builder::new()
        .name("s-compositor-portal".into())
        .spawn(move || {
            if let Err(err) = zbus::block_on(serve(requests, appearance, updates)) {
                log::warn!("desktop portal unavailable: {err}");
            }
        })
        .ok();
}

async fn serve(
    requests: async_channel::Sender<Request>,
    appearance: Arc<Mutex<Appearance>>,
    updates: async_channel::Receiver<Appearance>,
) -> zbus::Result<()> {
    let conn = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, FileChooser { requests })?
        .serve_at(
            OBJECT_PATH,
            Settings {
                appearance: appearance.clone(),
            },
        )?
        .build()
        .await?;
    log::info!("desktop portal registered as {BUS_NAME}");

    // Publish appearance changes coming from the UI thread, emitting the
    // standard SettingChanged signal so apps update live. Recv keeps the
    // connection alive for the lifetime of the process.
    while let Ok(next) = updates.recv().await {
        *appearance.lock().unwrap() = next;
        emit_setting_changed(&conn, "color-scheme", Value::from(next.scheme)).await;
        let accent = Value::from(Structure::from((
            next.accent.0,
            next.accent.1,
            next.accent.2,
        )));
        emit_setting_changed(&conn, "accent-color", accent).await;
    }
    Ok(())
}

async fn emit_setting_changed(conn: &zbus::Connection, key: &str, value: Value<'_>) {
    if let Err(err) = conn
        .emit_signal(
            None::<&str>,
            OBJECT_PATH,
            "org.freedesktop.impl.portal.Settings",
            "SettingChanged",
            &(APPEARANCE_NS, key, value),
        )
        .await
    {
        log::warn!("failed to emit SettingChanged: {err}");
    }
}

/// Convert a path to a `file://` URI, percent-encoding unsafe bytes.
fn path_to_uri(path: &std::path::Path) -> String {
    const UNRESERVED: &[u8] = b"-_.~/";
    let mut uri = String::from("file://");
    for &byte in path.to_string_lossy().as_bytes() {
        if byte.is_ascii_alphanumeric() || UNRESERVED.contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::path_to_uri;
    use std::path::Path;

    #[test]
    fn simple_path() {
        assert_eq!(path_to_uri(Path::new("/a/b")), "file:///a/b");
    }

    #[test]
    fn spaces_and_specials_are_percent_encoded() {
        assert_eq!(
            path_to_uri(Path::new("/home/u/My File.png")),
            "file:///home/u/My%20File.png"
        );
        assert_eq!(path_to_uri(Path::new("/a#b?c")), "file:///a%23b%3Fc");
    }

    #[test]
    fn unreserved_chars_kept() {
        assert_eq!(path_to_uri(Path::new("/a-b_c.d~e")), "file:///a-b_c.d~e");
    }
}
