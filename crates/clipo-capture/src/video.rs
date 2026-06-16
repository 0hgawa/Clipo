//! Screen recording — DXGI Desktop Duplication + Media Foundation
//! `SinkWriter`. Two threads:
//!
//! - Capture: owns `DxgiDuplicationApi`, GPU-crops, pushes BGRA frames.
//! - Encoder: hard-paced at `1/fps`, drains keeping only the latest
//!   frame, calls `WriteSample` with a wall-clock PTS. Idle ticks emit
//!   nothing (VFR timeline) — the only configuration that avoids
//!   hardware-encoder motion-estimation ghosting between cached frames.
//!
//! ## DXGI Desktop Duplication, not WGC
//! WGC draws a yellow capture border on Win10 < 1903; the suppress
//! toggle (`WithoutBorder`) requires Win11 22000+ and returns
//! `E_NOTIMPL` on Win10 LTSC. DXGI reads the framebuffer below the
//! WGC compositor and never draws a border.
//!
//! ## Own `SinkWriter`, not `windows-capture::VideoEncoder`
//! The wrapper buries the interesting attributes
//! (HARDWARE_TRANSFORMS, profile, container hints). We open directly
//! via `MFCreateSinkWriterFromURL` so we can set them. Encoder tuning
//! via `ICodecAPI` is NOT attempted — that path crashed `WriteSample`
//! on Win10 LTSC.
//!
//! ## Dimensions forced even
//! MF derives the input stride from `buffer_len / height` for BGRA
//! buffers. Odd width or height shears the MP4 by one pixel per row.
//! `& !1` strips the low bit before allocation.
//!
//! BT.709 + studio swing tagged on both input and output so the GPU
//! encoder doesn't re-guess colour space by frame size. Audio captures
//! to raw PCM sidecars and ffmpeg muxes at stop, keeping AAC entirely
//! off the encoder thread.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clipo_core::Rect;
use thiserror::Error;
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaType, IMFSinkWriter, MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_TRANSFER_FUNCTION, MF_MT_VIDEO_NOMINAL_RANGE,
    MF_MT_VIDEO_PRIMARIES, MF_MT_YUV_MATRIX, MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_VERSION,
    MFCreateAttributes, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFCreateSinkWriterFromURL, MFMediaType_Video, MFNominalRange_16_235, MFShutdown, MFStartup,
    MFVideoFormat_ARGB32, MFVideoFormat_H264, MFVideoInterlace_Progressive,
    MFVideoPrimaries_BT709, MFVideoTransFunc_709, MFVideoTransferMatrix_BT709,
};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::core::{GUID, HSTRING};
use windows_capture::dxgi_duplication_api::DxgiDuplicationApi;
use windows_capture::monitor::Monitor;

use crate::audio::{self, AudioCapture, AudioEndpoint};
use crate::clock::MasterClock;
use crate::cursor::CursorOverlay;
use crate::ffmpeg;

const MIN_CROP_EDGE: u32 = 32;
const FRAME_CHANNEL_CAP: usize = 2;

#[derive(Debug, Error)]
pub enum VideoError {
    #[error("capture: {0}")]
    Capture(String),
    #[error("crop too small ({width}x{height}); minimum is {min}x{min}", min = MIN_CROP_EDGE)]
    CropTooSmall { width: u32, height: u32 },
}

#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub rect: Rect,
    pub output: PathBuf,
    pub fps: u32,
    pub bitrate_bps: u32,
    pub capture_audio: bool,
    pub capture_mic: bool,
    pub show_cursor: bool,
    pub show_clicks: bool,
    /// `Some(path)` → try the hardware encoder, marking an in-flight recording at
    /// `path` so a native encoder crash is detectable next launch. `None` → use
    /// the software encoder directly (the caller already knows hardware is unsafe).
    pub crash_sentinel: Option<PathBuf>,
}

#[derive(Clone)]
struct Flags {
    halt: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    audio_muted: Arc<AtomicBool>,
    mic_muted: Arc<AtomicBool>,
    clock: Arc<MasterClock>,
}

