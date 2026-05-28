//! Composite the system cursor + click rings onto a captured BGRA frame.
//!
//! DXGI hands us the desktop without the cursor; we paint it via GDI
//! `DrawIconEx` through a same-thread DIB section. Click rings ride the
//! same DC.
//!
//! Click detection uses `GetAsyncKeyState`, NOT `WH_MOUSE_LL` — the
//! low-level hook adds 300 ms unhook timeout latency to every system
//! click. `GetAsyncKeyState` is a userspace cache read (~10 ns), no
//! syscall, no impact on the apps being recorded.
//!
//! Thread affinity is critical — GDI DCs are bound to the creating
//! thread, so `CursorOverlay` is `pub(crate)` and constructed inside
//! the capture loop, never sent across threads.
//!
//! Per-frame cost ~250 µs worst case (cursor draw ~50 µs + up to 8
//! click rings at ~50 µs each) = ~1.5% of the 60 fps budget.

#![allow(
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::suboptimal_flops
)]

use std::ffi::c_void;
use std::ptr;
use std::time::Instant;

use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, CreatePen,
    DIB_RGB_COLORS, DeleteDC, DeleteObject, Ellipse, GetDC, GetStockObject, HBITMAP, HDC, HGDIOBJ,
    NULL_BRUSH, PS_SOLID, ReleaseDC, SelectObject,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo,
};

// Square bbox around the cursor that round-trips through the DIB. 96
// covers 32x32 stock plus moderate accessibility scaling.
const CURSOR_BBOX: i32 = 96;

const CURSORINFO_SIZE: u32 = size_of::<CURSORINFO>() as u32;
const BITMAPINFOHEADER_SIZE: u32 = size_of::<BITMAPINFOHEADER>() as u32;

const RING_DURATION_MS: u128 = 500;
const RING_START_RADIUS: f32 = 10.0;
const RING_END_RADIUS: f32 = 40.0;
const RING_START_STROKE: f32 = 3.0;
// BGR packed: #7C4DFF (Clipo accent purple).
const RING_COLOR: u32 = 0x00FF_4D7C;
const MAX_RINGS: usize = 8;
const MOUSE_VKS: [u16; 3] = [VK_LBUTTON.0, VK_RBUTTON.0, VK_MBUTTON.0];

#[derive(Clone, Copy)]
struct ClickRing {
    x: i32,
    y: i32,
    t0: Instant,
}

pub(crate) struct CursorOverlay {
    dc: HDC,
    bitmap: HBITMAP,
    bits: *mut u8,
    width: i32,
    height: i32,
    show_cursor: bool,
    show_clicks: bool,
    rings: [Option<ClickRing>; MAX_RINGS],
    last_buttons: [bool; 3],
}

