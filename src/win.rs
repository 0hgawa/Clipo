//! Win32 window helpers: HWND lookup, monitor / work-area math, placement, and
//! the `show_*` entry points. All the `SetWindowPos` / `MonitorFromWindow` / DWM
//! glue lives here so the surfaces don't each re-derive coordinate spaces (mixing
//! Slint's logical coords with physical metrics is what threw placement off).
use std::time::Duration;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HINSTANCE, HWND, POINT, RECT, SIZE, TRUE};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_TRANSITIONS_FORCEDISABLED};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CalculatePopupWindowPosition, GetSystemMetrics, GetWindowRect, IsIconic, IsWindowVisible,
    LoadImageW, SetClassLongPtrW, SetForegroundWindow, SetWindowPos, ShowWindow, GCLP_HICON,
    GCLP_HICONSM, HWND_NOTOPMOST, HWND_TOP, HWND_TOPMOST, IMAGE_ICON, LR_DEFAULTCOLOR, LR_SHARED,
    SM_CXICON, SM_CXSMICON, SM_CYICON, SM_CYSMICON, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER,
    SWP_SHOWWINDOW, SW_RESTORE, TPM_BOTTOMALIGN, TPM_RIGHTALIGN, TPM_WORKAREA,
};

/// The HWND behind a Slint window, via the raw-window-handle bridge. None until
/// the window has been shown (no native handle yet).
pub fn hwnd_of(win: &slint::Window) -> Option<HWND> {
    let slint_handle = win.window_handle();
    let handle = slint_handle.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(raw) => Some(HWND(raw.hwnd.get() as *mut core::ffi::c_void)),
        _ => None,
    }
}

/// Install the brand icon on winit's shared window class — once, the first time
/// any window is shown. winit registers its class with no icon, so every window
/// otherwise falls back to the generic Windows placeholder in the taskbar and
/// alt-tab; setting the class icon fixes all of them at once (and any opened
/// later, since icon-less windows resolve through the class). The frames come
/// from the .ico embedded in the exe (app.rc, resource id 1) at the sizes the
/// shell asks for, so nothing is read from disk.
fn ensure_app_icon(hwnd: HWND) {
    thread_local! { static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) }; }
    if DONE.with(std::cell::Cell::get) {
        return;
    }
    DONE.with(|d| d.set(true));
    // SAFETY: the module handle is this exe; LoadImageW reads its own embedded
    // icon resource and SetClassLongPtrW sets it on `hwnd`'s (winit) class.
    unsafe {
        let Ok(hmod) = GetModuleHandleW(None) else { return };
        let hinst = HINSTANCE(hmod.0);
        let load = |cx, cy| {
            LoadImageW(Some(hinst), PCWSTR(1 as _), IMAGE_ICON, cx, cy, LR_DEFAULTCOLOR | LR_SHARED)
                .ok()
        };
        if let Some(big) = load(GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON)) {
            SetClassLongPtrW(hwnd, GCLP_HICON, big.0 as isize);
        }
        if let Some(small) = load(GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) {
            SetClassLongPtrW(hwnd, GCLP_HICONSM, small.0 as isize);
        }
    }
}

/// Normalise a freshly-shown window: install the app icon (first show only) and
/// kill the DWM show/hide fade. Without the latter the fade-out is still
/// mid-animation when we grab the screen and the window bleeds into the shot
/// (semi-transparent ghost). Every show path runs through here.
pub fn disable_dwm_transitions(win: &slint::Window) {
    let Some(hwnd) = hwnd_of(win) else { return };
    ensure_app_icon(hwnd);
    let yes: BOOL = TRUE;
    // SAFETY: `hwnd` is valid (from hwnd_of); the attribute pointer is a live
    // BOOL of the size we pass.
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            std::ptr::addr_of!(yes).cast(),
            core::mem::size_of::<BOOL>() as u32,
        );
    }
}