impl Flags {
    fn new() -> Self {
        Self {
            halt: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            audio_muted: Arc::new(AtomicBool::new(false)),
            mic_muted: Arc::new(AtomicBool::new(false)),
            clock: Arc::new(MasterClock::new()),
        }
    }
}

pub struct VideoRecorder {
    flags: Flags,
    thread: Option<JoinHandle<Result<(), String>>>,
}

impl VideoRecorder {
    /// Start recording. Returns once the encoder pipeline is up.
    pub fn start(cfg: VideoConfig) -> Result<Self, VideoError> {
        let crop_w = cfg.rect.width & !1;
        let crop_h = cfg.rect.height & !1;
        if crop_w < MIN_CROP_EDGE || crop_h < MIN_CROP_EDGE {
            return Err(VideoError::CropTooSmall {
                width: crop_w,
                height: crop_h,
            });
        }

        let flags = Flags::new();
        let flags_thread = flags.clone();
        let cfg_thread = cfg.clone();
        let thread = thread::Builder::new()
            .name("clipo-recorder".into())
            .spawn(move || run_pipeline(flags_thread, cfg_thread, crop_w, crop_h))
            .map_err(|e| VideoError::Capture(format!("spawn capture thread: {e}")))?;

        Ok(Self {
            flags,
            thread: Some(thread),
        })
    }

    /// Block until the SinkWriter writes its moov atom (<250 ms typical).
    pub fn stop(mut self) -> Result<(), VideoError> {
        self.flags.halt.store(true, Ordering::Release);
        match self.thread.take().map(JoinHandle::join) {
            None | Some(Ok(Ok(()))) => Ok(()),
            Some(Ok(Err(e))) => Err(VideoError::Capture(e)),
            Some(Err(_)) => Err(VideoError::Capture("recording thread panicked".into())),
        }
    }

    /// Pause. Clock freezes FIRST, then the `paused` flag flips —
    /// otherwise an audio thread that already passed the flag check
    /// would read a still-running clock and emit a packet PTS higher
    /// than every post-resume PTS, confusing the muxer. Idempotent.
    pub fn pause(&self) {
        self.flags.clock.pause();
        self.flags.paused.store(true, Ordering::Release);
    }

    /// Resume. Clock unfreezes FIRST. Reversed order would let the
    /// audio thread observe `paused = false`, read a still-frozen
    /// clock, and emit a duplicate-PTS packet that the MP4 muxer
    /// either rejects or rewrites. Idempotent.
    pub fn resume(&self) {
        self.flags.clock.resume();
        self.flags.paused.store(false, Ordering::Release);
    }

    #[must_use]
    pub fn is_paused(&self) -> bool {
        self.flags.paused.load(Ordering::Acquire)
    }

    pub fn set_audio_muted(&self, muted: bool) {
        self.flags.audio_muted.store(muted, Ordering::Release);
    }

    #[must_use]
    pub fn is_audio_muted(&self) -> bool {
        self.flags.audio_muted.load(Ordering::Acquire)
    }

    pub fn set_mic_muted(&self, muted: bool) {
        self.flags.mic_muted.store(muted, Ordering::Release);
    }

    #[must_use]
    pub fn is_mic_muted(&self) -> bool {
        self.flags.mic_muted.load(Ordering::Acquire)
    }

    /// Effective recording duration in seconds (pause-aware).
    #[must_use]
    pub fn elapsed_secs(&self) -> u64 {
        let ticks = self.flags.clock.elapsed_100ns().max(0);
        (ticks as u64) / 10_000_000
    }
}

