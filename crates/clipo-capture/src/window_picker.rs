//! Enumerate top-level windows that match the Alt+Tab inclusion rules,
//! focus + measure a chosen one, and extract per-window icons for the
//! picker UI.
//!
//! Matches the Alt+Tab ruleset documented at
//! <https://devblogs.microsoft.com/oldnewthing/20071008-00/?p=24863>
//! so the user's mental model is "if I can Alt+Tab to it, I can
//! capture it". Minimized windows are kept on purpose (they appear in
//! Alt+Tab); `focus_window_and_bounds` restores them before bounds are
//! re-read so the capture rect matches what the user sees.

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::ffi::c_void;
use std::ptr::addr_of_mut;

use clipo_core::Rect;
use serde::Serialize;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT as Win32Rect, TRUE, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DWMWINDOWATTRIBUTE, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC,
    DeleteObject, GetDIBits, GetObjectW, HBITMAP, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GCLP_HICON, GCLP_HICONSM, GW_OWNER, GWL_EXSTYLE, GetClassLongPtrW, GetIconInfo,
    GetShellWindow, GetWindow, GetWindowLongW, GetWindowPlacement, GetWindowTextLengthW,
    GetWindowTextW, HICON, ICON_BIG, ICON_SMALL, ICON_SMALL2, ICONINFO, IsIconic, IsWindowVisible,
    SMTO_ABORTIFHUNG, SW_RESTORE, SendMessageTimeoutW, SetForegroundWindow, ShowWindow,
    WINDOWPLACEMENT, WM_GETICON, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

// Soft cap on enumeration. Beyond this the picker UI is unscrollable.
const MAX_WINDOWS: usize = 64;
// SendMessageTimeoutW budget for WM_GETICON. 50 ms keeps a hung target
// from stalling the picker; responsive apps answer in <1 ms.
const ICON_PROBE_TIMEOUT_MS: u32 = 50;

/// Per-window UI payload. `id` is the raw HWND cast to i64 so the JSON
/// wire stays stable across 32/64-bit.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowInfo {
    pub id: i64,
    pub title: String,
    pub width: u32,
    pub height: u32,
}

#[must_use]
pub fn enumerate_capturable_windows() -> Vec<WindowInfo> {
    let mut out: Vec<WindowInfo> = Vec::with_capacity(32);
    // SAFETY: read-only Win32 query.
    let shell = unsafe { GetShellWindow() };
    let ctx = EnumCtx {
        out: addr_of_mut!(out),
        shell,
    };
    // SAFETY: EnumWindows is synchronous; ctx lives the entire call.
    unsafe {
        let _ = EnumWindows(
            Some(enum_callback),
            LPARAM(std::ptr::addr_of!(ctx) as isize),
        );
    }
    out
}

/// Bring `id` to the foreground (restoring if minimized) and re-read
/// its frame bounds. `None` when the window closed mid-flight.
#[must_use]
pub fn focus_window_and_bounds(id: i64) -> Option<Rect> {
    let hwnd = HWND(id as *mut c_void);
    // SAFETY: read-only checks; ShowWindow tolerates any HWND.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        // Best-effort: foreground-lock can refuse the call when our
        // picker lost foreground between opening and the click.
        if !SetForegroundWindow(hwnd).as_bool() {
            tracing::debug!(?id, "SetForegroundWindow returned FALSE");
        }
    }
    frame_bounds(hwnd)
}

#[must_use]
pub fn extract_window_icon(id: i64) -> Option<String> {
    let hwnd = HWND(id as *mut c_void);
    // SAFETY: every probe is a read-only Win32 query.
    let hicon = unsafe { resolve_hicon(hwnd) }?;
    let png = hicon_to_png(hicon)?;
    Some(png_to_data_url(&png))
}

// ─────── enumeration ───────

struct EnumCtx {
    out: *mut Vec<WindowInfo>,
    shell: HWND,
}

extern "system" fn enum_callback(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the &EnumCtx we passed; out lives on the stack
    // of the enumerate_capturable_windows call.
    let ctx = unsafe { &*(lparam.0 as *const EnumCtx) };
    let out = unsafe { &mut *ctx.out };
    if out.len() >= MAX_WINDOWS {
        return false.into();
    }
    if let Some(info) = inspect(hwnd, ctx.shell) {
        out.push(info);
    }
    TRUE
}

