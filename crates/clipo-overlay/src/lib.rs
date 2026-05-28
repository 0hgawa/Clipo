//! Layered Win32 selection overlay. Freezes the desktop into a still
//! image (`BitBlt` + `CAPTUREBLT`), dims it, and lets the user drag a
//! rectangle over the frozen frame. The freeze is what makes the dim
//! immune to z-order leaks — the background is an image, so no
//! topmost window or the taskbar can composite over it.
//!
//! Threading: the window lives on its own thread (Win32 is
//! thread-affine). The daemon drives via `SendMessageW` from any
//! thread; selection results flow back through a flume channel.
//!
//! `WS_EX_TRANSPARENT` (click-through) is set ONLY during the recording
//! indicator — both show/hide paths strip it as a safety net so a
//! selection session always receives mouse input. The window region
//! (the recording ring) is also cleared on selection show; leaving it
//! would clip the overlay to that ring and refuse to paint the full
//! frozen frame (the "post-recording lock-up").

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::wildcard_imports
)]

use std::ffi::c_void;
use std::sync::Once;
use std::sync::mpsc::sync_channel;
use std::thread;

use clipo_core::Rect;
use parking_lot::Mutex;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ReleaseCapture, SetCapture, SetFocus, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

// ─────── constants ───────

const CLASS_NAME: PCWSTR = w!("ClipoOverlay");
const LABEL_CLASS_NAME: PCWSTR = w!("ClipoOverlayLabel");
const MAGNIFIER_CLASS_NAME: PCWSTR = w!("ClipoOverlayMagnifier");

// Dimmer alpha: 95/255 ≈ 37%. Dark enough to read as "selection mode",
// light enough to keep the desktop legible.
const DIMMER_ALPHA: u8 = 95;

// Recording indicator: 3 px solid red ring. Matches `--color-danger`
// (Material Red A400 #FF1744) used elsewhere in the app so the
// recording cue reads as the same semantic red across surfaces.
const RECORDING_BORDER_PX: i32 = 3;
const RECORDING_BORDER_R: u8 = 0xFF;
const RECORDING_BORDER_G: u8 = 0x17;
const RECORDING_BORDER_B: u8 = 0x44;

// Dimensions label (W × H pill near cursor). Fixed width avoids jitter.
const LABEL_WIDTH: i32 = 136;
const LABEL_HEIGHT: i32 = 26;
const LABEL_CURSOR_OFFSET: i32 = 16;

// Pixel magnifier (4× zoom rounded popup).
const MAG_VIEW_SIZE: i32 = 120;
const MAG_SOURCE_SIZE: i32 = 30;
const MAG_CURSOR_OFFSET: i32 = 20;
const MAG_CORNER_RADIUS: i32 = 12;
const LABEL_BELOW_MAG_GAP: i32 = 4;

const WM_OVERLAY_SHOW: u32 = WM_USER + 1;
const WM_OVERLAY_HIDE: u32 = WM_USER + 2;
const WM_OVERLAY_QUIT: u32 = WM_USER + 3;
/// WPARAM = `Box<Rect>` ptr (virtual-desktop coords); handler frees it.
const WM_OVERLAY_RECORDING_INDICATOR: u32 = WM_USER + 4;
const WM_OVERLAY_RECORDING_END: u32 = WM_USER + 5;

const MIN_SELECTION_PX: i32 = 4;

// ─────── public types ───────

#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    #[error("Win32 error: {0}")]
    Win32(#[from] windows::core::Error),
    #[error("spawn overlay thread: {0}")]
    Spawn(String),
    #[error("overlay thread terminated before posting HWND")]
    ThreadGone,
}

#[derive(Debug, Clone, Copy)]
pub enum OverlayEvent {
    /// User confirmed a selection (virtual-screen coords).
    Confirmed(Rect),
    Cancelled,
}

#[derive(Debug)]
pub struct Overlay {
    inner: Mutex<OverlayInner>,
    events: flume::Receiver<OverlayEvent>,
}

#[derive(Debug)]
struct OverlayInner {
    hwnd: HWND,
    visible: bool,
    thread: Option<thread::JoinHandle<()>>,
}

// SAFETY: HWND is opaque; cross-thread access goes through SendMessageW.
unsafe impl Send for OverlayInner {}
unsafe impl Sync for OverlayInner {}

