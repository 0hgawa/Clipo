//! Extract a single frame from an MP4 as a JPEG thumbnail. Used by the
//! History grid for recordings that pre-date the sidecar-thumb-on-save
//! flow. Driven from `spawn_blocking` — IMF source reader is sync.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::path::Path;

use clipo_core::{CaptureError, CapturedImage};
use thiserror::Error;
use windows::Win32::Media::MediaFoundation::{
    IMFSourceReader, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    MF_SOURCE_READERF_ENDOFSTREAM, MF_VERSION, MFCreateAttributes, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFMediaType_Video, MFShutdown, MFStartup, MFVideoFormat_RGB32,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::HSTRING;

use crate::encode::save_thumbnail_jpeg;

// Cap to stop spinning on malformed MP4s. First/second sample yields in
// the steady-state path.
const MAX_READ_ATTEMPTS: u32 = 8;
const FIRST_STREAM: u32 = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

#[derive(Debug, Error)]
pub enum VideoThumbError {
    #[error("media foundation: {0}")]
    Mf(String),
    #[error("no decodable video frames")]
    NoFrames,
    #[error("save thumbnail: {0}")]
    Save(#[from] CaptureError),
}

impl From<windows::core::Error> for VideoThumbError {
    fn from(e: windows::core::Error) -> Self {
        Self::Mf(e.to_string())
    }
}

#[tracing::instrument(skip_all, fields(input = %mp4_path.display(), out = %output_jpeg.display()))]
pub fn extract_video_thumbnail(mp4_path: &Path, output_jpeg: &Path) -> Result<(), VideoThumbError> {
    let _com = ComMta::new()?;
    let _mf = MfRuntime::new()?;

    let reader = open_source_reader(mp4_path)?;
    let (width, height, stride) = configure_bgra_output(&reader)?;
    let frame = read_first_frame(&reader, width, height, stride)?;
    save_thumbnail_jpeg(&frame, output_jpeg)?;
    Ok(())
}

struct ComMta;
impl ComMta {
    fn new() -> Result<Self, VideoThumbError> {
        // SAFETY: S_OK / S_FALSE both ok; refcount pairs with Drop.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() {
            return Err(VideoThumbError::Mf(format!("CoInitializeEx: {hr:?}")));
        }
        Ok(Self)
    }
}
impl Drop for ComMta {
    fn drop(&mut self) {
        // SAFETY: pairs the CoInitializeEx on this thread.
        unsafe { CoUninitialize() };
    }
}

struct MfRuntime;
impl MfRuntime {
    fn new() -> Result<Self, VideoThumbError> {
        // SAFETY: globally refcounted; safe to nest with an active recording.
        unsafe { MFStartup(MF_VERSION, 0) }?;
        Ok(Self)
    }
}
impl Drop for MfRuntime {
    fn drop(&mut self) {
        // SAFETY: pairs the MFStartup.
        let _ = unsafe { MFShutdown() };
    }
}

fn open_source_reader(mp4_path: &Path) -> Result<IMFSourceReader, VideoThumbError> {
    // ENABLE_VIDEO_PROCESSING = 1 lets MF insert the colour-space +
    // format converter automatically — without it,
    // SetCurrentMediaType(RGB32) returns MF_E_INVALIDMEDIATYPE for
    // hardware-decoded NV12 output (what H.264 yields by default).
    // SAFETY: Attribute factory + SetUINT32 are documented out-params.
    let attrs = unsafe {
        let mut out = None;
        MFCreateAttributes(&raw mut out, 1)?;
        let attrs = out.ok_or_else(|| VideoThumbError::Mf("MFCreateAttributes: null".into()))?;
        attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)?;
        attrs
    };

    let url = HSTRING::from(mp4_path);
    // SAFETY: url outlives the call.
    let reader = unsafe { MFCreateSourceReaderFromURL(&url, Some(&attrs)) }?;
    Ok(reader)
}

/// Negotiate RGB32 (= BGRA in little-endian memory) output and return
/// resolved width/height/stride. Reads stride from the negotiated type
/// because decoders pad rows for SIMD alignment.
fn configure_bgra_output(reader: &IMFSourceReader) -> Result<(u32, u32, u32), VideoThumbError> {
    // SAFETY: factory + attribute writes on a fresh COM object.
    let media_type = unsafe { MFCreateMediaType() }?;
    unsafe {
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)?;
        reader.SetCurrentMediaType(FIRST_STREAM, None, &media_type)?;
    }
    // SAFETY: read the resolved type back to learn the actual dimensions.
    let current = unsafe { reader.GetCurrentMediaType(FIRST_STREAM) }?;
    let packed = unsafe { current.GetUINT64(&MF_MT_FRAME_SIZE) }?;
    let width = (packed >> 32) as u32;
    let height = (packed & 0xFFFF_FFFF) as u32;
    let stride = unsafe { current.GetUINT32(&MF_MT_DEFAULT_STRIDE) }.unwrap_or(width * 4);
    Ok((width, height, stride))
}

fn read_first_frame(
    reader: &IMFSourceReader,
    width: u32,
    height: u32,
    stride: u32,
) -> Result<CapturedImage, VideoThumbError> {
    let row_bytes = (width as usize) * 4;
    let stride = stride as usize;
    let needed = stride * (height as usize);
    let eof_flag = MF_SOURCE_READERF_ENDOFSTREAM.0 as u32;

    for _ in 0..MAX_READ_ATTEMPTS {
        let mut flags = 0u32;
        let mut sample = None;
        // SAFETY: standard ReadSample out-params.
        unsafe {
            reader.ReadSample(
                FIRST_STREAM,
                0,
                None,
                Some(&raw mut flags),
                None,
                Some(&raw mut sample),
            )?;
        }
        if flags & eof_flag != 0 {
            return Err(VideoThumbError::NoFrames);
        }
        let Some(sample) = sample else {
            // MF may yield a null sample on a non-fatal stream event
            // (format change). Retry.
            continue;
        };

        // SAFETY: contiguous buffer + Lock/Unlock paired.
        let buffer = unsafe { sample.ConvertToContiguousBuffer() }?;
        let mut data: *mut u8 = std::ptr::null_mut();
        let mut len: u32 = 0;
        unsafe {
            buffer.Lock(&raw mut data, None, Some(&raw mut len))?;
        }

        // Row-by-row copy honouring stride. Without it, widths whose
        // `width*4` isn't a multiple of the decoder's SIMD alignment
        // shear the thumbnail.
        let copied = (!data.is_null() && (len as usize) >= needed).then(|| {
            // SAFETY: Lock contract: data valid for `len` bytes ≥ needed.
            let src = unsafe { std::slice::from_raw_parts(data, needed) };
            let mut bgra = Vec::with_capacity(row_bytes * (height as usize));
            for y in 0..(height as usize) {
                bgra.extend_from_slice(&src[y * stride..y * stride + row_bytes]);
            }
            bgra
        });
        // SAFETY: Unlock regardless of copy outcome.
        let _ = unsafe { buffer.Unlock() };
        if let Some(bgra) = copied {
            return Ok(CapturedImage {
                width,
                height,
                bgra,
            });
        }
    }
    Err(VideoThumbError::NoFrames)
}