impl Drop for VideoRecorder {
    fn drop(&mut self) {
        self.flags.halt.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ─────── thread guards ───────

/// Bump the multimedia timer to 1 ms so per-frame `sleep` is accurate.
struct HighResTimerGuard;
impl HighResTimerGuard {
    fn new() -> Self {
        // SAFETY: fire-and-forget; Drop pairs it.
        unsafe {
            let _ = timeBeginPeriod(1);
        }
        Self
    }
}
impl Drop for HighResTimerGuard {
    fn drop(&mut self) {
        // SAFETY: pairs timeBeginPeriod.
        unsafe {
            let _ = timeEndPeriod(1);
        }
    }
}

struct ComGuard;
impl ComGuard {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: S_OK / S_FALSE / RPC_E_CHANGED_MODE all acceptable.
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            if hr.is_err() && hr.0 != 0x8001_0106_u32.cast_signed() {
                return Err(windows::core::Error::from(hr));
            }
        }
        Ok(Self)
    }
}
impl Drop for ComGuard {
    fn drop(&mut self) {
        // SAFETY: pairs CoInitializeEx on this thread.
        unsafe {
            CoUninitialize();
        }
    }
}

struct MfGuard;
impl MfGuard {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: safe from any COM-initialised thread.
        unsafe { MFStartup(MF_VERSION, 0) }?;
        Ok(Self)
    }
}
impl Drop for MfGuard {
    fn drop(&mut self) {
        // SAFETY: pairs MFStartup.
        unsafe {
            let _ = MFShutdown();
        }
    }
}

/// Clears the hardware-encoder crash sentinel on scope exit. Armed (file written)
/// before the GPU encoder is touched; a clean return — success OR a graceful
/// error — runs this Drop and removes it. Only a *native* crash skips Drop and
/// leaves the file, which is exactly the signal the next launch reads to fall
/// back to the software encoder.
struct SentinelGuard(Option<PathBuf>);
impl SentinelGuard {
    fn arm(path: Option<PathBuf>) -> Self {
        if let Some(p) = &path {
            let _ = std::fs::write(p, []);
        }
        Self(path)
    }
}
impl Drop for SentinelGuard {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = std::fs::remove_file(p);
        }
    }
}

// ─────── pipeline ───────

