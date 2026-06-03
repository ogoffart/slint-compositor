//! Conversion of `wl_shm` buffer contents into tightly-packed RGBA8.
//!
//! This is pure and testable without a GL context or a Wayland connection.
//! `wl_shm`'s ARGB8888/XRGB8888 formats are little-endian 32-bit words
//! (`0xAARRGGBB`), i.e. the bytes in memory are `[B, G, R, A]`. Slint's
//! `Rgba8Pixel` wants `[R, G, B, A]`, so we swap and (for XRGB) force alpha.

/// The shm pixel formats we support for the MVP.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShmFormat {
    /// 32-bit ARGB, alpha honored.
    Argb8888,
    /// 32-bit XRGB, alpha ignored (treated as opaque).
    Xrgb8888,
}

/// Convert `src` (which has `stride` bytes per row) into a tightly-packed
/// `width * height * 4` RGBA8 buffer. Rows beyond `width*4` (stride padding)
/// are skipped.
pub fn convert_to_rgba(
    src: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    format: ShmFormat,
) -> Vec<u8> {
    let mut out = vec![0u8; width * height * 4];
    for y in 0..height {
        let row_start = y * stride;
        let Some(row) = src.get(row_start..row_start + width * 4) else {
            // Truncated/short buffer: leave the remaining rows zeroed.
            break;
        };
        let out_row = &mut out[y * width * 4..(y + 1) * width * 4];
        for x in 0..width {
            let s = &row[x * 4..x * 4 + 4];
            let o = &mut out_row[x * 4..x * 4 + 4];
            o[0] = s[2]; // R
            o[1] = s[1]; // G
            o[2] = s[0]; // B
            o[3] = match format {
                ShmFormat::Argb8888 => s[3],
                ShmFormat::Xrgb8888 => 255,
            };
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swaps_bgra_to_rgba() {
        // One pixel, bytes in memory [B, G, R, A] = [10, 20, 30, 40].
        let src = [10u8, 20, 30, 40];
        let out = convert_to_rgba(&src, 1, 1, 4, ShmFormat::Argb8888);
        assert_eq!(out, vec![30, 20, 10, 40]);
    }

    #[test]
    fn xrgb_forces_opaque_alpha() {
        let src = [10u8, 20, 30, 0];
        let out = convert_to_rgba(&src, 1, 1, 4, ShmFormat::Xrgb8888);
        assert_eq!(out, vec![30, 20, 10, 255]);
    }

    #[test]
    fn honors_stride_padding() {
        // 1x2 image with stride 8 (4 bytes padding per row).
        let src = [
            1, 2, 3, 4, /*pad*/ 0, 0, 0, 0, // row 0 pixel [B,G,R,A]=1,2,3,4
            5, 6, 7, 8, /*pad*/ 0, 0, 0, 0, // row 1
        ];
        let out = convert_to_rgba(&src, 1, 2, 8, ShmFormat::Argb8888);
        assert_eq!(out, vec![3, 2, 1, 4, 7, 6, 5, 8]);
    }
}
