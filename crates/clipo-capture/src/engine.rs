//! DXGI Desktop Duplication capture worker.
//!
//! D3D11 + DXGI objects are thread-affine, so a single worker thread
//! owns the device and answers `flume`-channel requests. Everyone else
//! holds a cheap [`CaptureHandle`].

#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

use std::thread;

use clipo_core::{CaptureError, CapturedImage, Rect};
use flume::{Receiver, Sender};
use windows::Win32::Foundation::{HMODULE, RECT as Win32Rect};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::Interface;

type CaptureResult = Result<CapturedImage, CaptureError>;

enum Request {
    Primary {
        respond: Sender<CaptureResult>,
    },
    Region {
        region: Rect,
        respond: Sender<CaptureResult>,
    },
}

#[derive(Debug, Clone)]
pub struct CaptureHandle {
    tx: Sender<Request>,
}

impl CaptureHandle {
    pub fn capture_primary(&self) -> CaptureResult {
        self.request(|respond| Request::Primary { respond })
    }

    pub fn capture_region(&self, region: Rect) -> CaptureResult {
        self.request(|respond| Request::Region { region, respond })
    }

    fn request(&self, mk: impl FnOnce(Sender<CaptureResult>) -> Request) -> CaptureResult {
        let (respond, rx) = flume::bounded(1);
        self.tx
            .send(mk(respond))
            .map_err(|_| CaptureError::Platform("capture worker dead".into()))?;
        rx.recv()
            .map_err(|_| CaptureError::Platform("capture worker dropped response".into()))?
    }
}

#[derive(Debug)]
pub struct CaptureEngine {
    tx: Sender<Request>,
}

impl CaptureEngine {
    /// Spawn the worker; returns once D3D + DXGI are initialised.
    #[tracing::instrument(name = "capture::engine_start")]
    pub fn start() -> Result<Self, CaptureError> {
        let (tx, rx) = flume::unbounded::<Request>();
        let (ready_tx, ready_rx) = flume::bounded::<Result<(), CaptureError>>(1);

        thread::Builder::new()
            .name("clipo-capture".into())
            .spawn(move || worker_main(rx, ready_tx))
            .map_err(|e| CaptureError::Platform(format!("spawn worker: {e}")))?;

        ready_rx
            .recv()
            .map_err(|_| CaptureError::Platform("worker init dropped".into()))??;
        tracing::info!("capture engine ready");
        Ok(Self { tx })
    }

    #[must_use]
    pub fn handle(&self) -> CaptureHandle {
        CaptureHandle {
            tx: self.tx.clone(),
        }
    }
}

