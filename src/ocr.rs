//! Run OCR on a saved image off-thread, then show the result window.
use crate::{show_centered, OcrWindow};
use std::path::PathBuf;

/// Run OCR off the UI thread (WinRT async would deadlock it), then open the
/// result window. Shared by the actions panel and the viewer.
pub fn run_ocr_for(path: PathBuf, ocr: slint::Weak<OcrWindow>) {
    std::thread::spawn(move || {
        let result = std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| {
                clipo_capture::extract_text_from_png(&bytes).map_err(|e| e.to_string())
            });
        let _ = slint::invoke_from_event_loop(move || {
            let Some(w) = ocr.upgrade() else { return };
            match result {
                Ok(text) => {
                    let words = text.full_text.split_whitespace().count();
                    let chars = text.full_text.chars().count();
                    w.set_text(text.full_text.into());
                    w.set_word_count(words as i32);
                    w.set_char_count(chars as i32);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "ocr");
                    w.set_text("".into()); // empty → the window shows its empty state
                    w.set_char_count(0); // 0 → the stats label hides
                }
            }
            show_centered(&w, 520.0, 420.0);
        });
    });
}
