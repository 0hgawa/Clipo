//! PNG / JPEG encoding for captured frames. Synchronous on purpose —
//! the daemon dispatches encodes from `spawn_blocking`.
//!
//! PNG path uses `CompressionType::Fast` (~150 ms at 4K). Screenshots
//! get re-saved on every annotation edit — trading max compression for
//! speed is the right call.

use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use clipo_core::{CaptureError, CapturedImage};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::{CompressionType, FilterType, PngEncoder};
use image::{ExtendedColorType, ImageEncoder};

// 480×270 = exact intrinsic resolution of the History grid card at
// typical zoom. Bigger = wasted RAM/disk; smaller starts to alias
// text-heavy screenshots.
const THUMBNAIL_MAX_W: u32 = 480;
const THUMBNAIL_MAX_H: u32 = 270;
// libjpeg-turbo "imperceptible loss" knee for natural images at thumb
// size — files settle around 10-20 KB.
const SIDECAR_JPEG_QUALITY: u8 = 82;

/// Full-resolution JPEG quality. 90 = crisp text-edge knee while
/// cutting several × off a PNG for photo-like screenshots.
pub const CAPTURE_JPEG_QUALITY: u8 = 90;

#[tracing::instrument]
pub fn decode_to_bgra(path: &Path) -> Result<CapturedImage, CaptureError> {
    let reader = image::ImageReader::open(path)
        .map_err(|e| CaptureError::Io(format!("open {}: {e}", path.display())))?;
    finish_decode(reader)
}

#[tracing::instrument(skip(bytes), fields(len = bytes.len()))]
pub fn decode_bgra_from_bytes(bytes: &[u8]) -> Result<CapturedImage, CaptureError> {
    finish_decode(image::ImageReader::new(std::io::Cursor::new(bytes)))
}

#[tracing::instrument(skip(image), fields(w = image.width, h = image.height))]
pub fn save_png(image: &CapturedImage, path: &Path) -> Result<(), CaptureError> {
    let writer = create_writer(path)?;
    let rgba = bgra_to_rgba(&image.bgra);
    PngEncoder::new_with_quality(writer, CompressionType::Fast, FilterType::Adaptive)
        .write_image(&rgba, image.width, image.height, ExtendedColorType::Rgba8)
        .map_err(|e| CaptureError::Encode(format!("png encode: {e}")))
}

#[tracing::instrument(skip(image), fields(w = image.width, h = image.height))]
pub fn save_jpeg(image: &CapturedImage, path: &Path, quality: u8) -> Result<(), CaptureError> {
    let mut writer = create_writer(path)?;
    let rgb = bgra_to_rgb(&image.bgra);
    JpegEncoder::new_with_quality(&mut writer, quality)
        .encode(&rgb, image.width, image.height, ExtendedColorType::Rgb8)
        .map_err(|e| CaptureError::Encode(format!("jpeg encode: {e}")))
}

#[tracing::instrument(skip(image), fields(w = image.width, h = image.height))]
pub fn save_thumbnail_jpeg(image: &CapturedImage, path: &Path) -> Result<(), CaptureError> {
    let bgra_view = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(
        image.width,
        image.height,
        image.bgra.as_slice(),
    )
    .ok_or_else(|| CaptureError::Encode("bgra buffer size mismatch".into()))?;
    let (tw, th) = fit_within(image.width, image.height, THUMBNAIL_MAX_W, THUMBNAIL_MAX_H);
    let thumb = image::imageops::thumbnail(&bgra_view, tw, th);
    let rgb = bgra_to_rgb(&thumb);

    let mut writer = create_writer(path)?;
    JpegEncoder::new_with_quality(&mut writer, SIDECAR_JPEG_QUALITY)
        .encode(&rgb, tw, th, ExtendedColorType::Rgb8)
        .map_err(|e| CaptureError::Encode(format!("jpeg encode: {e}")))
}

fn finish_decode<R: std::io::BufRead + std::io::Seek>(
    reader: image::ImageReader<R>,
) -> Result<CapturedImage, CaptureError> {
    let dyn_img = reader
        .with_guessed_format()
        .map_err(|e| CaptureError::Io(format!("guess format: {e}")))?
        .decode()
        .map_err(|e| CaptureError::Encode(format!("decode: {e}")))?;
    let rgba = dyn_img.into_rgba8();
    let (width, height) = (rgba.width(), rgba.height());
    let mut bgra = rgba.into_raw();
    swap_br_in_place(&mut bgra);
    Ok(CapturedImage {
        width,
        height,
        bgra,
    })
}

fn create_writer(path: &Path) -> Result<BufWriter<File>, CaptureError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CaptureError::Io(format!("mkdir {}: {e}", parent.display())))?;
    }
    File::create(path)
        .map(BufWriter::new)
        .map_err(|e| CaptureError::Io(format!("create {}: {e}", path.display())))
}

fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut out = bgra.to_vec();
    swap_br_in_place(&mut out);
    out
}

fn bgra_to_rgb(bgra: &[u8]) -> Vec<u8> {
    let mut rgb = vec![0u8; bgra.len() / 4 * 3];
    for (src, dst) in bgra.chunks_exact(4).zip(rgb.chunks_exact_mut(3)) {
        dst[0] = src[2];
        dst[1] = src[1];
        dst[2] = src[0];
    }
    rgb
}

fn swap_br_in_place(buf: &mut [u8]) {
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
}

#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn fit_within(src_w: u32, src_h: u32, max_w: u32, max_h: u32) -> (u32, u32) {
    if src_w == 0 || src_h == 0 {
        return (1, 1);
    }
    let scale = (f64::from(max_w) / f64::from(src_w))
        .min(f64::from(max_h) / f64::from(src_h))
        .min(1.0);
    let w = (f64::from(src_w) * scale).round().max(1.0) as u32;
    let h = (f64::from(src_h) * scale).round().max(1.0) as u32;
    (w, h)
}