#[allow(clippy::needless_pass_by_value)]
fn worker_main(rx: Receiver<Request>, ready_tx: Sender<Result<(), CaptureError>>) {
    let state = match WorkerState::init() {
        Ok(s) => {
            let _ = ready_tx.send(Ok(()));
            s
        }
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    while let Ok(request) = rx.recv() {
        match request {
            Request::Primary { respond } => {
                let _ = respond.send(state.capture_primary());
            }
            Request::Region { region, respond } => {
                let _ = respond.send(state.capture_region(region));
            }
        }
    }
    tracing::debug!("capture worker exiting");
}

struct WorkerState {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    factory: IDXGIFactory1,
}

impl WorkerState {
    fn init() -> Result<Self, CaptureError> {
        // SAFETY: D3D11CreateDevice + CreateDXGIFactory1 are documented
        // out-param FFI; we surface HRESULTs and require non-null
        // device/context returns.
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            let mut feature_level = D3D_FEATURE_LEVEL_11_0;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&raw mut device),
                Some(&raw mut feature_level),
                Some(&raw mut context),
            )
            .map_err(|e| CaptureError::Platform(format!("D3D11CreateDevice: {e}")))?;
            Ok(Self {
                device: device
                    .ok_or_else(|| CaptureError::Platform("null device".into()))?,
                context: context
                    .ok_or_else(|| CaptureError::Platform("null context".into()))?,
                factory: CreateDXGIFactory1()
                    .map_err(|e| CaptureError::Platform(format!("CreateDXGIFactory1: {e}")))?,
            })
        }
    }

    #[tracing::instrument(skip(self))]
    fn capture_primary(&self) -> CaptureResult {
        let (output, bounds) = self.primary_output()?;
        let dupl = self.duplicate(&output)?;
        let staging = self.grab_to_staging(&dupl, bounds.width, bounds.height)?;
        self.read_staging(&staging, bounds.width, bounds.height)
    }

    #[tracing::instrument(skip(self))]
    fn capture_region(&self, region: Rect) -> CaptureResult {
        let (output, bounds) = self.output_containing(region)?;
        let dupl = self.duplicate(&output)?;
        let staging = self.grab_to_staging(&dupl, bounds.width, bounds.height)?;
        let full = self.read_staging(&staging, bounds.width, bounds.height)?;
        Ok(crop(&full, bounds, region))
    }

    fn primary_output(&self) -> Result<(IDXGIOutput1, Rect), CaptureError> {
        self.adapter()
            .and_then(|adapter| output_at(&adapter, 0))?
            .ok_or_else(|| CaptureError::Platform("no primary output".into()))
    }

    fn output_containing(&self, region: Rect) -> Result<(IDXGIOutput1, Rect), CaptureError> {
        let adapter = self.adapter()?;
        for idx in 0u32.. {
            let Some((output, bounds)) = output_at(&adapter, idx)? else {
                return Err(CaptureError::RegionOutsideMonitor);
            };
            if rect_contains(bounds, region) {
                return Ok((output, bounds));
            }
        }
        unreachable!()
    }

    fn adapter(&self) -> Result<IDXGIAdapter1, CaptureError> {
        // SAFETY: factory is alive through `self`.
        unsafe {
            self.factory
                .EnumAdapters1(0)
                .map_err(|e| CaptureError::Platform(format!("EnumAdapters1: {e}")))
        }
    }

    fn duplicate(&self, output: &IDXGIOutput1) -> Result<IDXGIOutputDuplication, CaptureError> {
        // SAFETY: device alive through `self`.
        unsafe {
            output
                .DuplicateOutput(&self.device)
                .map_err(|e| CaptureError::Platform(format!("DuplicateOutput: {e}")))
        }
    }

    /// Acquire + copy-to-staging in one step.
    ///
    /// Copy-before-release order matters: once `ReleaseFrame` runs, DXGI
    /// is free to recycle the underlying surface, so a later
    /// `CopyResource` on the original texture would race against the
    /// next frame and silently read black / undefined pixels. The RAII
    /// `FrameGuard` ensures we release on every exit (including error).
    ///
    /// Empty success frames (LastPresentTime == 0) are skipped: fresh
    /// duplications often hand back an unpopulated surface for the
    /// first frame; wait for an actual desktop refresh.
    fn grab_to_staging(
        &self,
        dupl: &IDXGIOutputDuplication,
        width: u32,
        height: u32,
    ) -> Result<ID3D11Texture2D, CaptureError> {
        for attempt in 0..40 {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            // SAFETY: out-params written by AcquireNextFrame; ReleaseFrame
            // pairing handled by FrameGuard below.
            let result = unsafe { dupl.AcquireNextFrame(200, &raw mut info, &raw mut resource) };
            match result {
                Ok(()) => {
                    let _guard = FrameGuard(dupl);
                    let resource = resource.ok_or_else(|| {
                        CaptureError::Platform("AcquireNextFrame: null resource".into())
                    })?;
                    if info.LastPresentTime == 0 {
                        // No real present yet — surface is uninitialised.
                        // Guard releases; re-arm next attempt.
                        tracing::trace!(attempt, "skipping empty frame");
                        continue;
                    }
                    let texture: ID3D11Texture2D = resource
                        .cast()
                        .map_err(|e| CaptureError::Platform(format!("cast ID3D11Texture2D: {e}")))?;
                    return self.copy_to_staging(&texture, width, height);
                }
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                    tracing::trace!(attempt, "AcquireNextFrame timeout");
                }
                Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                    return Err(CaptureError::Platform("DXGI access lost".into()));
                }
                Err(e) => return Err(CaptureError::Platform(format!("AcquireNextFrame: {e}"))),
            }
        }
        Err(CaptureError::Platform(
            "AcquireNextFrame: no frame after 40 attempts (~8s)".into(),
        ))
    }

    fn copy_to_staging(
        &self,
        source: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<ID3D11Texture2D, CaptureError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        // SAFETY: out-param + standard staging copy on the worker thread.
        unsafe {
            self.device
                .CreateTexture2D(&raw const desc, None, Some(&raw mut staging))
                .map_err(|e| CaptureError::Platform(format!("CreateTexture2D: {e}")))?;
            let staging = staging
                .ok_or_else(|| CaptureError::Platform("CreateTexture2D null".into()))?;
            self.context.CopyResource(&staging, source);
            Ok(staging)
        }
    }

    fn read_staging(
        &self,
        staging: &ID3D11Texture2D,
        width: u32,
        height: u32,
    ) -> Result<CapturedImage, CaptureError> {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        let row_bytes = (width as usize) * 4;
        let mut bgra = vec![0u8; row_bytes * (height as usize)];
        // SAFETY: staging texture is USAGE_STAGING + CPU_ACCESS_READ;
        // Map/Unmap paired, row stride bounded by RowPitch.
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&raw mut mapped))
                .map_err(|e| CaptureError::Platform(format!("Map staging: {e}")))?;
            let src = mapped.pData.cast::<u8>();
            let stride = mapped.RowPitch as usize;
            for y in 0..(height as usize) {
                std::ptr::copy_nonoverlapping(
                    src.add(y * stride),
                    bgra.as_mut_ptr().add(y * row_bytes),
                    row_bytes,
                );
            }
            self.context.Unmap(staging, 0);
        }
        Ok(CapturedImage {
            width,
            height,
            bgra,
        })
    }
}

