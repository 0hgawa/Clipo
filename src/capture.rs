//! Screen-capture helper: BGRA capture → Slint RGBA image conversion.
use clipo_core::CapturedImage;

/// BGRA capture → Slint RGBA image (channel swap).
pub fn to_slint_image(img: &CapturedImage) -> slint::Image {
    let mut buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(img.width, img.height);
    let bytes = buf.make_mut_bytes();
    // BGRA → RGBA one 32-bit word per pixel (single read + write + swizzle of
    // the B/R bytes) instead of four byte ops — fewer memory accesses on this
    // full-frame loop. Windows is LE.
    for (dst, src) in bytes.chunks_exact_mut(4).zip(img.bgra.chunks_exact(4)) {
        let p = u32::from_le_bytes([src[0], src[1], src[2], src[3]]);
        let rgba = (p & 0xFF00_FF00) | ((p & 0x0000_00FF) << 16) | ((p >> 16) & 0x0000_00FF);
        dst.copy_from_slice(&rgba.to_le_bytes());
    }
    slint::Image::from_rgba8(buf)
}
