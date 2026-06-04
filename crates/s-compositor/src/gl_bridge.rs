//! Shares GL textures between the compositor and Slint's (Skia) renderer.
//!
//! Client frames are uploaded into GL textures that *we* own, in Slint's own GL
//! context, and handed to Slint as borrowed textures via
//! [`slint::BorrowedOpenGLTextureBuilder`]. Slint then samples our texture
//! directly instead of managing its own copy. All GL calls must happen while the
//! context is current, i.e. only from inside the rendering-notifier callback.

use std::collections::HashMap;
use std::ffi::{c_void, CStr};
use std::os::fd::{AsRawFd, OwnedFd};
use std::rc::Rc;

use glow::HasContext;

/// A client frame waiting to be uploaded to its texture.
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// A client GPU (dmabuf) frame waiting to be imported as an EGLImage texture.
/// Single-plane only for now (multi-plane / YUV is a follow-up).
pub struct DmabufFrame {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub modifier: u64,
    pub fd: OwnedFd,
    pub offset: u32,
    pub stride: u32,
}

/// EGL constants and entry points for `EGL_EXT_image_dma_buf_import`, resolved
/// from Slint's GL loader. Best-effort: needs a real GPU to actually work.
mod egl {
    use std::ffi::c_void;
    pub type Display = *mut c_void;
    pub type Image = *mut c_void;
    pub type Context = *mut c_void;
    pub type ClientBuffer = *mut c_void;

    pub const LINUX_DMA_BUF_EXT: u32 = 0x3270;
    pub const WIDTH: i32 = 0x3057;
    pub const HEIGHT: i32 = 0x3056;
    pub const LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
    pub const PLANE0_FD_EXT: i32 = 0x3272;
    pub const PLANE0_OFFSET_EXT: i32 = 0x3273;
    pub const PLANE0_PITCH_EXT: i32 = 0x3274;
    pub const PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
    pub const PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;
    pub const NONE: i32 = 0x3038;
    pub const NO_CONTEXT: Context = std::ptr::null_mut();
    pub const NO_IMAGE: Image = std::ptr::null_mut();
    /// DRM_FORMAT_MOD_INVALID — omit explicit modifier attributes when set.
    pub const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

    pub type GetCurrentDisplay = unsafe extern "C" fn() -> Display;
    pub type CreateImage =
        unsafe extern "C" fn(Display, Context, u32, ClientBuffer, *const i32) -> Image;
    pub type DestroyImage = unsafe extern "C" fn(Display, Image) -> u32;
    pub type TargetTexture2DOES = unsafe extern "C" fn(u32, Image);
}

/// The resolved EGL/GLES extension entry points needed for dmabuf import.
struct EglFns {
    get_current_display: egl::GetCurrentDisplay,
    create_image: egl::CreateImage,
    destroy_image: egl::DestroyImage,
    target_texture: egl::TargetTexture2DOES,
}

/// Owns the per-window GL textures shared with Slint.
#[derive(Default)]
pub struct GlBridge {
    gl: Option<Rc<glow::Context>>,
    textures: HashMap<u64, (glow::NativeTexture, u32, u32)>,
    /// dmabuf import entry points (resolved at init; `None` if unavailable).
    egl: Option<EglFns>,
    /// Live EGLImages backing dmabuf textures, destroyed on re-import/removal.
    images: HashMap<u64, egl::Image>,
}

