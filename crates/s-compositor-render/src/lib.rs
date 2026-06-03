//! GL buffer-import bridge.
//!
//! All `unsafe` GL/EGL FFI for turning client `wl_surface` buffers into GL
//! textures lives behind the [`BufferImporter`] trait so the shm path (MVP) and
//! the dmabuf path (phase 2) are swappable. The resulting texture ids are handed
//! to Slint via `slint::Image::from_borrowed_gl_texture` in the binary crate's
//! rendering-notifier callback, where Slint's GL context is current.
//!
//! This crate currently defines the seam; the concrete shm/dmabuf importers land
//! with milestone M2 (see the project plan).

mod shm;
mod texture_cache;

pub use shm::{convert_to_rgba, ShmFormat};
pub use texture_cache::{ImportedTexture, TextureCache};

/// Pixel dimensions of an imported buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

/// A loader for raw GL/EGL function pointers, as provided by Slint's
/// `GraphicsAPI::NativeOpenGL { get_proc_address }` in the rendering notifier.
pub trait GlProcLoader {
    /// Look up the address of a GL/EGL function by name.
    fn get_proc_address(&self, name: &str) -> *const core::ffi::c_void;
}

/// Imports client buffers into GL textures usable by Slint.
///
/// Implementations must only be called while the Slint GL context is current
/// (i.e. from within the rendering-notifier callback).
pub trait BufferImporter {
    /// The opaque texture handle produced by this importer.
    type Texture;

    /// Import (or update) the texture for the given surface from its currently
    /// committed buffer. Returns `None` if there is nothing to display yet.
    fn import(&mut self, surface_id: u64) -> Option<Self::Texture>;

    /// Release any GL resources held for a surface that has been unmapped.
    fn release(&mut self, surface_id: u64);
}