/// The window's monitor info (work area + full bounds). `primary` picks the
/// primary monitor instead of the nearest. The one place the MonitorFromWindow +
/// GetMonitorInfo dance lives, so the placement helpers don't each repeat it.
fn monitor_info(hwnd: HWND, primary: bool) -> Option<MONITORINFO> {
    let flag = if primary {
        MONITOR_DEFAULTTOPRIMARY
    } else {
        MONITOR_DEFAULTTONEAREST
    };
    // SAFETY: `hwnd` is valid; `mi` is a live MONITORINFO with cbSize set, as
    // GetMonitorInfoW requires.
    unsafe {
        let mut mi = MONITORINFO {
            cbSize: core::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(MonitorFromWindow(hwnd, flag), &raw mut mi)
            .as_bool()
            .then_some(mi)
    }
}

/// Move a (physical) pw×ph window to the centre of work area `a`, on top.
fn center_in(hwnd: HWND, a: RECT, pw: i32, ph: i32) {
    let x = a.left + (a.right - a.left - pw) / 2;
    let y = a.top + (a.bottom - a.top - ph) / 2;
    // SAFETY: `hwnd` is valid; SetWindowPos only moves the window.
    unsafe {
        let _ = SetWindowPos(hwnd, Some(HWND_TOP), x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER);
    }
}

/// Park a window at the bottom-right of the monitor work area (a small margin
/// in), on top. Used by the post-capture toast. The window carries its own
/// transparent shadow margin, so the visible card sits a touch further in.
fn place_bottom_right(win: &slint::Window) {
    let Some(hwnd) = hwnd_of(win) else { return };
    let s = win.size();
    if s.width == 0 || s.height == 0 {
        return;
    }
    let Some(mi) = monitor_info(hwnd, false) else { return };
    let a = mi.rcWork;
    let m = (8.0 * win.scale_factor()).round() as i32;
    // SAFETY: `hwnd` is valid; SetWindowPos only moves the window.
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            a.right - s.width as i32 - m,
            a.bottom - s.height as i32 - m,
            0,
            0,
            // Real topmost (no SWP_NOZORDER, which would ignore HWND_TOPMOST) so
            // the toast always lands above other windows — it's shown
            // programmatically after a capture, so Windows won't foreground it on
            // its own. NOACTIVATE keeps it from stealing keyboard focus.
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Place a small popup window so its bottom-right sits at the cursor (physical
/// px), clamped to the cursor's monitor work area, topmost + focused. Used by
/// the tray menu — a small popup instead of a fullscreen overlay so it never
/// disrupts hardware video planes (e.g. a YouTube video) behind it.
pub fn place_at_cursor_topmost(win: &slint::Window, cx: i32, cy: i32) {
    let Some(hwnd) = hwnd_of(win) else { return };
    // SAFETY: `hwnd` is valid; the POINT/SIZE/RECT params are live stack locals;
    // the calls only query the window size, compute a position, then reposition.
    unsafe {
        let mut wr = RECT::default();
        if GetWindowRect(hwnd, &raw mut wr).is_err() {
            return;
        }
        let size = SIZE { cx: wr.right - wr.left, cy: wr.bottom - wr.top };
        let anchor = POINT { x: cx, y: cy };
        // The same math the OS uses for native menus: anchor the menu's bottom-
        // right edge at the cursor (so it opens up-left, the tray default), then
        // flip toward whichever edge has room and keep it inside the work area
        // (TPM_WORKAREA). Multi-monitor aware — it picks the monitor under the
        // anchor. Falls back to the plain up-left offset if the call fails.
        let mut pos = RECT::default();
        let flags = (TPM_RIGHTALIGN | TPM_BOTTOMALIGN | TPM_WORKAREA).0;
        let (x, y) = if CalculatePopupWindowPosition(&raw const anchor, &raw const size, flags, None, &raw mut pos).is_ok() {
            (pos.left, pos.top)
        } else {
            (cx - size.cx, cy - size.cy)
        };
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, 0, 0, SWP_NOSIZE | SWP_SHOWWINDOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

/// Park a window centred just above the taskbar (bottom-centre of the work
/// area). Used by the recording bar.
pub fn place_bottom_center(win: &slint::Window) -> bool {
    let Some(hwnd) = hwnd_of(win) else { return false };
    let s = win.size();
    if s.width == 0 || s.height == 0 {
        return false;
    }
    let Some(mi) = monitor_info(hwnd, true) else { return false };
    let r = mi.rcWork;
    let x = r.left + (r.right - r.left - s.width as i32) / 2;
    // Just above the taskbar (rcWork excludes it); the bar carries its own 16px
    // shadow margin, so the visible card clears the edge.
    let y = r.bottom - s.height as i32 - 16;
    // SAFETY: `hwnd` is valid; SetWindowPos only moves the window.
    unsafe {
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, 0, 0, SWP_NOSIZE);
    }
    true
}

/// Cover the monitor work area (taskbar still visible), no topmost.
// Sizes a borderless window to the monitor work area (taskbar excluded) using
// Slint's PhysicalSize/Position (DPI-correct) rather than a raw SetWindowPos
// (set_maximized would cover the taskbar on a no-frame window). The target
// window must use preferred-width/height, not fixed width/height — a fixed
// size binding would be reasserted and this resize ignored.
pub fn maximize_work_area(win: &slint::Window) -> bool {
    let Some(hwnd) = hwnd_of(win) else { return false };
    let Some(mi) = monitor_info(hwnd, true) else { return false };
    let r = mi.rcWork;
    win.set_position(slint::PhysicalPosition::new(r.left, r.top));
    win.set_size(slint::PhysicalSize::new(
        (r.right - r.left) as u32,
        (r.bottom - r.top) as u32,
    ));
    true
}

/// Restore (if minimized) and force `win` above the current foreground. Used
/// for "Open with Clipo" / forwarded launches: that request comes from the
/// background tray process, so `SetForegroundWindow` alone is ignored and the
/// viewer stays behind (or stuck minimized). The brief TOPMOST flip beats that
/// guard, then drops back so the window isn't pinned over everything.
pub fn raise_to_front(win: &slint::Window) {
    let Some(hwnd) = hwnd_of(win) else { return };
    bring_to_front(hwnd); // restore if minimized + SetForegroundWindow
    // SAFETY: `hwnd` is valid; SWP only reorders the window (no move/resize).
    unsafe {
        let raise = SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW;
        let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, raise);
        let _ = SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, raise);
    }
}

/// Restore a maximized window to a centered logical size (the inverse of
/// maximize_work_area). Logical size keeps it DPI-correct; the centre is
/// computed in physical pixels from the scale factor.
pub fn restore_window(win: &slint::Window, w: f32, h: f32) {
    win.set_size(slint::LogicalSize::new(w, h));
    let Some(hwnd) = hwnd_of(win) else { return };
    let Some(mi) = monitor_info(hwnd, false) else { return };
    let scale = win.scale_factor();
    center_in(hwnd, mi.rcWork, (w * scale) as i32, (h * scale) as i32);
}

/// Show a window at the given logical size, centred on its monitor. Sized via
/// set_size (Slint ignores a resizable window's `preferred-width`) and centred
/// from that KNOWN size — not `win.size()`, which on the first show still
/// reports a stale/default size, so centring off it lands the window in the
/// wrong spot until the next open. Computing from the size we set fixes that.
pub fn show_centered<T: ComponentHandle + 'static>(c: &T, w: f32, h: f32) {
    let fresh = is_fresh(c.window());
    let _ = c.show();
    let win = c.window();
    disable_dwm_transitions(win);
    let Some(hwnd) = hwnd_of(win) else { return };
    // Size + centre only on a fresh open; an already-open window keeps its place.
    if fresh {
        win.set_size(slint::LogicalSize::new(w, h));
        if let Some(mi) = monitor_info(hwnd, false) {
            let scale = win.scale_factor();
            center_in(hwnd, mi.rcWork, (w * scale) as i32, (h * scale) as i32);
        }
    }
    bring_to_front(hwnd);
}

/// Show a window restoring its remembered state: `maximized` fills the work area,
/// otherwise it opens at `w`×`h` centred. Pair with `set_is_maximized` so the
/// titlebar button matches. The maximize retries once on a zero-delay Timer for
/// the case the first call lands before layout settles.
pub fn show_remembered<T: ComponentHandle + 'static>(c: &T, w: f32, h: f32, maximized: bool) {
    if !maximized {
        show_centered(c, w, h);
        return;
    }
    let _ = c.show();
    let win = c.window();
    disable_dwm_transitions(win);
    if !maximize_work_area(win) {
        let wk = c.as_weak();
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            if let Some(c) = wk.upgrade() {
                disable_dwm_transitions(c.window());
                maximize_work_area(c.window());
            }
        });
    }
    if let Some(hwnd) = hwnd_of(win) {
        bring_to_front(hwnd);
    }
}