impl Overlay {
    pub fn spawn() -> Result<Self, OverlayError> {
        let (tx, rx) = sync_channel::<Result<isize, String>>(1);
        let (events_tx, events_rx) = flume::unbounded::<OverlayEvent>();

        let handle = thread::Builder::new()
            .name("clipo-overlay".into())
            .spawn(move || {
                // SAFETY: Win32 calls valid on this owning thread.
                let result = unsafe { create_window(events_tx) };
                match result {
                    Ok(hwnd) => {
                        let _ = tx.send(Ok(hwnd.0 as isize));
                        unsafe { run_message_loop() };
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e.to_string()));
                    }
                }
            })
            .map_err(|e| OverlayError::Spawn(e.to_string()))?;

        let hwnd_raw = rx.recv().map_err(|_| OverlayError::ThreadGone)?;
        let hwnd_raw = hwnd_raw.map_err(|e| {
            OverlayError::Win32(windows::core::Error::new(
                windows::core::HRESULT(0x8000_4005u32 as i32),
                e,
            ))
        })?;
        let hwnd = HWND(hwnd_raw as *mut _);

        Ok(Self {
            inner: Mutex::new(OverlayInner {
                hwnd,
                visible: false,
                thread: Some(handle),
            }),
            events: events_rx,
        })
    }

    #[must_use]
    pub fn events(&self) -> flume::Receiver<OverlayEvent> {
        self.events.clone()
    }

    /// Toggle the selection overlay. `show_magnifier` rides in WPARAM.
    pub fn toggle(&self, show_magnifier: bool) {
        let was_visible = self.inner.lock().visible;
        if was_visible {
            self.send(WM_OVERLAY_HIDE, 0);
            self.inner.lock().visible = false;
        } else {
            self.send(WM_OVERLAY_SHOW, usize::from(show_magnifier));
            self.inner.lock().visible = true;
        }
    }

    /// The window self-hid (after confirming a selection) — keep our
    /// `visible` flag in sync so the next toggle is a show, not a no-op.
    pub fn mark_hidden(&self) {
        self.inner.lock().visible = false;
    }

    /// Re-show as a click-through dim with a hole over `rect`
    /// (virtual-desktop coords). Used while a recording is in flight.
    pub fn show_recording_indicator(&self, rect: Rect) {
        // Box on the heap and ship the pointer through WPARAM. The
        // handler reclaims it via Box::from_raw — SendMessageW is
        // synchronous so the box is uniquely owned for that call.
        let rect_box = Box::into_raw(Box::new(rect));
        self.send(WM_OVERLAY_RECORDING_INDICATOR, rect_box as usize);
        self.inner.lock().visible = true;
    }

    pub fn hide_recording_indicator(&self) {
        let mut g = self.inner.lock();
        if !g.visible {
            return;
        }
        // SAFETY: cross-thread marshalled by SendMessageW.
        unsafe {
            SendMessageW(g.hwnd, WM_OVERLAY_RECORDING_END, Some(WPARAM(0)), Some(LPARAM(0)));
        }
        g.visible = false;
    }

    fn send(&self, msg: u32, wparam: usize) {
        let hwnd = self.inner.lock().hwnd;
        // SAFETY: SendMessageW marshals across threads.
        unsafe {
            SendMessageW(hwnd, msg, Some(WPARAM(wparam)), Some(LPARAM(0)));
        }
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        let handle = {
            let mut g = self.inner.lock();
            // SAFETY: PostMessageW is documented thread-safe.
            unsafe {
                let _ = PostMessageW(Some(g.hwnd), WM_OVERLAY_QUIT, WPARAM(0), LPARAM(0));
            }
            g.thread.take()
        };
        if let Some(h) = handle {
            let _ = h.join();
        }
    }
}

// ─────── window state (per-window via GWLP_USERDATA) ───────

struct WindowState {
    origin_x: i32,
    origin_y: i32,
    width: i32,
    height: i32,
    drag_start: Option<POINT>,
    current: Option<RECT>,
    label: HWND,
    magnifier: HWND,
    /// Frozen desktop snapshot (CAPTUREBLT). Bright source for selection.
    frozen: HBITMAP,
    /// Pre-darkened copy — drag hot path is two BitBlts, no per-pixel work.
    frozen_dim: HBITMAP,
    /// Pre-allocated compositing buffers (zero GDI alloc on WM_MOUSEMOVE).
    work_dc: HDC,
    work_bmp: HBITMAP,
    work_stock: HGDIOBJ,
    src_dc: HDC,
    events: flume::Sender<OverlayEvent>,
}

static REGISTERED: Once = Once::new();

unsafe fn create_window(events: flume::Sender<OverlayEvent>) -> windows::core::Result<HWND> {
    REGISTERED.call_once(|| {
        if let Err(e) = unsafe { register_classes() } {
            tracing::error!(?e, "overlay window class registration failed");
        }
    });

    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();
    let (origin_x, origin_y, cx, cy) = unsafe { virtual_desktop_bounds() };

    let label = unsafe { create_popup(LABEL_CLASS_NAME, instance, LABEL_WIDTH, LABEL_HEIGHT) }?;
    let magnifier = unsafe { create_magnifier_window(instance) }?;

    let state = Box::new(WindowState {
        origin_x,
        origin_y,
        width: cx,
        height: cy,
        drag_start: None,
        current: None,
        label,
        magnifier,
        frozen: HBITMAP::default(),
        frozen_dim: HBITMAP::default(),
        work_dc: HDC::default(),
        work_bmp: HBITMAP::default(),
        work_stock: HGDIOBJ::default(),
        src_dc: HDC::default(),
        events,
    });
    let state_ptr = Box::into_raw(state).cast::<c_void>();

    // SAFETY: state_ptr is reclaimed in WM_NCDESTROY (or below if create fails).
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            CLASS_NAME,
            w!("Clipo Overlay"),
            WS_POPUP,
            origin_x,
            origin_y,
            cx,
            cy,
            None,
            None,
            Some(instance),
            Some(state_ptr),
        )
    };
    match hwnd {
        Ok(h) => Ok(h),
        Err(e) => {
            // CreateWindowEx failed before WM_NCCREATE attached our box.
            drop(unsafe { Box::from_raw(state_ptr.cast::<WindowState>()) });
            Err(e)
        }
    }
}

