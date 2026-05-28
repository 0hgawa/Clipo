//! Screen capture, encode, and clipboard pipeline.

mod audio;
mod clipboard;
mod clock;
mod cursor;
mod encode;
mod engine;
mod ffmpeg;
mod ocr;
mod video;
mod video_thumb;
mod window_picker;

pub use clipboard::copy_image_to_clipboard;
pub use clock::MasterClock;
pub use encode::{
    CAPTURE_JPEG_QUALITY, decode_bgra_from_bytes, decode_to_bgra, save_jpeg, save_png,
    save_thumbnail_jpeg,
};
pub use engine::{CaptureEngine, CaptureHandle};
pub use ffmpeg::locate as locate_ffmpeg;
pub use ocr::{OcrError, OcrLine, OcrText, extract_text_from_png};
pub use video::{VideoConfig, VideoError, VideoRecorder};
pub use video_thumb::{VideoThumbError, extract_video_thumbnail};
pub use window_picker::{
    WindowInfo, enumerate_capturable_windows, extract_window_icon, focus_window_and_bounds,
};
