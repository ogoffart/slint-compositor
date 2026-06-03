//! Plain-Rust data models for the s-compositor shell.
//!
//! This crate has no Slint or GL dependencies on purpose: the data types and
//! logic here (windows, workspaces, clock formatting) are pure and unit-testable
//! without a display. The binary crate binds these to the Slint-generated models.

mod clock;
mod model;

pub use clock::format_clock;
pub use model::{WindowId, WindowInfo, WorkspaceId};