/// Apply Alt+Tab inclusion rules. Cheapest checks first.
fn inspect(hwnd: HWND, shell: HWND) -> Option<WindowInfo> {
    if hwnd == shell {
        return None;
    }

    // SAFETY: every call is a read-only query tolerating invalid HWND.
    let (is_app, is_tool, has_owner, iconic) = unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return None;
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        (
            (ex & WS_EX_APPWINDOW.0) != 0,
            (ex & WS_EX_TOOLWINDOW.0) != 0,
            !GetWindow(hwnd, GW_OWNER).unwrap_or_default().is_invalid(),
            IsIconic(hwnd).as_bool(),
        )
    };

    // Owned windows are dialogs/popups unless WS_EX_APPWINDOW opts in.
    if has_owner && !is_app {
        return None;
    }
    // Tool windows opt out of Alt+Tab unless WS_EX_APPWINDOW opts in.
    if is_tool && !is_app {
        return None;
    }
    if is_cloaked(hwnd) {
        return None;
    }
    let title = window_title(hwnd);
    if title.is_empty() {
        return None;
    }

    // Minimized → use the restored rect (taskbar-slot rect would be
    // (near-)zero). focus_window_and_bounds restores before capturing,
    // so this matches what the user will see.
    let (width, height) = if iconic { restored_bounds(hwnd) } else { frame_bounds(hwnd) }
        .map_or((0, 0), |r| (r.width, r.height));

    Some(WindowInfo {
        id: hwnd.0 as i64,
        title,
        width,
        height,
    })
}

fn is_cloaked(hwnd: HWND) -> bool {
    // Cloaked = UWP suspended / on another virtual desktop / not being
    // composited despite IsWindowVisible. Older Win10 builds without
    // the attribute → assume not cloaked (don't drop legitimate windows).
    dwm_attr::<u32>(hwnd, DWMWA_CLOAKED).is_some_and(|v| v != 0)
}

fn frame_bounds(hwnd: HWND) -> Option<Rect> {
    // EXTENDED_FRAME_BOUNDS is the rect DWM actually composites (no
    // invisible drop-shadow padding, unlike GetWindowRect).
    let rect = dwm_attr::<Win32Rect>(hwnd, DWMWA_EXTENDED_FRAME_BOUNDS)?;
    let width = (rect.right - rect.left).max(0) as u32;
    let height = (rect.bottom - rect.top).max(0) as u32;
    Some(Rect {
        x: rect.left,
        y: rect.top,
        width,
        height,
    })
}

fn restored_bounds(hwnd: HWND) -> Option<Rect> {
    let mut placement = WINDOWPLACEMENT {
        length: size_of::<WINDOWPLACEMENT>() as u32,
        ..Default::default()
    };
    // SAFETY: GetWindowPlacement writes `placement.length` bytes; safe
    // with an invalid HWND (returns Err).
    unsafe { GetWindowPlacement(hwnd, addr_of_mut!(placement)).ok()? };
    let r = placement.rcNormalPosition;
    let width = (r.right - r.left).max(0) as u32;
    let height = (r.bottom - r.top).max(0) as u32;
    if width == 0 || height == 0 {
        return None;
    }
    Some(Rect {
        x: r.left,
        y: r.top,
        width,
        height,
    })
}

fn dwm_attr<T: Copy + Default>(hwnd: HWND, attr: DWMWINDOWATTRIBUTE) -> Option<T> {
    let mut value = T::default();
    // SAFETY: out-param sized to T; DWM tolerates invalid HWND (returns Err).
    unsafe {
        DwmGetWindowAttribute(hwnd, attr, addr_of_mut!(value).cast::<c_void>(), size_of::<T>() as u32)
            .ok()?;
    }
    Some(value)
}

fn window_title(hwnd: HWND) -> String {
    // SAFETY: read-only length probe.
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len as usize) + 1];
    // SAFETY: buf has cap+1 u16s; we pass cap+1 in.
    let written = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if written <= 0 {
        String::new()
    } else {
        String::from_utf16_lossy(&buf[..written as usize])
    }
}

// ─────── icons ───────

/// HICONs from these APIs are owned by the window/class — we must NOT
/// DestroyIcon. The bitmaps GetIconInfo returns are caller-owned and
/// freed by `hicon_to_png`.
///
/// # Safety
/// hwnd may be invalid; every probe tolerates that.
unsafe fn resolve_hicon(hwnd: HWND) -> Option<HICON> {
    // Large-first — picker renders at 32×32; downsampling beats upsampling.
    for icon_type in [ICON_BIG, ICON_SMALL, ICON_SMALL2] {
        let mut result: usize = 0;
        // SAFETY: result is stack-local; SMTO_ABORTIFHUNG caps wait time.
        let _ = unsafe {
            SendMessageTimeoutW(
                hwnd,
                WM_GETICON,
                WPARAM(icon_type as usize),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                ICON_PROBE_TIMEOUT_MS,
                Some(addr_of_mut!(result)),
            )
        };
        if result != 0 {
            return Some(HICON(result as *mut c_void));
        }
    }
    // Class icon fallback.
    for class_attr in [GCLP_HICON, GCLP_HICONSM] {
        // SAFETY: read-only class long lookup; returns 0 on miss.
        let h = unsafe { GetClassLongPtrW(hwnd, class_attr) };
        if h != 0 {
            return Some(HICON(h as *mut c_void));
        }
    }
    None
}