unsafe fn register_classes() -> windows::core::Result<()> {
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None) }?.into();
    let cross = unsafe { LoadCursorW(None, IDC_CROSS) }?;
    let arrow = unsafe { LoadCursorW(None, IDC_ARROW) }?;

    // (class_name, wnd_proc, cursor, style)
    let classes: [(PCWSTR, WNDPROC, HCURSOR, WNDCLASS_STYLES); 3] = [
        (CLASS_NAME, Some(wnd_proc), cross, WNDCLASS_STYLES(0)),
        (LABEL_CLASS_NAME, Some(label_wnd_proc), arrow, WNDCLASS_STYLES(0)),
        // CS_DROPSHADOW gives the magnifier the native Win11 popup shadow.
        (MAGNIFIER_CLASS_NAME, Some(magnifier_wnd_proc), arrow, CS_DROPSHADOW),
    ];
    for (name, wndproc, cursor, style) in classes {
        let class = WNDCLASSW {
            style,
            lpfnWndProc: wndproc,
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: HICON::default(),
            hCursor: cursor,
            hbrBackground: HBRUSH::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: name,
        };
        if unsafe { RegisterClassW(&raw const class) } == 0 {
            return Err(windows::core::Error::from_win32());
        }
    }
    Ok(())
}

unsafe fn create_popup(
    class: PCWSTR,
    instance: HINSTANCE,
    w: i32,
    h: i32,
) -> windows::core::Result<HWND> {
    unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TOPMOST,
            class,
            windows::core::w!(""),
            WS_POPUP,
            0,
            0,
            w,
            h,
            None,
            None,
            Some(instance),
            None,
        )
    }
}

unsafe fn create_magnifier_window(instance: HINSTANCE) -> windows::core::Result<HWND> {
    let hwnd = unsafe { create_popup(MAGNIFIER_CLASS_NAME, instance, MAG_VIEW_SIZE, MAG_VIEW_SIZE) }?;
    // Round the clipping region (the class CS_DROPSHADOW adds the shadow).
    let radius = MAG_CORNER_RADIUS * 2 + 1;
    let rgn = unsafe {
        CreateRoundRectRgn(0, 0, MAG_VIEW_SIZE + 1, MAG_VIEW_SIZE + 1, radius, radius)
    };
    if !rgn.is_invalid() {
        // SetWindowRgn takes ownership.
        let _ = unsafe { SetWindowRgn(hwnd, Some(rgn), false) };
    }
    Ok(hwnd)
}

// ─────── main window proc ───────