fn run_pipeline(flags: Flags, cfg: VideoConfig, crop_w: u32, crop_h: u32) -> Result<(), String> {
    let _timer = HighResTimerGuard::new();
    let _com = ComGuard::new().map_err(|e| format!("CoInitializeEx: {e}"))?;
    let _mf = MfGuard::new().map_err(|e| format!("MFStartup: {e}"))?;

    // Arm the crash sentinel before any GPU work: `Some` means try hardware and
    // mark this recording in-flight; the guard clears it on a clean return.
    let prefer_hardware = cfg.crash_sentinel.is_some();
    let _sentinel = SentinelGuard::arm(cfg.crash_sentinel);

    let monitor = Monitor::primary().map_err(|e| format!("monitor: {e}"))?;
    let dup = DxgiDuplicationApi::new(monitor).map_err(|e| format!("dxgi: {e}"))?;

    let crop_x = u32::try_from(cfg.rect.x.max(0)).unwrap_or(0);
    let crop_y = u32::try_from(cfg.rect.y.max(0)).unwrap_or(0);

    // Audio sidecars + ffmpeg discovery up front. No ffmpeg → record
    // silent; muxer skipped, video goes straight to final path.
    let ffmpeg_path = ffmpeg::locate();
    let want_any_audio = (cfg.capture_audio || cfg.capture_mic) && ffmpeg_path.is_some();
    if (cfg.capture_audio || cfg.capture_mic) && ffmpeg_path.is_none() {
        tracing::warn!("recording: ffmpeg.exe not found; recording without audio");
    }

    let (video_target, sys_pcm_path, mic_pcm_path) = if want_any_audio {
        (
            cfg.output.with_extension("tmp.mp4"),
            cfg.capture_audio.then(|| cfg.output.with_extension("tmp.sys.pcm")),
            cfg.capture_mic.then(|| cfg.output.with_extension("tmp.mic.pcm")),
        )
    } else {
        (cfg.output.clone(), None, None)
    };

    let sys_audio = spawn_audio(
        AudioEndpoint::SystemLoopback,
        sys_pcm_path.as_ref(),
        &flags.paused,
        &flags.audio_muted,
    );
    let mic_audio = spawn_audio(
        AudioEndpoint::Microphone,
        mic_pcm_path.as_ref(),
        &flags.paused,
        &flags.mic_muted,
    );

    // Hardware encoder when allowed, with a graceful HRESULT fallback to software
    // (broken Intel HD / ARM laptops). A native encoder crash is handled out of
    // band by the sentinel above, not here.
    let (writer, video_stream) = create_writer_with_fallback(
        &video_target,
        crop_w,
        crop_h,
        cfg.fps,
        cfg.bitrate_bps,
        prefer_hardware,
    )?;

    let (frame_tx, frame_rx) = flume::bounded::<Vec<u8>>(FRAME_CHANNEL_CAP);

    let capture_halt = flags.halt.clone();
    let capture_handle = thread::Builder::new()
        .name("clipo-recorder-capture".into())
        .spawn(move || {
            capture_loop(
                &capture_halt,
                dup,
                crop_x,
                crop_y,
                crop_w,
                crop_h,
                cfg.show_cursor,
                cfg.show_clicks,
                &frame_tx,
            );
        })
        .map_err(|e| format!("spawn capture sub-thread: {e}"))?;

    let encode_result = encode_loop(
        &flags,
        &writer,
        video_stream,
        &frame_rx,
        cfg.fps,
        crop_w,
        crop_h,
    );

    let _ = capture_handle.join();
    let sys_has_content = sys_audio.is_some_and(AudioCapture::stop);
    let mic_has_content = mic_audio.is_some_and(AudioCapture::stop);

    // SAFETY: writer outlives this call; Finalize flushes + writes moov.
    let finalize_result = unsafe { writer.Finalize() }.map_err(|e| format!("Finalize: {e}"));

    finalize_or_rename(
        &cfg.output,
        &video_target,
        ffmpeg_path.as_deref(),
        sys_pcm_path.as_ref().filter(|_| sys_has_content),
        mic_pcm_path.as_ref().filter(|_| mic_has_content),
        finalize_result.is_ok(),
    );

    // Cleanup PCM sidecars best-effort.
    for p in [&sys_pcm_path, &mic_pcm_path].iter().filter_map(|o| o.as_ref()) {
        let _ = std::fs::remove_file(p);
    }

    encode_result.and(finalize_result)
}

/// Mux PCM sidecars into the final MP4 if any audio captured; otherwise
/// rename the video temp into the final path.
fn finalize_or_rename(
    output: &Path,
    video_target: &Path,
    ffmpeg_path: Option<&Path>,
    sys_pcm: Option<&PathBuf>,
    mic_pcm: Option<&PathBuf>,
    finalize_ok: bool,
) {
    let want_mux = (sys_pcm.is_some() || mic_pcm.is_some()) && ffmpeg_path.is_some() && finalize_ok;
    if want_mux {
        let muxed = mux_audio_into_mp4(ffmpeg_path.unwrap(), video_target, sys_pcm, mic_pcm, output)
            .is_ok();
        if muxed {
            let _ = std::fs::remove_file(video_target);
            return;
        }
    }
    if video_target != output {
        if let Err(e) = std::fs::rename(video_target, output) {
            tracing::error!(error = %e, "recording: rename video temp to final");
        }
    }
}

