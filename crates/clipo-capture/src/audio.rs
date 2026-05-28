//! WASAPI capture (system loopback or microphone) to a raw s16le PCM
//! file. ffmpeg muxes the file with the video MP4 at stop — keeps the
//! encoder thread free of subprocess scheduling contention.
//!
//! A previous attempt that ran ffmpeg as a pipe-fed subprocess
//! concurrently with recording dropped half the video frames under
//! GPU load (YouTube playing behind, etc.). File-then-mux moves the
//! heavy work to a moment when the user already pressed Stop.
//!
//! Canonical 48 kHz / 2-ch / s16, forced via AUTOCONVERTPCM +
//! SRC_DEFAULT_QUALITY. Windows resamples + matrixes every endpoint
//! (BT-HFP 16 kHz mono, 192 kHz DAC, virtual cables) in the kernel
//! before bytes reach us — capture loop is a pure memcpy + write.
//!
//! Pause skips file writes (WASAPI keeps being drained so the ring
//! doesn't overflow). Mute writes zeros so the muxer sees a contiguous
//! AAC stream. System silence (loopback emits no frames when nothing
//! is playing) is detected via `device_position` and backfilled with
//! zeros so the file's byte length tracks effective wall time.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use thiserror::Error;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT,
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, EDataFlow, ERole, IAudioCaptureClient, IAudioClient,
    IMMDeviceEnumerator, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, eCapture,
    eCommunications, eConsole, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};

pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u16 = 2;
const BYTES_PER_FRAME: usize = (CHANNELS as usize) * 2;

// 100 ms — short enough that pause latency is imperceptible.
const REQUESTED_BUFFER_100NS: i64 = 1_000_000;

const FLAG_DISCONTINUITY: u32 = AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32;
const FLAG_SILENT: u32 = AUDCLNT_BUFFERFLAGS_SILENT.0 as u32;

#[derive(Debug, Clone, Copy)]
pub enum AudioEndpoint {
    SystemLoopback,
    Microphone,
}

impl AudioEndpoint {
    const fn flow(self) -> EDataFlow {
        match self {
            Self::SystemLoopback => eRender,
            Self::Microphone => eCapture,
        }
    }
    const fn role(self) -> ERole {
        match self {
            Self::SystemLoopback => eConsole,
            Self::Microphone => eCommunications,
        }
    }
    const fn stream_flags(self) -> u32 {
        let base = AUDCLNT_STREAMFLAGS_EVENTCALLBACK
            | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
            | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        match self {
            Self::SystemLoopback => base | AUDCLNT_STREAMFLAGS_LOOPBACK,
            Self::Microphone => base,
        }
    }
    const fn thread_name(self) -> &'static str {
        match self {
            Self::SystemLoopback => "clipo-recorder-audio-sys",
            Self::Microphone => "clipo-recorder-audio-mic",
        }
    }
}

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("audio init: {0}")]
    Init(String),
}

impl From<windows::core::Error> for AudioError {
    fn from(e: windows::core::Error) -> Self {
        Self::Init(e.to_string())
    }
}

pub struct AudioCapture {
    halt: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    output_path: PathBuf,
}

impl AudioCapture {
    pub(crate) fn start(
        endpoint: AudioEndpoint,
        output_path: PathBuf,
        paused: Arc<AtomicBool>,
        muted: Arc<AtomicBool>,
    ) -> Result<Self, AudioError> {
        let halt = Arc::new(AtomicBool::new(false));
        let halt_thread = halt.clone();
        let path_thread = output_path.clone();
        let thread = thread::Builder::new()
            .name(endpoint.thread_name().to_string())
            .spawn(move || {
                if let Err(e) =
                    capture_loop(endpoint, &halt_thread, &paused, &muted, &path_thread)
                {
                    tracing::error!(error = %e, ?endpoint, "audio capture loop");
                }
            })
            .map_err(|e| AudioError::Init(format!("spawn audio thread: {e}")))?;
        Ok(Self {
            halt,
            thread: Some(thread),
            output_path,
        })
    }

    /// Halt + join; returns whether the file has at least one frame
    /// (caller decides whether to mux).
    pub(crate) fn stop(mut self) -> bool {
        self.shutdown();
        self.output_path
            .metadata()
            .is_ok_and(|m| m.len() >= BYTES_PER_FRAME as u64)
    }

