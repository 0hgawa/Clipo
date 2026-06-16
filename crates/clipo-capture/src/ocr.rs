//! On-demand OCR via `Windows.Media.Ocr`. Engine is lazily built on
//! first call and cached. Caller must drive this from a worker — the
//! `WinRT` async `get()` parks the calling thread.

use std::sync::OnceLock;

use serde::Serialize;
use thiserror::Error;
use windows::Graphics::Imaging::{BitmapDecoder, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrLine {
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrText {
    pub full_text: String,
    pub lines: Vec<OcrLine>,
}

#[derive(Debug, Error)]
pub enum OcrError {
    #[error("ocr engine init: {0}")]
    EngineInit(String),
    #[error("decode image: {0}")]
    Decode(String),
    #[error("recognize: {0}")]
    Recognize(String),
}

static ENGINE: OnceLock<OcrEngine> = OnceLock::new();

fn engine() -> Result<&'static OcrEngine, OcrError> {
    if let Some(e) = ENGINE.get() {
        return Ok(e);
    }
    let new_engine = OcrEngine::TryCreateFromUserProfileLanguages()
        .map_err(|e| OcrError::EngineInit(format!("TryCreateFromUserProfileLanguages: {e}")))?;
    Ok(ENGINE.get_or_init(|| new_engine))
}

/// Run OCR on an image byte buffer (PNG/JPEG/WebP). Empty `OcrText`
/// means the recognizer succeeded but found no characters — distinct
/// from a hard failure.
pub fn extract_text_from_png(png: &[u8]) -> Result<OcrText, OcrError> {
    let bitmap = decode_to_software_bitmap(png)?;
    let result = engine()?
        .RecognizeAsync(&bitmap)
        .map_err(|e| OcrError::Recognize(format!("start: {e}")))?
        .get()
        .map_err(|e| OcrError::Recognize(format!("await: {e}")))?;

    let lines_iv = result
        .Lines()
        .map_err(|e| OcrError::Recognize(format!("Lines: {e}")))?;
    let count = lines_iv
        .Size()
        .map_err(|e| OcrError::Recognize(format!("Size: {e}")))?;

    let mut lines = Vec::with_capacity(count as usize);
    let mut full_text = String::new();
    for i in 0..count {
        let s = lines_iv
            .GetAt(i)
            .and_then(|l| l.Text())
            .map_err(|e| OcrError::Recognize(format!("line {i}: {e}")))?
            .to_string();
        if s.trim().is_empty() {
            continue;
        }
        if !full_text.is_empty() {
            full_text.push('\n');
        }
        full_text.push_str(&s);
        lines.push(OcrLine { text: s });
    }

    Ok(OcrText { full_text, lines })
}

fn decode_to_software_bitmap(png: &[u8]) -> Result<SoftwareBitmap, OcrError> {
    let stream = InMemoryRandomAccessStream::new()
        .map_err(|e| OcrError::Decode(format!("stream new: {e}")))?;

    // Tight scope so the writer doesn't outlive the seek — BitmapDecoder
    // needs sole ownership of the stream.
    {
        let writer = DataWriter::CreateDataWriter(&stream)
            .map_err(|e| OcrError::Decode(format!("writer: {e}")))?;
        writer
            .WriteBytes(png)
            .map_err(|e| OcrError::Decode(format!("write: {e}")))?;
        writer
            .StoreAsync()
            .map_err(|e| OcrError::Decode(format!("store: {e}")))?
            .get()
            .map_err(|e| OcrError::Decode(format!("store await: {e}")))?;
        let _ = writer
            .DetachStream()
            .map_err(|e| OcrError::Decode(format!("detach: {e}")))?;
    }

    stream
        .Seek(0)
        .map_err(|e| OcrError::Decode(format!("seek: {e}")))?;

    BitmapDecoder::CreateAsync(&stream)
        .map_err(|e| OcrError::Decode(format!("decoder: {e}")))?
        .get()
        .map_err(|e| OcrError::Decode(format!("decoder await: {e}")))?
        .GetSoftwareBitmapAsync()
        .map_err(|e| OcrError::Decode(format!("bitmap: {e}")))?
        .get()
        .map_err(|e| OcrError::Decode(format!("bitmap await: {e}")))
}
