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

use zbus::zvariant::{ObjectPath, OwnedValue, Value};

/// A file-open request from the portal, to be served on the UI thread.
pub struct Request {
    pub title: String,
    pub reply: async_channel::Sender<Option<PathBuf>>,
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

/// Spawn the portal backend on its own thread. Failures (e.g. no session bus)
/// are logged and otherwise ignored — the shell keeps running.
pub fn spawn(requests: async_channel::Sender<Request>) {
    std::thread::Builder::new()
        .name("s-compositor-portal".into())
        .spawn(move || {
            if let Err(err) = zbus::block_on(serve(requests)) {
                log::warn!("file chooser portal unavailable: {err}");
            }
        })
        .ok();
}

async fn serve(requests: async_channel::Sender<Request>) -> zbus::Result<()> {
    let _conn = zbus::connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, FileChooser { requests })?
        .build()
        .await?;
    log::info!("file chooser portal registered as {BUS_NAME}");
    // Keep the connection alive for the lifetime of the process.
    std::future::pending::<()>().await;
    Ok(())
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