impl CursorOverlay {
    pub(crate) fn new(
        width: u32,
        height: u32,
        show_cursor: bool,
        show_clicks: bool,
    ) -> windows::core::Result<Self> {
        // SAFETY: GDI bring-up; matched Delete* in Drop on this thread.
        unsafe {
            let screen_dc = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen_dc));
            ReleaseDC(None, screen_dc);

            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: BITMAPINFOHEADER_SIZE,
                    biWidth: width as i32,
                    // Negative = top-down, matching DXGI's row 0 = top.
                    biHeight: -(height as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits_ptr: *mut c_void = ptr::null_mut();
            let bitmap = CreateDIBSection(
                Some(dc),
                &raw const info,
                DIB_RGB_COLORS,
                &raw mut bits_ptr,
                None,
                0,
            )?;
            SelectObject(dc, HGDIOBJ(bitmap.0));

            // Snapshot already-held buttons so first-frame rising-edge
            // check doesn't emit a phantom ring.
            let last_buttons = if show_clicks {
                [
                    is_button_down(MOUSE_VKS[0]),
                    is_button_down(MOUSE_VKS[1]),
                    is_button_down(MOUSE_VKS[2]),
                ]
            } else {
                [false; 3]
            };

            Ok(Self {
                dc,
                bitmap,
                bits: bits_ptr.cast(),
                width: width as i32,
                height: height as i32,
                show_cursor,
                show_clicks,
                rings: [None; MAX_RINGS],
                last_buttons,
            })
        }
    }

    /// Composite onto `frame` (BGRA, `width × height`) at the cursor's
    /// screen position translated by (`crop_x`, `crop_y`). Rings paint
    /// first, cursor on top.
    pub(crate) fn compose(&mut self, frame: &mut [u8], crop_x: u32, crop_y: u32) {
        if self.show_clicks {
            self.poll_clicks();
            self.draw_rings(frame, crop_x, crop_y);
        }
        if self.show_cursor {
            self.draw_cursor(frame, crop_x, crop_y);
        }
    }

    fn poll_clicks(&mut self) {
        let mut new_click = false;
        for (i, &vk) in MOUSE_VKS.iter().enumerate() {
            let down = is_button_down(vk);
            if down && !self.last_buttons[i] {
                new_click = true;
            }
            self.last_buttons[i] = down;
        }
        if !new_click {
            return;
        }
        let mut ci = CURSORINFO {
            cbSize: CURSORINFO_SIZE,
            ..Default::default()
        };
        // SAFETY: read-only.
        if unsafe { GetCursorInfo(&raw mut ci) }.is_err() {
            return;
        }
        let pos = ci.ptScreenPos;
        // First-free-slot. Dropping under sustained 16+ clicks/sec is
        // the lesser evil vs disrupting in-flight fades.
        if let Some(slot) = self.rings.iter_mut().find(|r| r.is_none()) {
            *slot = Some(ClickRing {
                x: pos.x,
                y: pos.y,
                t0: Instant::now(),
            });
        }
    }

    fn draw_rings(&mut self, frame: &mut [u8], crop_x: u32, crop_y: u32) {
        let now = Instant::now();
        let (width, height, dc, bits) = (self.width, self.height, self.dc, self.bits);
        let stride = (width as usize) * 4;

        for slot in &mut self.rings {
            let Some(ring) = slot else { continue };
            let age_ms = now.duration_since(ring.t0).as_millis();
            if age_ms >= RING_DURATION_MS {
                *slot = None;
                continue;
            }
            let progress = (age_ms as f32) / (RING_DURATION_MS as f32);
            let radius =
                (RING_START_RADIUS + (RING_END_RADIUS - RING_START_RADIUS) * progress) as i32;
            // Stroke shrink approximates an alpha fade; GDI solid pens
            // have no alpha. Floor at 1: <1 px renders as 1 anyway.
            let stroke = ((1.0 - progress) * RING_START_STROKE).max(1.0) as i32;

            let local_x = ring.x - crop_x as i32;
            let local_y = ring.y - crop_y as i32;
            let pad = radius + stroke;
            let Some((bx, by, bw, bh)) =
                clip_to_frame(width, height, local_x - pad, local_y - pad, pad * 2, pad * 2)
            else {
                continue;
            };

            // SAFETY: frame and DIB share `width * height` BGRA layout;
            // pen lifetime bracketed by Select/Delete.
            unsafe {
                copy_bbox(stride, frame.as_ptr(), bits, bx, by, bw, bh);

                let pen = CreatePen(PS_SOLID, stroke, COLORREF(RING_COLOR));
                let old_pen = SelectObject(dc, HGDIOBJ(pen.0));
                let old_brush = SelectObject(dc, GetStockObject(NULL_BRUSH));
                let _ = Ellipse(
                    dc,
                    local_x - radius,
                    local_y - radius,
                    local_x + radius,
                    local_y + radius,
                );
                SelectObject(dc, old_brush);
                SelectObject(dc, old_pen);
                let _ = DeleteObject(HGDIOBJ(pen.0));

                copy_bbox(stride, bits, frame.as_mut_ptr(), bx, by, bw, bh);
            }
        }
    }

    fn draw_cursor(&self, frame: &mut [u8], crop_x: u32, crop_y: u32) {
        let mut ci = CURSORINFO {
            cbSize: CURSORINFO_SIZE,
            ..Default::default()
        };
        // SAFETY: read-only.
        if unsafe { GetCursorInfo(&raw mut ci) }.is_err()
            || (ci.flags.0 & CURSOR_SHOWING.0) == 0
            || ci.hCursor.is_invalid()
        {
            return;
        }
        let local_x = ci.ptScreenPos.x - crop_x as i32;
        let local_y = ci.ptScreenPos.y - crop_y as i32;
        let Some((bx, by, bw, bh)) =
            clip_to_frame(self.width, self.height, local_x, local_y, CURSOR_BBOX, CURSOR_BBOX)
        else {
            return;
        };
        let stride = (self.width as usize) * 4;

        // SAFETY: same invariants as draw_rings; DrawIconEx accounts
        // for the cursor hotspot internally.
        unsafe {
            copy_bbox(stride, frame.as_ptr(), self.bits, bx, by, bw, bh);
            let _ = DrawIconEx(
                self.dc,
                local_x,
                local_y,
                ci.hCursor.into(),
                0,
                0,
                0,
                None,
                DI_NORMAL,
            );
            copy_bbox(stride, self.bits, frame.as_mut_ptr(), bx, by, bw, bh);
        }
    }
}

/// Clip a rectangle to `(0..width, 0..height)`. `None` if entirely outside.
fn clip_to_frame(
    width: i32,
    height: i32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) -> Option<(i32, i32, i32, i32)> {
    let x0 = x.max(0);
    let y0 = y.max(0);
    let x1 = (x + w).min(width);
    let y1 = (y + h).min(height);
    let cw = x1 - x0;
    let ch = y1 - y0;
    (cw > 0 && ch > 0).then_some((x0, y0, cw, ch))
}

/// # Safety
/// `src` and `dst` must both span `≥ stride * (y + h)` BGRA bytes.
/// `(x, y, w, h)` must already be clipped to the frame bounds.
unsafe fn copy_bbox(
    stride: usize,
    src: *const u8,
    dst: *mut u8,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    let row_bytes = (w as usize) * 4;
    unsafe {
        for row in 0..h {
            let offset = ((y + row) as usize) * stride + (x as usize) * 4;
            ptr::copy_nonoverlapping(src.add(offset), dst.add(offset), row_bytes);
        }
    }
}

impl Drop for CursorOverlay {
    fn drop(&mut self) {
        // SAFETY: handles created in `new` on this same thread.
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

fn is_button_down(vk: u16) -> bool {
    // SAFETY: GetAsyncKeyState is a pure user-mode cache read.
    let state = unsafe { GetAsyncKeyState(i32::from(vk)) };
    (state as u16 & 0x8000) != 0
}