/// A window is "fresh" (needs sizing + centring) when it's hidden or was never
/// shown. A minimized or already-visible one is not fresh: just restore + raise
/// it where it is — and skip the placement maths.
fn is_fresh(win: &slint::Window) -> bool {
    // SAFETY: `h` is a live window handle from hwnd_of; IsWindowVisible only reads.
    hwnd_of(win).is_none_or(|h| unsafe { !IsWindowVisible(h).as_bool() })
}

/// Restore (un-minimize) + raise + focus, so invoking a menu item for an
/// already-open window always brings it to the front instead of doing nothing.
fn bring_to_front(hwnd: HWND) {
    // SAFETY: `hwnd` is a live window handle; these only restore/activate it.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
    }
}

pub fn show_bottom_right<T: ComponentHandle + 'static>(c: &T) {
    let _ = c.show();
    disable_dwm_transitions(c.window());
    // Place now for the common (no-flash) case…
    place_bottom_right(c.window());
    // …then once more after the size/DPI scale has settled. Right after show() —
    // most visibly when the foreground just changed (window capture) — win.size()
    // can briefly report the window's *pre-scale* height, so the immediate place
    // computes `work_bottom - small_height` and the card lands behind the taskbar.
    // An unconditional deferred re-place (size now settled) snaps it back to the
    // corner; it's a no-op when the first place already hit the right spot. This
    // also covers the first-ever show, where the size is momentarily 0.
    let weak = c.as_weak();
    slint::Timer::single_shot(Duration::from_millis(50), move || {
        if let Some(c) = weak.upgrade() {
            disable_dwm_transitions(c.window());
            place_bottom_right(c.window());
        }
    });
}