fn mux_audio_into_mp4(
    ffmpeg: &Path,
    video_in: &Path,
    sys_pcm: Option<&PathBuf>,
    mic_pcm: Option<&PathBuf>,
    final_out: &Path,
) -> Result<(), String> {
    use std::process::Command;
    let sample_rate = audio::SAMPLE_RATE.to_string();
    let channels = audio::CHANNELS.to_string();

    let mut cmd = Command::new(ffmpeg);
    // No console flash: spawn ffmpeg detached from any console window.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.arg("-hide_banner")
        .args(["-loglevel", "warning", "-y", "-i"])
        .arg(video_in);

    let mut next_idx = 1u32;
    let mut sys_idx = None;
    let mut mic_idx = None;
    for (pcm, slot) in [(sys_pcm, &mut sys_idx), (mic_pcm, &mut mic_idx)] {
        if let Some(p) = pcm {
            cmd.args(["-f", "s16le", "-ar", &sample_rate, "-ac", &channels, "-i"])
                .arg(p);
            *slot = Some(next_idx);
            next_idx += 1;
        }
    }

    match (sys_idx, mic_idx) {
        (Some(s), Some(m)) => {
            let filter = format!("[{s}:a][{m}:a]amix=inputs=2:duration=longest:normalize=0[aout]");
            cmd.args(["-filter_complex", &filter])
                .args(["-map", "0:v", "-map", "[aout]"]);
        }
        (Some(idx), None) | (None, Some(idx)) => {
            cmd.args(["-map", "0:v"])
                .args(["-map", &format!("{idx}:a")]);
        }
        (None, None) => return Err("mux requested without any audio source".into()),
    }

    cmd.args(["-c:v", "copy", "-c:a", "aac", "-b:a", "128k", "-shortest"])
        .arg(final_out);

    let status = cmd.status().map_err(|e| format!("spawn ffmpeg: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("ffmpeg exited with {status}"))
    }
}

fn spawn_audio(
    endpoint: AudioEndpoint,
    pcm_path: Option<&PathBuf>,
    paused: &Arc<AtomicBool>,
    muted: &Arc<AtomicBool>,
) -> Option<AudioCapture> {
    let path = pcm_path?.clone();
    let label = match endpoint {
        AudioEndpoint::SystemLoopback => "system audio",
        AudioEndpoint::Microphone => "microphone",
    };
    match AudioCapture::start(endpoint, path, paused.clone(), muted.clone()) {
        Ok(a) => {
            tracing::info!("recording: {label} capture started");
            Some(a)
        }
        Err(e) => {
            tracing::warn!(error = %e, "recording: {label} capture failed");
            None
        }
    }
}

fn create_writer_with_fallback(
    output: &Path,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
    prefer_hardware: bool,
) -> Result<(IMFSinkWriter, u32), String> {
    if !prefer_hardware {
        let pair = create_writer_and_begin(output, width, height, fps, bitrate_bps, false)
            .map_err(|e| format!("create sink writer (software): {e}"))?;
        tracing::info!("recording: using software encoder (compatibility)");
        return Ok(pair);
    }
    match create_writer_and_begin(output, width, height, fps, bitrate_bps, true) {
        Ok(pair) => {
            tracing::info!("recording: using hardware encoder");
            Ok(pair)
        }
        Err(hw_err) => {
            tracing::warn!(error = %hw_err, "recording: hardware encoder failed; trying software");
            let pair = create_writer_and_begin(output, width, height, fps, bitrate_bps, false)
                .map_err(|sw_err| format!("create sink writer (hw + sw failed): hw={hw_err} sw={sw_err}"))?;
            tracing::info!("recording: using software encoder");
            Ok(pair)
        }
    }
}

fn create_writer_and_begin(
    output: &Path,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
    prefer_hardware: bool,
) -> Result<(IMFSinkWriter, u32), String> {
    let (writer, stream) = create_sink_writer(output, width, height, fps, bitrate_bps, prefer_hardware)
        .map_err(|e| format!("create_sink_writer: {e}"))?;
    // SAFETY: documented thread-safe after input media type is set.
    unsafe { writer.BeginWriting() }.map_err(|e| format!("BeginWriting: {e}"))?;
    Ok((writer, stream))
}

fn create_sink_writer(
    output: &Path,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_bps: u32,
    prefer_hardware: bool,
) -> windows::core::Result<(IMFSinkWriter, u32)> {
    // MF_LOW_LATENCY NOT set — streaming hint, costs ~15-20% bitrate.
    let attrs = mf_create_attrs(1)?;
    // SAFETY: u32 attribute write.
    unsafe {
        attrs.SetUINT32(
            &MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
            u32::from(prefer_hardware),
        )?;
    }

    let url = HSTRING::from(output.as_os_str());
    // SAFETY: url outlives the call; None bytestream → container from ext.
    let writer = unsafe { MFCreateSinkWriterFromURL(&url, None, &attrs) }?;

    let video_out = make_video_type(&MFVideoFormat_H264, width, height, fps, |t| {
        // SAFETY: t is the fresh media type we just created.
        unsafe { t.SetUINT32(&MF_MT_AVG_BITRATE, bitrate_bps) }
    })?;
    // SAFETY: writer is fresh; out_type owns its attributes.
    let video_stream = unsafe { writer.AddStream(&video_out) }?;

    // MFVideoFormat_ARGB32 is 32-bit BGRA in Windows memory order
    // (B in byte 0). Name is historical.
    let video_in = make_video_type(&MFVideoFormat_ARGB32, width, height, fps, |t| {
        // Positive stride = top-down rows. DXGI hands us top-down.
        // SAFETY: same as above.
        unsafe { t.SetUINT32(&MF_MT_DEFAULT_STRIDE, width * 4) }
    })?;
    // SAFETY: same as AddStream.
    unsafe { writer.SetInputMediaType(video_stream, &video_in, None) }?;

    Ok((writer, video_stream))
}

fn mf_create_attrs(initial: u32) -> windows::core::Result<IMFAttributes> {
    let mut out: Option<IMFAttributes> = None;
    // SAFETY: writes COM pointer into out.
    unsafe { MFCreateAttributes(&raw mut out, initial) }?;
    out.ok_or_else(|| windows::core::Error::from(windows::core::HRESULT(-1)))
}

fn make_video_type(
    subtype: &GUID,
    width: u32,
    height: u32,
    fps: u32,
    extras: impl FnOnce(&IMFMediaType) -> windows::core::Result<()>,
) -> windows::core::Result<IMFMediaType> {
    // SAFETY: MFCreateMediaType + attribute writes on a fresh COM object.
    let t = unsafe { MFCreateMediaType() }?;
    unsafe {
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack_u32_pair(width, height))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(fps, 1))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_u32_pair(1, 1))?;
        // BT.709 + studio swing so the GPU encoder doesn't re-guess
        // colour space by frame size and the player reads consistent
        // primaries.
        t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        t.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_709.0 as u32)?;
        t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    extras(&t)?;
    Ok(t)
}