struct FrameGuard<'a>(&'a IDXGIOutputDuplication);

impl Drop for FrameGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: caller paired this with a successful AcquireNextFrame.
        unsafe {
            let _ = self.0.ReleaseFrame();
        }
    }
}

/// Enumerate one output by adapter index. `Ok(None)` = past the last
/// output (not an error); `Err` = real platform failure.
fn output_at(
    adapter: &IDXGIAdapter1,
    idx: u32,
) -> Result<Option<(IDXGIOutput1, Rect)>, CaptureError> {
    // SAFETY: EnumOutputs returns Err past end; we surface that as None.
    unsafe {
        let Ok(output) = adapter.EnumOutputs(idx) else {
            return Ok(None);
        };
        let output1: IDXGIOutput1 = output
            .cast()
            .map_err(|e| CaptureError::Platform(format!("cast IDXGIOutput1: {e}")))?;
        let desc = output1
            .GetDesc()
            .map_err(|e| CaptureError::Platform(format!("GetDesc: {e}")))?;
        Ok(Some((output1, win32_to_rect(desc.DesktopCoordinates))))
    }
}

fn win32_to_rect(r: Win32Rect) -> Rect {
    Rect {
        x: r.left,
        y: r.top,
        width: (r.right - r.left).max(0) as u32,
        height: (r.bottom - r.top).max(0) as u32,
    }
}

#[allow(clippy::cast_possible_wrap)]
const fn rect_contains(outer: Rect, inner: Rect) -> bool {
    let inner_right = inner.x + inner.width as i32;
    let inner_bottom = inner.y + inner.height as i32;
    let outer_right = outer.x + outer.width as i32;
    let outer_bottom = outer.y + outer.height as i32;
    inner.x >= outer.x
        && inner.y >= outer.y
        && inner_right <= outer_right
        && inner_bottom <= outer_bottom
}

/// Crop `full` (in `full_bounds` virtual-screen coords) to `region`.
/// Clamps to `full` so a stale rect silently shrinks instead of panicking.
fn crop(full: &CapturedImage, full_bounds: Rect, region: Rect) -> CapturedImage {
    let local_x = (region.x - full_bounds.x).max(0) as u32;
    let local_y = (region.y - full_bounds.y).max(0) as u32;
    let eff_w = region.width.min(full.width.saturating_sub(local_x));
    let eff_h = region.height.min(full.height.saturating_sub(local_y));
    let row_bytes = (full.width as usize) * 4;
    let len = (eff_w as usize) * 4;
    let mut bgra = Vec::with_capacity(len * (eff_h as usize));
    for y in 0..eff_h {
        let off = ((local_y + y) as usize) * row_bytes + (local_x as usize) * 4;
        bgra.extend_from_slice(&full.bgra[off..off + len]);
    }
    CapturedImage {
        width: eff_w,
        height: eff_h,
        bgra,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn indexed_frame(w: u32, h: u32) -> CapturedImage {
        let mut bgra = Vec::with_capacity((w * h * 4) as usize);
        for i in 0..(w * h) {
            let v = u8::try_from(i).expect("test frames stay small");
            bgra.extend_from_slice(&[v, v, v, 255]);
        }
        CapturedImage {
            width: w,
            height: h,
            bgra,
        }
    }

    const fn r(x: i32, y: i32, w: u32, h: u32) -> Rect {
        Rect {
            x,
            y,
            width: w,
            height: h,
        }
    }

    #[test]
    fn crop_full_frame_is_identity() {
        let full = indexed_frame(4, 4);
        let out = crop(&full, r(0, 0, 4, 4), r(0, 0, 4, 4));
        assert_eq!((out.width, out.height), (4, 4));
        assert_eq!(out.bgra, full.bgra);
    }

    #[test]
    fn crop_subregion_picks_correct_pixels() {
        let out = crop(&indexed_frame(4, 4), r(0, 0, 4, 4), r(1, 1, 2, 2));
        assert_eq!((out.width, out.height), (2, 2));
        assert_eq!(out.bgra[0], 5); // (1,1)
        assert_eq!(out.bgra[4], 6); // (2,1)
        assert_eq!(out.bgra[8], 9); // (1,2)
    }

    #[test]
    fn crop_clamps_region_wider_than_frame() {
        let out = crop(&indexed_frame(4, 4), r(0, 0, 4, 4), r(2, 0, 10, 2));
        assert_eq!((out.width, out.height), (2, 2));
        assert_eq!(out.bgra[0], 2);
    }

    #[test]
    fn crop_clamps_region_taller_than_frame() {
        let out = crop(&indexed_frame(4, 4), r(0, 0, 4, 4), r(0, 3, 2, 10));
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(out.bgra[0], 12);
    }

    #[test]
    fn crop_translates_virtual_coords_with_monitor_offset() {
        let out = crop(&indexed_frame(4, 4), r(100, 100, 4, 4), r(101, 101, 2, 2));
        assert_eq!((out.width, out.height), (2, 2));
        assert_eq!(out.bgra[0], 5);
    }
}