fn hicon_to_png(hicon: HICON) -> Option<Vec<u8>> {
    let mut info = ICONINFO::default();
    // SAFETY: GetIconInfo fills caller-owned bitmap handles.
    if unsafe { GetIconInfo(hicon, addr_of_mut!(info)) }.is_err() {
        return None;
    }
    let rgba = read_bitmap_to_rgba(info.hbmColor);
    // SAFETY: bitmap handles are caller-owned per MSDN.
    unsafe {
        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
        }
    }
    let (rgba, w, h) = rgba?;
    encode_png(&rgba, w, h)
}

/// HBITMAP → straight-alpha RGBA via top-down DIB read + un-premultiply.
fn read_bitmap_to_rgba(hbm: HBITMAP) -> Option<(Vec<u8>, u32, u32)> {
    if hbm.is_invalid() {
        return None;
    }
    let mut bm = BITMAP::default();
    // SAFETY: GetObjectW writes size_of::<BITMAP>() bytes for an HBITMAP.
    let n = unsafe {
        GetObjectW(
            HGDIOBJ(hbm.0),
            size_of::<BITMAP>() as i32,
            Some(addr_of_mut!(bm).cast::<c_void>()),
        )
    };
    if n == 0 {
        return None;
    }
    let w = bm.bmWidth as u32;
    let h = bm.bmHeight.unsigned_abs();
    if w == 0 || h == 0 {
        return None;
    }

    let mut bgra: Vec<u8> = vec![0u8; (w * h * 4) as usize];
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w as i32,
            // Negative = top-down so PNG encoder consumes directly.
            biHeight: -(h as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: CreateCompatibleDC(None) → screen-compat DC released below.
    let dc = unsafe { CreateCompatibleDC(None) };
    if dc.is_invalid() {
        return None;
    }
    // SAFETY: bgra sized exactly width*height*4; bmi is 32-bpp top-down.
    let lines = unsafe {
        GetDIBits(
            dc,
            hbm,
            0,
            h,
            Some(bgra.as_mut_ptr().cast::<c_void>()),
            addr_of_mut!(bmi),
            DIB_RGB_COLORS,
        )
    };
    // SAFETY: pair the CreateCompatibleDC.
    let _ = unsafe { DeleteDC(dc) };
    if lines == 0 {
        return None;
    }

    // BGRA premultiplied → straight RGBA. Channel-swap + un-premul in
    // one pass. Integer division avoids per-pixel float.
    for px in bgra.chunks_exact_mut(4) {
        px.swap(0, 2);
        let a = px[3];
        if a > 0 && a < 255 {
            let a32 = u32::from(a);
            px[0] = (u32::from(px[0]) * 255 / a32).min(255) as u8;
            px[1] = (u32::from(px[1]) * 255 / a32).min(255) as u8;
            px[2] = (u32::from(px[2]) * 255 / a32).min(255) as u8;
        }
    }
    Some((bgra, w, h))
}

fn encode_png(rgba: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    use image::ExtendedColorType;
    use image::ImageEncoder;
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};
    // NoFilter — icon-scale per-byte savings don't justify encoder time.
    let mut out = Vec::with_capacity(1024);
    PngEncoder::new_with_quality(&mut out, CompressionType::Default, FilterType::NoFilter)
        .write_image(rgba, w, h, ExtendedColorType::Rgba8)
        .ok()?;
    Some(out)
}

/// Inline base64 PNG → `data:` URL. No dep; microseconds at icon scale.
fn png_to_data_url(png: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    const PREFIX: &str = "data:image/png;base64,";

    let mut out = String::with_capacity(PREFIX.len() + png.len().div_ceil(3) * 4);
    out.push_str(PREFIX);

    let chunks = png.chunks_exact(3);
    let remainder = chunks.remainder();
    for c in chunks {
        let v = (u32::from(c[0]) << 16) | (u32::from(c[1]) << 8) | u32::from(c[2]);
        for shift in [18, 12, 6, 0] {
            out.push(char::from(ALPHABET[((v >> shift) & 0x3f) as usize]));
        }
    }
    match remainder {
        [b0] => {
            let v = u32::from(*b0) << 16;
            out.push(char::from(ALPHABET[((v >> 18) & 0x3f) as usize]));
            out.push(char::from(ALPHABET[((v >> 12) & 0x3f) as usize]));
            out.push_str("==");
        }
        [b0, b1] => {
            let v = (u32::from(*b0) << 16) | (u32::from(*b1) << 8);
            for shift in [18, 12, 6] {
                out.push(char::from(ALPHABET[((v >> shift) & 0x3f) as usize]));
            }
            out.push('=');
        }
        _ => {}
    }
    out
}
