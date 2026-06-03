//! Shares GL textures between the compositor and Slint's (Skia) renderer.
//!
//! Client frames are uploaded into GL textures that *we* own, in Slint's own GL
//! context, and handed to Slint as borrowed textures via
//! [`slint::BorrowedOpenGLTextureBuilder`]. Slint then samples our texture
//! directly instead of managing its own copy. All GL calls must happen while the
//! context is current, i.e. only from inside the rendering-notifier callback.

use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::rc::Rc;

use glow::HasContext;

/// A client frame waiting to be uploaded to its texture.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Owns the per-window GL textures shared with Slint.
#[derive(Default)]
pub struct GlBridge {
    gl: Option<Rc<glow::Context>>,
    textures: HashMap<u64, (glow::NativeTexture, u32, u32)>,
}

impl GlBridge {
    /// Bind to Slint's GL context (idempotent). Call from `RenderingSetup`.
    pub fn init(&mut self, get_proc_address: &dyn Fn(&CStr) -> *const c_void) {
        if self.gl.is_none() {
            // SAFETY: the loader returns valid GL function pointers for the
            // context that is current during the rendering notifier.
            let ctx = unsafe { glow::Context::from_loader_function_cstr(get_proc_address) };
            self.gl = Some(Rc::new(ctx));
        }
    }

    pub fn ready(&self) -> bool {
        self.gl.is_some()
    }

    /// Upload `frame` into the texture for `id` (creating/reallocating it as
    /// needed) and return a borrowed-texture image referencing it.
    pub fn upload(&mut self, id: u64, frame: &Frame) -> Option<slint::Image> {
        let gl = self.gl.clone()?;
        // SAFETY: GL context is current (we're in the rendering notifier); the
        // texture id we pass to Slint stays alive in `self.textures` until the
        // window is removed.
        unsafe {
            let texture = match self.textures.get(&id) {
                Some(&(texture, _, _)) => texture,
                None => gl.create_texture().ok()?,
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::NEAREST as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::NEAREST as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA as i32,
                frame.width as i32,
                frame.height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&frame.pixels)),
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
            self.textures
                .insert(id, (texture, frame.width, frame.height));

            let image = slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                texture.0,
                [frame.width, frame.height].into(),
            )
            .build();
            Some(image)
        }
    }

    /// Delete the texture for a removed window. Must run with the GL context
    /// current (i.e. from the rendering notifier).
    pub fn remove(&mut self, id: u64) {
        if let (Some(gl), Some((texture, _, _))) = (self.gl.as_ref(), self.textures.remove(&id)) {
            // SAFETY: GL context current; texture is no longer referenced.
            unsafe { gl.delete_texture(texture) };
        }
    }
}
