//! Locate the ffmpeg sidecar. `None` is non-fatal — GIF export degrades
//! gracefully to a missing feature.

use std::path::PathBuf;

#[must_use]
pub fn locate() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for candidate in [
            dir.join("ffmpeg.exe"),
            // Dev fallback: target/<profile>/clipo.exe → workspace/vendor/ffmpeg-bin/.
            dir.join("../../vendor/ffmpeg-bin/ffmpeg.exe"),
        ] {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|p| p.join("ffmpeg.exe"))
            .find(|p| p.is_file())
    })
}