const fn pack_u32_pair(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | (lo as u64)
}

#[allow(clippy::too_many_arguments)]
fn capture_loop(
    halt: &AtomicBool,
    mut dup: DxgiDuplicationApi,
    crop_x: u32,
    crop_y: u32,
    crop_w: u32,
    crop_h: u32,
    show_cursor: bool,
    show_clicks: bool,
    tx: &flume::Sender<Vec<u8>>,
) {
    let frame_bytes = (crop_w as usize) * (crop_h as usize) * 4;
    // Pre-sized + filled, never .clear()'d between frames:
    // windows-capture v2's `as_nopadding_buffer` checks `capacity()`
    // to decide whether to resize, then indexes by `len()`. A Vec
    // with cap >= frame_size but len = 0 panics on the index.
    let mut tight: Vec<u8> = vec![0u8; frame_bytes];
    let mut overlay = if show_cursor || show_clicks {
        CursorOverlay::new(crop_w, crop_h, show_cursor, show_clicks)
            .inspect_err(|e| {
                tracing::warn!(error = %e, "cursor overlay init; recording without overlay");
            })
            .ok()
    } else {
        None
    };
    // 100 ms — empirically the cadence that keeps YouTube's DXVA2 happy.
    const TIMEOUT_MS: u32 = 100;

    while !halt.load(Ordering::Acquire) {
        let Ok(mut frame) = dup.acquire_next_frame(TIMEOUT_MS) else {
            // Timeout / no new frame; encoder thread emits nothing
            // (VFR timeline, player holds last frame).
            continue;
        };
        let end_x = crop_x + crop_w;
        let end_y = crop_y + crop_h;
        if end_x > frame.width() || end_y > frame.height() {
            continue;
        }
        let Ok(cropped) = frame.buffer_crop(crop_x, crop_y, end_x, end_y) else {
            continue;
        };
        let bytes = cropped.as_nopadding_buffer(&mut tight);
        // Top-down direct — hardware H.264 MFT reads top-down regardless
        // of MF_MT_DEFAULT_STRIDE; flipping inverts the output and costs
        // an 8 MB memcpy at 2K.
        let mut buf = bytes.to_vec();
        if let Some(overlay) = overlay.as_mut() {
            overlay.compose(&mut buf, crop_x, crop_y);
        }
        let _ = tx.try_send(buf);
    }
}

