//! Screen recording: bitrate heuristic + starting a capture session.
use crate::capture_path;
use crate::settings::load_settings;
use clipo_capture::{VideoConfig, VideoRecorder};
use clipo_core::Rect;
use std::path::PathBuf;

/// Convert an MP4 to a sibling `.gif` via the ffmpeg sidecar — 15 fps, native
/// resolution capped at 1080 px, one global 128-colour palette + bayer dither.
/// The single global palette is what keeps GIFs small (it compresses far better
/// than a per-frame palette), so the resolution can be generous without the file
/// ballooning. Returns the .gif path; `Err` if ffmpeg isn't found (GIF is an
/// optional, ffmpeg-gated feature). Blocking.
pub fn export_gif(src: &std::path::Path) -> Result<PathBuf, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000; // no console flash
    let ffmpeg = clipo_capture::locate_ffmpeg().ok_or("ffmpeg not found")?;
    let dst = src.with_extension("gif");
    let filter = "fps=15,scale=w='min(1080,iw)':h=-1:flags=lanczos,split[s0][s1];\
                  [s0]palettegen=max_colors=128[p];[s1][p]paletteuse=dither=bayer";
    let status = std::process::Command::new(&ffmpeg)
        .arg("-y")
        .arg("-i")
        .arg(src)
        .arg("-vf")
        .arg(filter)
        .arg(&dst)
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|e| format!("spawn ffmpeg: {e}"))?;
    if !status.success() {
        return Err(format!("ffmpeg exited with {status}"));
    }
    Ok(dst)
}

/// True when the ffmpeg sidecar is available (so the GIF action can show).
pub fn gif_available() -> bool {
    clipo_capture::locate_ffmpeg().is_some()
}

/// Recording bitrate (bps): w·h·fps_shoulder·0.1, clamped to [2, 80] Mbps.
pub fn recording_bitrate(w: u32, h: u32, fps: u32) -> u32 {
    let shoulder = 30 + fps.saturating_sub(30) / 2;
    let bps = u64::from(w) * u64::from(h) * u64::from(shoulder) / 10;
    bps.clamp(2_000_000, 80_000_000) as u32
}

/// Start recording `rect` (system audio on, mic off) → (recorder, path, rect).
/// Shared by the record trigger and Restart.
pub fn begin_recording(rect: Rect) -> Option<(VideoRecorder, PathBuf, Rect)> {
    let s = load_settings();
    let fps = s.fps();
    let path = capture_path("mp4");
    let cfg = VideoConfig {
        rect,
        output: path.clone(),
        fps,
        bitrate_bps: recording_bitrate(rect.width, rect.height, fps),
        capture_audio: s.audio_enabled(),
        capture_mic: s.capture_mic,
        show_cursor: s.record_cursor_enabled(),
        show_clicks: s.highlight_cursor,
        // Hardware encoder unless a past crash (or the user) forced software.
        crash_sentinel: (!s.software_encoder).then(crate::settings::encoder_sentinel_path),
    };
    match VideoRecorder::start(cfg) {
        Ok(rec) => Some((rec, path, rect)),
        Err(e) => {
            tracing::error!(error = %e, "recording start");
            None
        }
    }
}
