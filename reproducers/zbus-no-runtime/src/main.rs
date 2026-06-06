//! Reproducer: with zbus's `tokio` feature enabled, calling zbus from a thread
//! that has no Tokio runtime panics with
//!
//!     there is no reactor running, must be called from the context of a Tokio 1.x runtime
//!
//! instead of returning an `Err`. Observed with zbus 5.16.0
//! (src/abstractions/executor.rs:190 -> tokio::runtime::Handle::current()).
//!
//! Why it matters: Slint's winit backend uses zbus on the UI thread (to watch
//! the XDG colour scheme). If anything in the dependency tree turns on zbus's
//! `tokio` feature, that watcher hits this panic and the whole window dies with
//! a blank screen. Entering a Tokio runtime on the calling thread works around
//! it, but ideally zbus would fall back / return an error rather than panic.
//!
//! Run: `cargo run` (no D-Bus session bus is required — it panics during the
//! transport connect, before any connection attempt can fail).
fn main() {
    // No Tokio runtime on this thread; drive the future with a plain executor.
    let result = futures_lite::future::block_on(zbus::Connection::session());
    println!("connection result: {result:?}");
}