fn encode_loop(
    flags: &Flags,
    writer: &IMFSinkWriter,
    video_stream: u32,
    video_rx: &flume::Receiver<Vec<u8>>,
    fps: u32,
    width: u32,
    height: u32,
) -> Result<(), String> {
    let frame_interval = Duration::from_nanos(1_000_000_000 / u64::from(fps));
    let frame_duration_100ns = i64::try_from(10_000_000 / u64::from(fps)).unwrap_or(166_667);
    let frame_bytes = (width as usize) * (height as usize) * 4;

    let mut next_tick = Instant::now();
    let mut was_paused = false;
    while !flags.halt.load(Ordering::Acquire) {
        if flags.paused.load(Ordering::Acquire) {
            // Drain stale frames so a pre-pause buffer doesn't outlive
            // the freeze and encode with a post-resume PTS.
            while video_rx.try_recv().is_ok() {}
            was_paused = true;
            thread::sleep(Duration::from_millis(50));
            continue;
        }
        if was_paused {
            was_paused = false;
            next_tick = Instant::now();
        }

        let now = Instant::now();
        if now < next_tick {
            thread::sleep(next_tick - now);
        }
        next_tick += frame_interval;

        // Drain keeping only the latest frame; older ones are stale.
        let mut latest: Option<Vec<u8>> = None;
        loop {
            match video_rx.try_recv() {
                Ok(frame) => latest = Some(frame),
                Err(flume::TryRecvError::Empty) => break,
                Err(flume::TryRecvError::Disconnected) => {
                    if latest.is_none() {
                        return Ok(());
                    }
                    break;
                }
            }
        }

        if let Some(frame) = latest {
            if frame.len() == frame_bytes {
                let ts = flags.clock.elapsed_100ns();
                write_sample(writer, video_stream, &frame, ts, frame_duration_100ns)
                    .map_err(|e| format!("write_sample (video): {e}"))?;
            } else {
                tracing::warn!(
                    got = frame.len(),
                    expected = frame_bytes,
                    "recording: dropping frame with mismatched size"
                );
            }
        }
    }
    Ok(())
}

fn write_sample(
    writer: &IMFSinkWriter,
    stream_index: u32,
    bytes: &[u8],
    ts_100ns: i64,
    duration_100ns: i64,
) -> windows::core::Result<()> {
    // SAFETY: factory returns a fresh COM pointer.
    let buffer = unsafe { MFCreateMemoryBuffer(bytes.len() as u32) }?;
    // SAFETY: Lock/Unlock paired; max_len check guards SetCurrentLength.
    unsafe {
        let mut ptr = std::ptr::null_mut();
        let mut max_len = 0u32;
        let mut cur_len = 0u32;
        buffer.Lock(&raw mut ptr, Some(&raw mut max_len), Some(&raw mut cur_len))?;
        if ptr.is_null() || (max_len as usize) < bytes.len() {
            buffer.Unlock()?;
            return Err(windows::core::Error::from(windows::core::HRESULT(
                0x8007_000E_u32 as i32, // E_OUTOFMEMORY
            )));
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        buffer.SetCurrentLength(bytes.len() as u32)?;
        buffer.Unlock()?;
    }
    // SAFETY: empty sample; buffer + timestamps attached then submitted.
    let sample = unsafe { MFCreateSample() }?;
    unsafe {
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(ts_100ns)?;
        sample.SetSampleDuration(duration_100ns)?;
        writer.WriteSample(stream_index, &sample)?;
    }
    Ok(())
}