impl GlBridge {
    /// Bind to Slint's GL context (idempotent). Call from `RenderingSetup`.
    pub fn init(&mut self, get_proc_address: &dyn Fn(&CStr) -> *const c_void) {
        if self.gl.is_none() {
            // SAFETY: the loader returns valid GL function pointers for the
            // context that is current during the rendering notifier.
            let ctx = unsafe { glow::Context::from_loader_function_cstr(get_proc_address) };
            self.gl = Some(Rc::new(ctx));

            // Resolve the dmabuf-import extension entry points (optional).
            let gcd = get_proc_address(c"eglGetCurrentDisplay");
            let ci = get_proc_address(c"eglCreateImageKHR");
            let di = get_proc_address(c"eglDestroyImageKHR");
            let tt = get_proc_address(c"glEGLImageTargetTexture2DOES");
            if !gcd.is_null() && !ci.is_null() && !di.is_null() && !tt.is_null() {
                // SAFETY: the loader returned non-null pointers with the expected
                // signatures for these well-known EGL/GLES extension functions.
                self.egl = Some(unsafe {
                    EglFns {
                        get_current_display: std::mem::transmute::<_, egl::GetCurrentDisplay>(gcd),
                        create_image: std::mem::transmute::<_, egl::CreateImage>(ci),
                        destroy_image: std::mem::transmute::<_, egl::DestroyImage>(di),
                        target_texture: std::mem::transmute::<_, egl::TargetTexture2DOES>(tt),
                    }
                });
            } else {
                log::info!(
                    "dmabuf: EGL image-import functions unavailable; GPU buffers unsupported"
                );
            }
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

    /// Import a dmabuf `frame` as an EGLImage-backed GL texture and return a
    /// borrowed-texture image referencing it. Best-effort: returns `None` on any
    /// failure (missing EGL functions, no GPU, unsupported format) so the window
    /// simply keeps its previous frame rather than crashing. Must run with the GL
    /// context current (i.e. from the rendering notifier).
    pub fn import_dmabuf(&mut self, id: u64, frame: &DmabufFrame) -> Option<slint::Image> {
        let gl = self.gl.clone()?;
        let egl = self.egl.as_ref()?;
        // SAFETY: GL context is current; we hold valid extension entry points and
        // a valid plane fd for the lifetime of this call. EGL dups the fd, so the
        // caller may drop it afterwards.
        unsafe {
            let display = (egl.get_current_display)();
            if display.is_null() {
                return None;
            }
            let mut attribs = vec![
                egl::WIDTH,
                frame.width as i32,
                egl::HEIGHT,
                frame.height as i32,
                egl::LINUX_DRM_FOURCC_EXT,
                frame.fourcc as i32,
                egl::PLANE0_FD_EXT,
                frame.fd.as_raw_fd(),
                egl::PLANE0_OFFSET_EXT,
                frame.offset as i32,
                egl::PLANE0_PITCH_EXT,
                frame.stride as i32,
            ];
            if frame.modifier != egl::MOD_INVALID {
                attribs.push(egl::PLANE0_MODIFIER_LO_EXT);
                attribs.push((frame.modifier & 0xffff_ffff) as i32);
                attribs.push(egl::PLANE0_MODIFIER_HI_EXT);
                attribs.push((frame.modifier >> 32) as i32);
            }
            attribs.push(egl::NONE);

            let image = (egl.create_image)(
                display,
                egl::NO_CONTEXT,
                egl::LINUX_DMA_BUF_EXT,
                std::ptr::null_mut(),
                attribs.as_ptr(),
            );
            if image == egl::NO_IMAGE {
                log::warn!("dmabuf: eglCreateImageKHR failed for window {id}");
                return None;
            }
            // Replace any previous EGLImage for this window.
            if let Some(old) = self.images.insert(id, image) {
                (egl.destroy_image)(display, old);
            }

            let texture = match self.textures.get(&id) {
                Some(&(texture, _, _)) => texture,
                None => gl.create_texture().ok()?,
            };
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
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
            (egl.target_texture)(glow::TEXTURE_2D, image);
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

    /// Delete the texture (and any dmabuf EGLImage) for a removed window. Must
    /// run with the GL context current (i.e. from the rendering notifier).
    pub fn remove(&mut self, id: u64) {
        if let (Some(gl), Some((texture, _, _))) = (self.gl.as_ref(), self.textures.remove(&id)) {
            // SAFETY: GL context current; texture is no longer referenced.
            unsafe { gl.delete_texture(texture) };
        }
        if let (Some(egl), Some(image)) = (self.egl.as_ref(), self.images.remove(&id)) {
            // SAFETY: GL context current; the EGLImage is no longer referenced.
            unsafe {
                let display = (egl.get_current_display)();
                if !display.is_null() {
                    (egl.destroy_image)(display, image);
                }
            }
        }
    }
}