/// Show a 16:10 window, as large as fits within 68% of the work area but no
/// wider than max_w (logical), centred. Forces the size via set_size (the
/// editor's approach) since Slint ignores a resizable window's `preferred-width`,
/// and derives height from width so the ratio is fixed regardless of monitor.
pub fn show_fitted<T: ComponentHandle + 'static>(c: &T, max_w: f32) {
    let fresh = is_fresh(c.window());
    let _ = c.show();
    let win = c.window();
    disable_dwm_transitions(win);
    let Some(hwnd) = hwnd_of(win) else { return };
    // Fit + centre only on a fresh open; reopening keeps the window's place/size.
    if fresh {
        if let Some(mi) = monitor_info(hwnd, false) {
            let a = mi.rcWork;
            let scale = win.scale_factor();
            let avail_w = (a.right - a.left) as f32 / scale * 0.68;
            let avail_h = (a.bottom - a.top) as f32 / scale * 0.68;
            // Largest 16:10 box within the available width/height, capped at max_w.
            let mut w = max_w.min(avail_w);
            let mut h = w * 10.0 / 16.0;
            if h > avail_h {
                h = avail_h;
                w = h * 16.0 / 10.0;
            }
            win.set_size(slint::LogicalSize::new(w, h));
            center_in(hwnd, a, (w * scale) as i32, (h * scale) as i32);
        }
    }
    bring_to_front(hwnd);
}