#[allow(clippy::too_many_lines)]
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_NCCREATE {
        // SAFETY: lpCreateParams is the state_ptr we leaked in create_window.
        let cs = lparam.0 as *const CREATESTRUCTW;
        let state_ptr = unsafe { (*cs).lpCreateParams } as isize;
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr) };
    }

    let state_ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut WindowState;
    if state_ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // SAFETY: state_ptr is valid from WM_NCCREATE until WM_NCDESTROY.
    let state = unsafe { &mut *state_ptr };

    match msg {
        WM_OVERLAY_SHOW => {
            reset_state(state);
            // SAFETY: Win32 calls on the owning thread.
            unsafe {
                set_click_through(hwnd, false);
                hide_dimensions_label(state);
                // Clear any window region left by a prior recording ring.
                let _ = SetWindowRgn(hwnd, None, true);
                refresh_bounds(state, hwnd);
                capture_frozen(state);
                paint_frozen(hwnd, state, None);
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                let _ = SetForegroundWindow(hwnd);
                let _ = SetFocus(Some(hwnd));
                if wparam.0 != 0 {
                    let mut cursor = POINT::default();
                    let _ = GetCursorPos(&raw mut cursor);
                    update_magnifier(state, cursor);
                    let _ = ShowWindow(state.magnifier, SW_SHOWNOACTIVATE);
                }
            }
            LRESULT(0)
        }
        WM_OVERLAY_HIDE => {
            reset_state(state);
            // SAFETY: as above.
            unsafe {
                set_click_through(hwnd, false);
                hide_dimensions_label(state);
                free_frozen(state);
                let _ = ShowWindow(state.magnifier, SW_HIDE);
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        }
        WM_OVERLAY_RECORDING_INDICATOR => {
            // SAFETY: wparam is the Box<Rect> ptr the caller minted with
            // Box::into_raw; SendMessageW kept it live for this call.
            let rect = unsafe { *Box::from_raw(wparam.0 as *mut Rect) };
            // SAFETY: thread affinity satisfied.
            unsafe {
                refresh_bounds(state, hwnd);
                let local = RECT {
                    left: rect.x - state.origin_x,
                    top: rect.y - state.origin_y,
                    right: rect.x - state.origin_x + rect.width as i32,
                    bottom: rect.y - state.origin_y + rect.height as i32,
                };
                set_click_through(hwnd, true);
                if let Err(e) = paint_layered_solid(
                    hwnd,
                    state.width,
                    state.height,
                    RECORDING_BORDER_B,
                    RECORDING_BORDER_G,
                    RECORDING_BORDER_R,
                    255,
                ) {
                    tracing::warn!(error = ?e, "paint recording border");
                }
                update_recording_ring(hwnd, local, RECORDING_BORDER_PX);
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            LRESULT(0)
        }
        WM_OVERLAY_RECORDING_END => {
            // SAFETY: thread affinity satisfied.
            unsafe {
                set_click_through(hwnd, false);
                hide_dimensions_label(state);
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        }
        WM_OVERLAY_QUIT => {
            // SAFETY: thread affinity satisfied.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let pt = point_from_lparam(lparam);
            state.drag_start = Some(pt);
            state.current = None;
            // SAFETY: SetCapture on this window's thread.
            unsafe { SetCapture(hwnd) };
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let pt = point_from_lparam(lparam);
            // SAFETY: IsWindowVisible is a read; update_magnifier guards
            // GDI work behind that check.
            if unsafe { IsWindowVisible(state.magnifier) }.as_bool() {
                let screen_pt = POINT {
                    x: pt.x + state.origin_x,
                    y: pt.y + state.origin_y,
                };
                unsafe { update_magnifier(state, screen_pt) };
            }
            if let Some(start) = state.drag_start {
                let rect = normalize_rect(start, pt);
                state.current = Some(rect);
                // SAFETY: as above.
                unsafe {
                    paint_frozen(hwnd, state, Some(rect));
                    update_dimensions_label(state, rect, pt);
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            // SAFETY: ReleaseCapture pairs the SetCapture above.
            let _ = unsafe { ReleaseCapture() };
            if let Some(start) = state.drag_start.take() {
                state.current = None;
                let sel = normalize_rect(start, point_from_lparam(lparam));
                if !unsafe { confirm_and_hide(hwnd, state, sel) } {
                    // SAFETY: paint repaints the dim; treat as cancelled click.
                    unsafe {
                        hide_dimensions_label(state);
                        paint_frozen(hwnd, state, None);
                    }
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
            reset_state(state);
            let _ = state.events.send(OverlayEvent::Cancelled);
            // SAFETY: thread affinity satisfied.
            unsafe {
                hide_dimensions_label(state);
                free_frozen(state);
                let _ = ShowWindow(state.magnifier, SW_HIDE);
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        }
        WM_KEYDOWN if wparam.0 == VK_RETURN.0 as usize => {
            if let Some(rect) = state.current.take() {
                state.drag_start = None;
                let _ = unsafe { confirm_and_hide(hwnd, state, rect) };
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // SAFETY: standard message loop quit.
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // SAFETY: tear down child popups + reclaim the leaked Box.
            unsafe {
                free_frozen(state);
                let _ = DestroyWindow(state.label);
                let _ = DestroyWindow(state.magnifier);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            drop(unsafe { Box::from_raw(state_ptr) });
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

// ─────── helpers ───────

const fn reset_state(state: &mut WindowState) {
    state.drag_start = None;
    state.current = None;
}

/// Add or remove `WS_EX_TRANSPARENT` (click-through). Idempotent.
///
/// # Safety
/// Caller is on `hwnd`'s owning thread.
unsafe fn set_click_through(hwnd: HWND, on: bool) {
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new_ex = if on {
            ex | (WS_EX_TRANSPARENT.0 as isize)
        } else {
            ex & !(WS_EX_TRANSPARENT.0 as isize)
        };
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_ex);
    }
}

unsafe fn run_message_loop() {
    let mut msg = MSG::default();
    loop {
        // SAFETY: standard message loop.
        let ret = unsafe { GetMessageW(&raw mut msg, None, 0, 0) };
        if !ret.as_bool() {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&raw const msg);
            DispatchMessageW(&raw const msg);
        }
    }
}

unsafe fn virtual_desktop_bounds() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

/// Re-read virtual-desktop bounds and resize the window.
///
/// Bounds are captured once at spawn; without refreshing on each show,
/// a resolution / monitor-layout change leaves the overlay stuck at
/// boot-time size — smaller (or larger) than the current desktop,
/// with a hard visible edge. Must run before the freeze capture so
/// the BitBlt is taken at the current size.
unsafe fn refresh_bounds(state: &mut WindowState, hwnd: HWND) {
    let (ox, oy, cx, cy) = unsafe { virtual_desktop_bounds() };
    state.origin_x = ox;
    state.origin_y = oy;
    state.width = cx;
    state.height = cy;
    let _ = unsafe { SetWindowPos(hwnd, None, ox, oy, cx, cy, SWP_NOZORDER | SWP_NOACTIVATE) };
}

/// Repaint the layered overlay with a solid premultiplied BGRA. Mode
/// transitions only (not the drag hot path) — DIB alloc cost is fine.
unsafe fn paint_layered_solid(
    hwnd: HWND,
    cx: i32,
    cy: i32,
    b: u8,
    g: u8,
    r: u8,
    a: u8,
) -> windows::core::Result<()> {
    let screen_dc = unsafe { GetDC(None) };
    if screen_dc.is_invalid() {
        return Err(windows::core::Error::from_win32());
    }
    let mem_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
    if mem_dc.is_invalid() {
        unsafe { ReleaseDC(None, screen_dc) };
        return Err(windows::core::Error::from_win32());
    }

    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: cx,
            biHeight: -cy, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut bits_ptr: *mut c_void = std::ptr::null_mut();
    // SAFETY: standard DIB section creation.
    let bitmap = unsafe {
        CreateDIBSection(
            Some(screen_dc),
            &raw const bmi,
            DIB_RGB_COLORS,
            &raw mut bits_ptr,
            None,
            0,
        )
    }?;

    let pixel = u32::from_le_bytes([b, g, r, a]);
    let pixel_count = (cx as usize) * (cy as usize);
    // SAFETY: bits_ptr backs `pixel_count` u32s by construction.
    let pixels = unsafe { std::slice::from_raw_parts_mut(bits_ptr.cast::<u32>(), pixel_count) };
    pixels.fill(pixel);

    // SAFETY: standard layered update + GDI cleanup pairs.
    let result = unsafe {
        let old_obj = SelectObject(mem_dc, bitmap.into());
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let size = SIZE { cx, cy };
        let origin = POINT { x: 0, y: 0 };
        let r = UpdateLayeredWindow(
            hwnd,
            Some(screen_dc),
            Some(&raw const origin),
            Some(&raw const size),
            Some(mem_dc),
            Some(&raw const origin),
            COLORREF(0),
            Some(&raw const blend),
            ULW_ALPHA,
        );
        SelectObject(mem_dc, old_obj);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(mem_dc);
        ReleaseDC(None, screen_dc);
        r
    };
    result
}

/// Capture the virtual desktop into `frozen` (bright) and `frozen_dim`
/// (pre-darkened). CAPTUREBLT bakes in layered/topmost windows + the
/// taskbar so the dim is immune to z-order leaks: the background is a
/// still image, no always-on-top window can composite over it.
///
/// `frozen_dim` is pre-computed (one alpha-blend at capture time) so
/// each drag frame is two `BitBlt`s + one `UpdateLayeredWindow` —
/// zero per-pixel CPU work on the hot path.
unsafe fn capture_frozen(state: &mut WindowState) {
    unsafe { free_frozen(state) };
    let (cx, cy) = (state.width, state.height);
    let screen_dc = unsafe { GetDC(None) };
    if screen_dc.is_invalid() {
        return;
    }
    let bright = unsafe { CreateCompatibleBitmap(screen_dc, cx, cy) };
    let dark = unsafe { CreateCompatibleBitmap(screen_dc, cx, cy) };
    let mem = unsafe { CreateCompatibleDC(Some(screen_dc)) };
    if bright.is_invalid() || dark.is_invalid() || mem.is_invalid() {
        // SAFETY: best-effort cleanup; each branch checks before deleting.
        unsafe {
            if !bright.is_invalid() {
                let _ = DeleteObject(bright.into());
            }
            if !dark.is_invalid() {
                let _ = DeleteObject(dark.into());
            }
            if !mem.is_invalid() {
                let _ = DeleteDC(mem);
            }
            ReleaseDC(None, screen_dc);
        }
        return;
    }

    // SAFETY: bitmaps freshly created; BitBlt + SelectObject pairs handled.
    unsafe {
        let prev = SelectObject(mem, bright.into());
        let _ = BitBlt(
            mem, 0, 0, cx, cy, Some(screen_dc), state.origin_x, state.origin_y,
            SRCCOPY | CAPTUREBLT,
        );
        SelectObject(mem, dark.into());
        let _ = BitBlt(
            mem, 0, 0, cx, cy, Some(screen_dc), state.origin_x, state.origin_y,
            SRCCOPY | CAPTUREBLT,
        );
        darken(mem, cx, cy, screen_dc);
        SelectObject(mem, prev);
        let _ = DeleteDC(mem);
    }

    // Pre-allocate compositing buffers so paint_frozen does zero GDI
    // alloc on the drag hot path.
    let work_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
    let work_bmp = unsafe { CreateCompatibleBitmap(screen_dc, cx, cy) };
    let src_dc = unsafe { CreateCompatibleDC(Some(screen_dc)) };
    let work_stock = if work_dc.is_invalid() || work_bmp.is_invalid() {
        HGDIOBJ::default()
    } else {
        unsafe { SelectObject(work_dc, work_bmp.into()) }
    };
    unsafe { ReleaseDC(None, screen_dc) };
    state.frozen = bright;
    state.frozen_dim = dark;
    state.work_dc = work_dc;
    state.work_bmp = work_bmp;
    state.work_stock = work_stock;
    state.src_dc = src_dc;
}

/// Alpha-blend opaque black over `dst` at the dimmer strength. One
/// stretched 1×1 source — GPU path, no CPU per-pixel loop.
unsafe fn darken(dst: HDC, cx: i32, cy: i32, screen_dc: HDC) {
    let black = unsafe { CreateCompatibleBitmap(screen_dc, 1, 1) };
    if black.is_invalid() {
        return;
    }
    let src = unsafe { CreateCompatibleDC(Some(screen_dc)) };
    if src.is_invalid() {
        let _ = unsafe { DeleteObject(black.into()) };
        return;
    }
    // SAFETY: pairs all selections + deletes.
    unsafe {
        let prev = SelectObject(src, black.into());
        let _ = PatBlt(src, 0, 0, 1, 1, BLACKNESS);
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: DIMMER_ALPHA,
            AlphaFormat: 0,
        };
        let _ = AlphaBlend(dst, 0, 0, cx, cy, src, 0, 0, 1, 1, blend);
        SelectObject(src, prev);
        let _ = DeleteDC(src);
        let _ = DeleteObject(black.into());
    }
}

/// Push the frozen frame to the layered window: darkened everywhere,
/// bright inside `sel`. Two BitBlts + one UpdateLayeredWindow.
unsafe fn paint_frozen(hwnd: HWND, state: &WindowState, sel: Option<RECT>) {
    if state.frozen.is_invalid()
        || state.frozen_dim.is_invalid()
        || state.work_dc.is_invalid()
        || state.src_dc.is_invalid()
    {
        return;
    }
    let (cx, cy) = (state.width, state.height);
    let screen_dc = unsafe { GetDC(None) };
    if screen_dc.is_invalid() {
        return;
    }
    // SAFETY: work/src DCs alive through state; selections paired.
    unsafe {
        let src_prev = SelectObject(state.src_dc, state.frozen_dim.into());
        let _ = BitBlt(state.work_dc, 0, 0, cx, cy, Some(state.src_dc), 0, 0, SRCCOPY);
        if let Some(s) = sel {
            let (sw, sh) = (s.right - s.left, s.bottom - s.top);
            if sw > 0 && sh > 0 {
                SelectObject(state.src_dc, state.frozen.into());
                let _ = BitBlt(
                    state.work_dc, s.left, s.top, sw, sh, Some(state.src_dc), s.left, s.top, SRCCOPY,
                );
            }
        }
        SelectObject(state.src_dc, src_prev);

        let size = SIZE { cx, cy };
        let origin = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: 0,
        };
        let _ = UpdateLayeredWindow(
            hwnd,
            Some(screen_dc),
            Some(&raw const origin),
            Some(&raw const size),
            Some(state.work_dc),
            Some(&raw const origin),
            COLORREF(0),
            Some(&raw const blend),
            ULW_ALPHA,
        );
        ReleaseDC(None, screen_dc);
    }
}

/// Release the frozen pair + compositing buffers. Idempotent.
unsafe fn free_frozen(state: &mut WindowState) {
    // SAFETY: each check + delete is paired; setting to Default after
    // makes the next call a no-op.
    unsafe {
        if !state.work_dc.is_invalid() {
            if !state.work_stock.is_invalid() {
                SelectObject(state.work_dc, state.work_stock);
            }
            let _ = DeleteDC(state.work_dc);
            state.work_dc = HDC::default();
            state.work_stock = HGDIOBJ::default();
        }
        if !state.work_bmp.is_invalid() {
            let _ = DeleteObject(state.work_bmp.into());
            state.work_bmp = HBITMAP::default();
        }
        if !state.src_dc.is_invalid() {
            let _ = DeleteDC(state.src_dc);
            state.src_dc = HDC::default();
        }
        if !state.frozen.is_invalid() {
            let _ = DeleteObject(state.frozen.into());
            state.frozen = HBITMAP::default();
        }
        if !state.frozen_dim.is_invalid() {
            let _ = DeleteObject(state.frozen_dim.into());
            state.frozen_dim = HBITMAP::default();
        }
    }
}

/// Carve a ring around `sel` (combined with opaque red repaint =
/// recording border without dimming the captured content).
unsafe fn update_recording_ring(hwnd: HWND, sel: RECT, border_px: i32) {
    // SAFETY: rgn handles transferred / freed below.
    unsafe {
        let outer = CreateRectRgn(
            sel.left - border_px,
            sel.top - border_px,
            sel.right + border_px,
            sel.bottom + border_px,
        );
        let inner = CreateRectRgn(sel.left, sel.top, sel.right, sel.bottom);
        let ring = CreateRectRgn(0, 0, 1, 1);
        let _ = CombineRgn(Some(ring), Some(outer), Some(inner), RGN_DIFF);
        let _ = SetWindowRgn(hwnd, Some(ring), true);
        let _ = DeleteObject(outer.into());
        let _ = DeleteObject(inner.into());
    }
}

/// Validate the drag, emit Confirmed, and hide directly (not via
/// WM_OVERLAY_HIDE — avoids cross-thread roundtrip + race with capture).
unsafe fn confirm_and_hide(hwnd: HWND, state: &mut WindowState, sel: RECT) -> bool {
    let w = sel.right - sel.left;
    let h = sel.bottom - sel.top;
    if w < MIN_SELECTION_PX || h < MIN_SELECTION_PX {
        return false;
    }
    let confirmed = Rect {
        x: sel.left + state.origin_x,
        y: sel.top + state.origin_y,
        width: w as u32,
        height: h as u32,
    };
    let _ = state.events.send(OverlayEvent::Confirmed(confirmed));
    // SAFETY: thread affinity satisfied.
    unsafe {
        hide_dimensions_label(state);
        free_frozen(state);
        let _ = ShowWindow(state.magnifier, SW_HIDE);
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    true
}

// ─────── label window ───────

unsafe extern "system" fn label_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_PAINT {
        let mut ps = PAINTSTRUCT::default();
        // SAFETY: standard paint pairs.
        let hdc = unsafe { BeginPaint(hwnd, &raw mut ps) };
        unsafe { paint_label(hdc, hwnd) };
        let _ = unsafe { EndPaint(hwnd, &raw const ps) };
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe fn paint_label(hdc: HDC, hwnd: HWND) {
    let mut rect = RECT::default();
    // SAFETY: GetClientRect writes its out-param.
    let _ = unsafe { GetClientRect(hwnd, &raw mut rect) };

    // COLORREF = 0x00BBGGRR. 0x2B2B2B Fluent dark tooltip; 0xF2F2F2 text.
    // SAFETY: GDI calls with paired Select/Delete.
    unsafe {
        let brush = CreateSolidBrush(COLORREF(0x002B_2B2B));
        let _ = FillRect(hdc, &raw const rect, brush);
        let _ = DeleteObject(brush.into());

        let _ = SetTextColor(hdc, COLORREF(0x00F2_F2F2));
        let _ = SetBkMode(hdc, TRANSPARENT);

        let font = GetStockObject(DEFAULT_GUI_FONT);
        let prev_font = SelectObject(hdc, font);

        let mut buf = [0u16; 32];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len > 0 {
            let _ = DrawTextW(
                hdc,
                &mut buf[..len as usize],
                &raw mut rect,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        }
        let _ = SelectObject(hdc, prev_font);
    }
}

/// Update label text + position. Anchors below the magnifier when
/// visible (single visual cluster); otherwise floats near the cursor.
unsafe fn update_dimensions_label(state: &WindowState, sel: RECT, cursor: POINT) {
    let w = sel.right - sel.left;
    let h = sel.bottom - sel.top;
    // U+00D7 multiplication sign reads cleaner than ASCII "x".
    let text = format!("{w} \u{00D7} {h}");
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: utf16 owns the buffer for this call.
    let _ = unsafe { SetWindowTextW(state.label, PCWSTR(utf16.as_ptr())) };

    let pos = if unsafe { IsWindowVisible(state.magnifier) }.as_bool() {
        let mut mag = RECT::default();
        // SAFETY: GetWindowRect writes its out-param.
        let _ = unsafe { GetWindowRect(state.magnifier, &raw mut mag) };
        let max_x = state.origin_x + state.width - LABEL_WIDTH;
        let max_y = state.origin_y + state.height - LABEL_HEIGHT;
        let raw_x = mag.left + (MAG_VIEW_SIZE - LABEL_WIDTH) / 2;
        let raw_y = if mag.bottom + LABEL_BELOW_MAG_GAP + LABEL_HEIGHT
            <= state.origin_y + state.height
        {
            mag.bottom + LABEL_BELOW_MAG_GAP
        } else {
            mag.top - LABEL_BELOW_MAG_GAP - LABEL_HEIGHT
        };
        POINT {
            x: raw_x.clamp(state.origin_x, max_x.max(state.origin_x)),
            y: raw_y.clamp(state.origin_y, max_y.max(state.origin_y)),
        }
    } else {
        compute_label_position(
            cursor,
            SIZE {
                cx: LABEL_WIDTH,
                cy: LABEL_HEIGHT,
            },
            SIZE {
                cx: state.width,
                cy: state.height,
            },
        )
    };
    // SAFETY: window-affine GDI calls.
    unsafe {
        let _ = SetWindowPos(
            state.label,
            Some(HWND_TOPMOST),
            pos.x,
            pos.y,
            LABEL_WIDTH,
            LABEL_HEIGHT,
            SWP_SHOWWINDOW | SWP_NOACTIVATE,
        );
        let _ = InvalidateRect(Some(state.label), None, false);
    }
}

unsafe fn hide_dimensions_label(state: &WindowState) {
    // SAFETY: ShowWindow with a valid HWND.
    let _ = unsafe { ShowWindow(state.label, SW_HIDE) };
}

// ─────── magnifier window ───────

unsafe extern "system" fn magnifier_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // Skip the erase pass — paint repaints unconditionally.
    if msg == WM_ERASEBKGND {
        return LRESULT(1);
    }
    if msg == WM_PAINT {
        let mut ps = PAINTSTRUCT::default();
        // SAFETY: paint pairs.
        let hdc = unsafe { BeginPaint(hwnd, &raw mut ps) };
        unsafe { paint_magnifier(hdc) };
        let _ = unsafe { EndPaint(hwnd, &raw const ps) };
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Paint the 4× zoom + 1-px highlight around the centre pixel.
/// `StretchBlt` without CAPTUREBLT skips layered windows so the
/// overlay's own dimmer is excluded from the source.
unsafe fn paint_magnifier(hdc: HDC) {
    let mut cursor = POINT::default();
    // SAFETY: read of cursor coords.
    let _ = unsafe { GetCursorPos(&raw mut cursor) };
    let half = MAG_SOURCE_SIZE / 2;
    let src_x = cursor.x - half;
    let src_y = cursor.y - half;

    // SAFETY: GetDC/ReleaseDC paired; pen lifecycle bracketed.
    unsafe {
        let screen_dc = GetDC(None);
        let _ = SetStretchBltMode(hdc, COLORONCOLOR);
        let _ = StretchBlt(
            hdc, 0, 0, MAG_VIEW_SIZE, MAG_VIEW_SIZE,
            Some(screen_dc),
            src_x, src_y, MAG_SOURCE_SIZE, MAG_SOURCE_SIZE,
            SRCCOPY,
        );
        ReleaseDC(None, screen_dc);

        // 1-px white frame around the centre sample, one px outside it.
        let zoom = MAG_VIEW_SIZE / MAG_SOURCE_SIZE;
        let centre = MAG_SOURCE_SIZE / 2;
        let l = centre * zoom;
        let r = l + zoom;
        let pen = CreatePen(PS_SOLID, 1, COLORREF(0x00FF_FFFF));
        let old_pen = SelectObject(hdc, pen.into());
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        let _ = Rectangle(hdc, l - 1, l - 1, r + 1, r + 1);
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(pen.into());
    }
}

/// Reposition the magnifier near `cursor` (virtual-desktop coords) with
/// edge-flip so it stays on-screen.
unsafe fn update_magnifier(state: &WindowState, cursor: POINT) {
    let virtual_right = state.origin_x + state.width;
    let virtual_bottom = state.origin_y + state.height;

    let mut x = cursor.x + MAG_CURSOR_OFFSET;
    if x + MAG_VIEW_SIZE > virtual_right {
        x = cursor.x - MAG_CURSOR_OFFSET - MAG_VIEW_SIZE;
    }
    let mut y = cursor.y + MAG_CURSOR_OFFSET;
    if y + MAG_VIEW_SIZE > virtual_bottom {
        y = cursor.y - MAG_CURSOR_OFFSET - MAG_VIEW_SIZE;
    }

    // SAFETY: window-affine GDI calls.
    unsafe {
        let _ = SetWindowPos(
            state.magnifier,
            Some(HWND_TOPMOST),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
        let _ = InvalidateRect(Some(state.magnifier), None, false);
    }
}

/// Default position is bottom-right of `cursor`; flip to the other
/// side when it would overflow, then clamp.
fn compute_label_position(cursor: POINT, label: SIZE, screen: SIZE) -> POINT {
    let mut x = cursor.x + LABEL_CURSOR_OFFSET;
    let mut y = cursor.y + LABEL_CURSOR_OFFSET;
    if x + label.cx > screen.cx {
        x = cursor.x - LABEL_CURSOR_OFFSET - label.cx;
    }
    if y + label.cy > screen.cy {
        y = cursor.y - LABEL_CURSOR_OFFSET - label.cy;
    }
    let max_x = (screen.cx - label.cx).max(0);
    let max_y = (screen.cy - label.cy).max(0);
    POINT {
        x: x.clamp(0, max_x),
        y: y.clamp(0, max_y),
    }
}

fn point_from_lparam(lparam: LPARAM) -> POINT {
    let raw = lparam.0;
    let x = i32::from((raw & 0xFFFF) as i16);
    let y = i32::from(((raw >> 16) & 0xFFFF) as i16);
    POINT { x, y }
}

fn normalize_rect(a: POINT, b: POINT) -> RECT {
    RECT {
        left: a.x.min(b.x),
        top: a.y.min(b.y),
        right: a.x.max(b.x),
        bottom: a.y.max(b.y),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LABEL_CURSOR_OFFSET, LABEL_HEIGHT, LABEL_WIDTH, POINT, SIZE, compute_label_position,
        normalize_rect, point_from_lparam,
    };
    use windows::Win32::Foundation::LPARAM;

    #[test]
    fn normalize_handles_each_corner_direction() {
        let tl = POINT { x: 10, y: 20 };
        let br = POINT { x: 100, y: 200 };
        let r = normalize_rect(tl, br);
        assert_eq!((r.left, r.top, r.right, r.bottom), (10, 20, 100, 200));

        let r = normalize_rect(br, tl);
        assert_eq!((r.left, r.top, r.right, r.bottom), (10, 20, 100, 200));

        let tr = POINT { x: 100, y: 20 };
        let bl = POINT { x: 10, y: 200 };
        let r = normalize_rect(tr, bl);
        assert_eq!((r.left, r.top, r.right, r.bottom), (10, 20, 100, 200));
    }

    #[test]
    fn normalize_zero_size_rect_is_degenerate_but_valid() {
        let pt = POINT { x: 50, y: 60 };
        let r = normalize_rect(pt, pt);
        assert_eq!((r.left, r.top, r.right, r.bottom), (50, 60, 50, 60));
    }

    #[test]
    fn point_from_lparam_decodes_packed_i16_coords() {
        let (x, y) = (100_i32, 200_i32);
        let packed = (y << 16) | x;
        let pt = point_from_lparam(LPARAM(packed as isize));
        assert_eq!((pt.x, pt.y), (100, 200));
    }

    const fn label_size() -> SIZE {
        SIZE {
            cx: LABEL_WIDTH,
            cy: LABEL_HEIGHT,
        }
    }

    const fn fullhd() -> SIZE {
        SIZE { cx: 1920, cy: 1080 }
    }

    #[test]
    fn label_default_position_below_right_of_cursor() {
        let cursor = POINT { x: 500, y: 500 };
        let pos = compute_label_position(cursor, label_size(), fullhd());
        assert_eq!(
            (pos.x, pos.y),
            (500 + LABEL_CURSOR_OFFSET, 500 + LABEL_CURSOR_OFFSET),
        );
    }

    #[test]
    fn label_flips_left_when_right_edge_overflows() {
        let cursor = POINT { x: 1900, y: 500 };
        let pos = compute_label_position(cursor, label_size(), fullhd());
        let expected_x = 1900 - LABEL_CURSOR_OFFSET - LABEL_WIDTH;
        assert_eq!(pos.x, expected_x);
        assert!(pos.x + LABEL_WIDTH <= 1920);
    }

    #[test]
    fn label_flips_up_when_bottom_edge_overflows() {
        let cursor = POINT { x: 500, y: 1070 };
        let pos = compute_label_position(cursor, label_size(), fullhd());
        let expected_y = 1070 - LABEL_CURSOR_OFFSET - LABEL_HEIGHT;
        assert_eq!(pos.y, expected_y);
        assert!(pos.y + LABEL_HEIGHT <= 1080);
    }

    #[test]
    fn label_clamps_to_screen_when_neither_side_fits() {
        let cursor = POINT { x: 50, y: 50 };
        let pos = compute_label_position(
            cursor,
            SIZE { cx: 200, cy: 100 },
            SIZE { cx: 100, cy: 100 },
        );
        assert_eq!((pos.x, pos.y), (0, 0));
    }

    #[test]
    fn point_from_lparam_decodes_negative_coords_via_sign_extension() {
        let packed = (0xFFF6_i32 << 16) | 0xFFFB_i32;
        let pt = point_from_lparam(LPARAM(packed as isize));
        assert_eq!((pt.x, pt.y), (-5, -10));
    }
}
