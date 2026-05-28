//! Publish a captured frame on the Windows clipboard as `CF_DIBV5`.
//! V5 carries premultiplied-alpha + sRGB so Office and Chromium honour
//! it; legacy targets read the bottom-up rows correctly too.

#![allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]

use std::time::Duration;

use clipo_core::{CaptureError, CapturedImage};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::Graphics::Gdi::{BI_BITFIELDS, BITMAPV5HEADER, LCS_GM_GRAPHICS};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

// CF_DIBV5 = clipboard format for BITMAPV5HEADER with alpha.
// LCS_sRGB = 'sRGB' four-CC tag in big-endian ASCII.
// Both inlined to avoid pulling Win32_System_Ole + Win32_UI_ColorSystem
// features for a single u32 each.
const CF_DIBV5: u32 = 17;
const LCS_SRGB: u32 = 0x7352_4742;

const OPEN_RETRIES: u32 = 5;
const OPEN_RETRY_DELAY: Duration = Duration::from_millis(20);

#[tracing::instrument(skip(image), fields(w = image.width, h = image.height))]
pub fn copy_image_to_clipboard(image: &CapturedImage) -> Result<(), CaptureError> {
    if image.width == 0 || image.height == 0 {
        return Err(CaptureError::Platform("empty image".into()));
    }

    let header_size = size_of::<BITMAPV5HEADER>();
    let row_bytes = (image.width as usize) * 4;
    let pixel_bytes = row_bytes * (image.height as usize);
    let total = header_size + pixel_bytes;

    // SAFETY: GMEM_MOVEABLE handle ownership is ours until commit
    // succeeds; every error path GlobalFrees below.
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, total) }
        .map_err(|e| CaptureError::Platform(format!("GlobalAlloc: {e}")))?;

    let result = unsafe { fill(handle, image, header_size, row_bytes, pixel_bytes) }
        .and_then(|()| unsafe { commit(handle) });

    if result.is_err() {
        // SAFETY: handle is the live block we just allocated and
        // ownership did not transfer to the clipboard.
        unsafe {
            let _ = GlobalFree(Some(handle));
        }
    }
    result
}

/// # Safety
/// `handle` must be a live `GMEM_MOVEABLE` block ≥ `header_size + pixel_bytes`.
unsafe fn fill(
    handle: HGLOBAL,
    image: &CapturedImage,
    header_size: usize,
    row_bytes: usize,
    pixel_bytes: usize,
) -> Result<(), CaptureError> {
    // SAFETY: caller guarantees handle is allocated and unlocked.
    let ptr = unsafe { GlobalLock(handle) };
    if ptr.is_null() {
        return Err(CaptureError::Platform("GlobalLock returned null".into()));
    }

    let header = BITMAPV5HEADER {
        bV5Size: header_size as u32,
        bV5Width: image.width as i32,
        // Positive height = bottom-up rows; we flip during copy so
        // legacy targets (Word, MSPaint) paste correctly.
        bV5Height: image.height as i32,
        bV5Planes: 1,
        bV5BitCount: 32,
        bV5Compression: BI_BITFIELDS,
        bV5SizeImage: pixel_bytes as u32,
        bV5RedMask: 0x00FF_0000,
        bV5GreenMask: 0x0000_FF00,
        bV5BlueMask: 0x0000_00FF,
        bV5AlphaMask: 0xFF00_0000,
        bV5CSType: LCS_SRGB,
        bV5Intent: LCS_GM_GRAPHICS as u32,
        ..Default::default()
    };

    // SAFETY: ptr points to ≥ header_size bytes.
    unsafe {
        std::ptr::write(ptr.cast::<BITMAPV5HEADER>(), header);

        let dst_pixels = ptr.cast::<u8>().add(header_size);
        let src = image.bgra.as_ptr();
        let height = image.height as usize;
        for y in 0..height {
            std::ptr::copy_nonoverlapping(
                src.add(y * row_bytes),
                dst_pixels.add((height - 1 - y) * row_bytes),
                row_bytes,
            );
        }

        let _ = GlobalUnlock(handle);
    }
    Ok(())
}

/// # Safety
/// `handle` must hold a valid `CF_DIBV5` payload. Ownership is consumed
/// by the clipboard on Ok.
unsafe fn commit(handle: HGLOBAL) -> Result<(), CaptureError> {
    for _ in 0..OPEN_RETRIES {
        // SAFETY: open/close pair is matched on every branch below.
        if unsafe { OpenClipboard(None) }.is_ok() {
            let result = unsafe {
                EmptyClipboard()
                    .map_err(|e| CaptureError::Platform(format!("EmptyClipboard: {e}")))
                    .and_then(|()| {
                        SetClipboardData(CF_DIBV5, Some(HANDLE(handle.0)))
                            .map(|_| ())
                            .map_err(|e| {
                                CaptureError::Platform(format!("SetClipboardData: {e}"))
                            })
                    })
            };
            // SAFETY: pair the open.
            let _ = unsafe { CloseClipboard() };
            return result;
        }
        std::thread::sleep(OPEN_RETRY_DELAY);
    }
    Err(CaptureError::Platform(
        "OpenClipboard busy after retries".into(),
    ))
}