    fn shutdown(&mut self) {
        self.halt.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct ComMta;
impl ComMta {
    fn new() -> Result<Self, AudioError> {
        // SAFETY: S_OK or S_FALSE (already MTA) both Ok via .ok().
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(Self)
    }
}
impl Drop for ComMta {
    fn drop(&mut self) {
        // SAFETY: pairs with CoInitializeEx on this thread.
        unsafe { CoUninitialize() };
    }
}

struct EventHandle(HANDLE);
impl EventHandle {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: default ACL, auto-reset, owned + closed in Drop.
        let h = unsafe { CreateEventW(None, false, false, None)? };
        Ok(Self(h))
    }
    const fn raw(&self) -> HANDLE {
        self.0
    }
}
impl Drop for EventHandle {
    fn drop(&mut self) {
        // SAFETY: handle from CreateEventW in new().
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

const fn canonical_wave_format() -> WAVEFORMATEX {
    let block_align = (CHANNELS as usize) * 2;
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: CHANNELS,
        nSamplesPerSec: SAMPLE_RATE,
        nAvgBytesPerSec: SAMPLE_RATE * (block_align as u32),
        nBlockAlign: block_align as u16,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

fn capture_loop(
    endpoint: AudioEndpoint,
    halt: &AtomicBool,
    paused: &AtomicBool,
    muted: &AtomicBool,
    output_path: &Path,
) -> Result<(), AudioError> {
    let _com = ComMta::new()?;
    let event = EventHandle::new()?;
    let format = canonical_wave_format();

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| AudioError::Init(format!("mkdir {}: {e}", parent.display())))?;
    }
    let file = File::create(output_path)
        .map_err(|e| AudioError::Init(format!("create {}: {e}", output_path.display())))?;
    // 64 KB ≈ 330 ms of audio — flushes far below the WASAPI period.
    let mut writer = BufWriter::with_capacity(64 * 1024, file);

    let (mut client, mut capture) = init_wasapi(endpoint, &event, &format)?;

    // Sized for the worst-case write: 1-second silence-gap backfill.
    let silence = vec![0u8; (SAMPLE_RATE as usize) * BYTES_PER_FRAME];

    // None = next write starts fresh (no silence-gap backfill).
    let mut last_device_pos: Option<u64> = None;
    // Rising edge of paused→running: drop the first post-resume packet
    // (the WASAPI ring straddles the resume click).
    let mut was_paused = false;

    while !halt.load(Ordering::Acquire) {
        // SAFETY: event owned here; 200 ms keeps `halt` observable.
        let wait = unsafe { WaitForSingleObject(event.raw(), 200) };
        if wait != WAIT_OBJECT_0 {
            continue;
        }

        // Drain the ring. On device invalidation we break out and
        // reconnect below — zero steady-state polling cost.
        let mut needs_reconnect = false;
        'drain: loop {
            // SAFETY: read of queued packet size.
            let frames = match unsafe { capture.GetNextPacketSize() } {
                Ok(n) => n,
                Err(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED => {
                    needs_reconnect = true;
                    break 'drain;
                }
                Err(e) => return Err(e.into()),
            };
            if frames == 0 {
                break;
            }

            let mut data: *mut u8 = ptr::null_mut();
            let mut got_frames: u32 = 0;
            let mut flags: u32 = 0;
            let mut device_pos: u64 = 0;
            // SAFETY: all out-params provided; success transfers buffer
            // ownership until the paired ReleaseBuffer below.
            match unsafe {
                capture.GetBuffer(
                    &raw mut data,
                    &raw mut got_frames,
                    &raw mut flags,
                    Some(&raw mut device_pos),
                    None,
                )
            } {
                Ok(()) => {}
                Err(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED => {
                    needs_reconnect = true;
                    break 'drain;
                }
                Err(e) => return Err(e.into()),
            }

            let write_forward = !paused.load(Ordering::Acquire);

            if (flags & FLAG_DISCONTINUITY) != 0 {
                // Ring overflowed — treat as fresh start so the silence
                // backfill below doesn't invent gigabytes from a stale
                // anchor.
                tracing::warn!("audio: WASAPI reported data discontinuity");
                last_device_pos = None;
            }

            if write_forward {
                if was_paused {
                    // Discard the resume-straddling packet so the file
                    // lines up with the master clock.
                    was_paused = false;
                    // SAFETY: pairs the GetBuffer.
                    unsafe { capture.ReleaseBuffer(got_frames)? };
                    continue;
                }

                // Backfill any silence the loopback skipped (it emits
                // zero frames while the system mix is silent). Cap at
                // 1 s so a stale device_pos can't synthesise minutes.
                if let Some(last) = last_device_pos {
                    let expected = last + u64::from(got_frames);
                    if device_pos > expected {
                        let gap = (device_pos - expected).min(u64::from(SAMPLE_RATE));
                        let bytes = (gap as usize) * BYTES_PER_FRAME;
                        let _ = writer.write_all(&silence[..bytes]);
                    }
                }
                let bytes = (got_frames as usize) * BYTES_PER_FRAME;
                if muted.load(Ordering::Acquire) || (flags & FLAG_SILENT) != 0 || data.is_null() {
                    let n = bytes.min(silence.len());
                    let _ = writer.write_all(&silence[..n]);
                } else {
                    // SAFETY: WASAPI contract — `data` valid for `bytes`
                    // bytes until ReleaseBuffer.
                    let src = unsafe { slice::from_raw_parts(data, bytes) };
                    let _ = writer.write_all(src);
                }
                last_device_pos = Some(device_pos);
            } else {
                // Paused: drain WASAPI without writing.
                last_device_pos = None;
                was_paused = true;
            }

            // SAFETY: pairs the GetBuffer above.
            match unsafe { capture.ReleaseBuffer(got_frames) } {
                Ok(()) => {}
                Err(e) if e.code() == AUDCLNT_E_DEVICE_INVALIDATED => {
                    needs_reconnect = true;
                    break 'drain;
                }
                Err(e) => return Err(e.into()),
            }
        }

        if needs_reconnect {
            // Old client dead — best-effort Stop, rebind below.
            // SAFETY: Stop is documented thread-safe + idempotent.
            let _ = unsafe { client.Stop() };
            match reconnect_wasapi(endpoint, halt, &event, &format) {
                Ok((new_client, new_capture)) => {
                    client = new_client;
                    capture = new_capture;
                    last_device_pos = None;
                    tracing::info!(?endpoint, "audio: reconnected");
                }
                Err(e) => {
                    tracing::error!(error = %e, ?endpoint, "audio: reconnect failed; stopping");
                    break;
                }
            }
        }
    }

    // SAFETY: Stop is documented thread-safe + idempotent.
    let _ = unsafe { client.Stop() };
    let _ = unsafe { SetEvent(event.raw()) };
    if let Err(e) = writer.flush() {
        tracing::error!(error = %e, "audio: flush on stop");
    }
    Ok(())
}

fn init_wasapi(
    endpoint: AudioEndpoint,
    event: &EventHandle,
    format: &WAVEFORMATEX,
) -> windows::core::Result<(IAudioClient, IAudioCaptureClient)> {
    // SAFETY: standard WASAPI bring-up. AUTOCONVERTPCM + SRC_DEFAULT
    // make Windows insert an SRC + matrixer in the kernel so every
    // endpoint arrives here as 48 kHz / 2-ch / s16.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device =
            enumerator.GetDefaultAudioEndpoint(endpoint.flow(), endpoint.role())?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            endpoint.stream_flags(),
            REQUESTED_BUFFER_100NS,
            0,
            format,
            None,
        )?;
        client.SetEventHandle(event.raw())?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        Ok((client, capture))
    }
}

/// Linear backoff retry after `AUDCLNT_E_DEVICE_INVALIDATED`. Bails out
/// when `halt` flips. 10 × 250 ms ≈ 11.5 s total ceiling.
fn reconnect_wasapi(
    endpoint: AudioEndpoint,
    halt: &AtomicBool,
    event: &EventHandle,
    format: &WAVEFORMATEX,
) -> windows::core::Result<(IAudioClient, IAudioCaptureClient)> {
    const MAX_ATTEMPTS: u32 = 10;
    const BASE_BACKOFF_MS: u64 = 250;

    for attempt in 1..=MAX_ATTEMPTS {
        if halt.load(Ordering::Acquire) {
            return Err(windows::core::Error::from(AUDCLNT_E_DEVICE_INVALIDATED));
        }
        match init_wasapi(endpoint, event, format) {
            Ok(pair) => return Ok(pair),
            Err(e) if attempt == MAX_ATTEMPTS => return Err(e),
            Err(e) => {
                let backoff_ms = BASE_BACKOFF_MS * u64::from(attempt);
                tracing::warn!(attempt, backoff_ms, error = %e, ?endpoint, "audio: reconnect retry");
                thread::sleep(Duration::from_millis(backoff_ms));
            }
        }
    }
    unreachable!()
}
