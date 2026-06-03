//! System tray (StatusNotifierItem / SNI) host.
//!
//! Planned for milestone M6: this crate will wrap the `system-tray` crate on a
//! dedicated Tokio thread, host the `StatusNotifierWatcher`, and forward tray
//! item add/update/remove events (with icon pixmaps) into the Slint tray model
//! via `slint::invoke_from_event_loop`. Menu activations are routed back out
//! through the DBusMenu protocol.
//!
//! It is intentionally a stub for now so the workspace layout is in place.

/// A tray item to be displayed in the panel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrayItem {
    pub id: String,
    pub title: String,
    pub icon_name: String,
}
