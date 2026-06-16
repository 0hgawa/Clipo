//! Annotation editor: image compositing (background/shadow/blur/crop/resize),
//! shape geometry + hit-testing, glyph rasterisation, and editor-window wiring.
use crate::{show_remembered, to_slint_image, EditorWindow};
use clipo_core::CapturedImage;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

/// One annotation in image-pixel coords. kind: 0 arrow · 1 rectangle.
#[derive(Clone)]
pub struct Shape {
    pub(crate) kind: i32,
    pub(crate) x1: f32,
    pub(crate) y1: f32,
    pub(crate) x2: f32,
    pub(crate) y2: f32,
    pub(crate) r: u8,
    pub(crate) g: u8,
    pub(crate) b: u8,
    pub(crate) width: f32,
    pub(crate) filled: bool,
    pub(crate) text: String,
    pub(crate) points: Vec<f32>, // freehand polyline (flattened x,y, screenshot px); kind 10 only
    pub(crate) font: i32,        // text family (0 sans·1 serif·2 mono·3 impact); kind 6 only
}

/// Parse a `#rrggbb` / `#rgb` hex string into RGB.
pub fn parse_hex(s: &str) -> Option<(u8, u8, u8)> {
    let s = s.trim().trim_start_matches('#');
    let h = |a: usize, b: usize| u8::from_str_radix(&s[a..b], 16).ok();
    match s.len() {
        6 => Some((h(0, 2)?, h(2, 4)?, h(4, 6)?)),
        3 => {
            let d = |c: usize| u8::from_str_radix(&s[c..=c], 16).ok().map(|v| v * 17);
            Some((d(0)?, d(1)?, d(2)?))
        }
        _ => None,
    }
}

// Background preset ids (canonical order).
pub const BG_NONE: i32 = 0;
pub const BG_BLUR: i32 = 8;
pub const BG_CUSTOM: i32 = 9; // a user-picked solid colour (the "+" swatch)

/// What a preset paints behind the card.
pub enum BgFill {
    Solid([u8; 3]),
    Gradient([u8; 3], [u8; 3]),
    Blur,
}

/// Resolve a preset id → its backdrop fill (RGB). Palette:
/// slate / snow solids, ocean / mint / sunset / aurora / ember gradients, blur.
pub const fn bg_fill(preset: i32, custom: (u8, u8, u8)) -> BgFill {
    match preset {
        1 => BgFill::Solid([15, 23, 42]),                 // slate  #0f172a
        2 => BgFill::Solid([248, 250, 252]),              // snow   #f8fafc
        3 => BgFill::Gradient([56, 189, 248], [29, 78, 216]),   // ocean
        4 => BgFill::Gradient([52, 211, 153], [6, 182, 212]),   // mint
        5 => BgFill::Gradient([251, 146, 60], [236, 72, 153]),  // sunset
        6 => BgFill::Gradient([168, 85, 247], [37, 99, 235]),   // aurora
        7 => BgFill::Gradient([244, 63, 94], [124, 45, 18]),    // ember
        BG_BLUR => BgFill::Blur,
        _ => BgFill::Solid([custom.0, custom.1, custom.2]),     // custom solid
    }
}

/// RGB → HSV with hue normalised to 0..1 (matches the Slint picker's bg-hue).
// Exact float compares are intended here: they pick the max channel / detect a
// fully-grey pixel, which is the standard HSV branch logic.
#[allow(clippy::float_cmp)]
pub fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let (rf, gf, bf) = (f32::from(r) / 255.0, f32::from(g) / 255.0, f32::from(b) / 255.0);
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let d = max - min;
    let mut h = if d == 0.0 {
        0.0
    } else if max == rf {
        ((gf - bf) / d).rem_euclid(6.0)
    } else if max == gf {
        (bf - rf) / d + 2.0
    } else {
        (rf - gf) / d + 4.0
    } / 6.0;
    if h < 0.0 {
        h += 1.0;
    }
    let s = if max == 0.0 { 0.0 } else { d / max };
    (h, s, max)
}

/// Apply a solid backdrop colour (the custom "+" swatch + picker). `sync_hsv`
/// also moves the picker cursor (palette/hex picks); the HSV drag passes false
/// since it owns hue/sat/val.
pub fn apply_solid(st: &mut EditorState, e: &EditorWindow, r: u8, g: u8, b: u8, sync_hsv: bool) {
    st.bg_custom = (r, g, b);
    st.bg_preset = BG_CUSTOM;
    let hex = format!("#{r:02X}{g:02X}{b:02X}");
    e.set_bg_on(true);
    e.set_bg_preset(BG_CUSTOM);
    e.set_bg_color(slint::Color::from_rgb_u8(r, g, b));
    e.set_bg_hex(hex.into());
    if sync_hsv {
        let (h, s, v) = rgb_to_hsv(r, g, b);
        e.set_bg_hue(h);
        e.set_bg_sat(s);
        e.set_bg_val(v);
    }
}

/// Whether pixel (x, y) is inside a w×h rounded rectangle of corner radius `r`.
pub fn inside_rounded(x: u32, y: u32, w: u32, h: u32, r: f32) -> bool {
    let r = r.max(0.0);
    let (xf, yf) = (x as f32 + 0.5, y as f32 + 0.5);
    let (wf, hf) = (w as f32, h as f32);
    let cx = xf.clamp(r, (wf - r).max(r));
    let cy = yf.clamp(r, (hf - r).max(r));
    (xf - cx).hypot(yf - cy) <= r
}

/// Separable box blur of one interleaved channel (`stride` bytes/pixel, channel
/// at `off`). O(n) sliding window; `passes` box passes ≈ a gaussian. Used for
/// the soft shadow (alpha mask, stride 1) and the blur backdrop (BGR, stride 4).
pub fn box_blur_1ch(buf: &mut [u8], w: u32, h: u32, stride: usize, off: usize, radius: i32, passes: u32) {
    if radius <= 0 {
        return;
    }
    let (wi, hi) = (w as i32, h as i32);
    let win = 2 * radius + 1;
    let at = |x: i32, y: i32| (y as usize * w as usize + x as usize) * stride + off;
    for _ in 0..passes {
        let src = buf.to_vec();
        for y in 0..hi {
            let mut sum = 0i32;
            for k in -radius..=radius {
                sum += i32::from(src[at(k.clamp(0, wi - 1), y)]);
            }
            for x in 0..wi {
                buf[at(x, y)] = (sum / win) as u8;
                sum += i32::from(src[at((x + radius + 1).clamp(0, wi - 1), y)])
                    - i32::from(src[at((x - radius).clamp(0, wi - 1), y)]);
            }
        }
        let src = buf.to_vec();
        for x in 0..wi {
            let mut sum = 0i32;
            for k in -radius..=radius {
                sum += i32::from(src[at(x, k.clamp(0, hi - 1))]);
            }
            for y in 0..hi {
                buf[at(x, y)] = (sum / win) as u8;
                sum += i32::from(src[at(x, (y + radius + 1).clamp(0, hi - 1))])
                    - i32::from(src[at(x, (y - radius).clamp(0, hi - 1))]);
            }
        }
    }
}

/// Cover-fit `src` into `cw × ch` (preserve aspect, centre-crop). Nearest —
/// it feeds the blur backdrop, which is itself blurred, so sampling is moot.
pub fn resample_cover(src: &CapturedImage, cw: u32, ch: u32) -> Vec<u8> {
    let scale = (cw as f32 / src.width as f32).max(ch as f32 / src.height as f32);
    let ox = (src.width as f32).mul_add(-scale, cw as f32) / 2.0;
    let oy = (src.height as f32).mul_add(-scale, ch as f32) / 2.0;
    let mut out = vec![0u8; (cw * ch * 4) as usize];
    for y in 0..ch {
        for x in 0..cw {
            let sx = (((x as f32 - ox) / scale) as i32).clamp(0, src.width as i32 - 1) as u32;
            let sy = (((y as f32 - oy) / scale) as i32).clamp(0, src.height as i32 - 1) as u32;
            let si = ((sy * src.width + sx) * 4) as usize;
            let di = ((y * cw + x) * 4) as usize;
            out[di..di + 4].copy_from_slice(&src.bgra[si..si + 4]);
            out[di + 3] = 255;
        }
    }
    out
}

/// A heavily-blurred, slightly-darkened miniature of `base` for the blur
/// backdrop. Downsample → blur → darken (cheap gaussian trick). Cached by the
/// caller; rebuilt only when the base image changes.
pub fn bg_blur_miniature(base: &CapturedImage) -> CapturedImage {
    let s = (500.0 / base.width as f32).min(500.0 / base.height as f32).min(1.0);
    let (sw, sh) = (
        (base.width as f32 * s).round().max(1.0) as u32,
        (base.height as f32 * s).round().max(1.0) as u32,
    );
    let mut bgra = vec![0u8; (sw * sh * 4) as usize];
    for y in 0..sh {
        for x in 0..sw {
            let bx = (x * base.width / sw).min(base.width - 1);
            let by = (y * base.height / sh).min(base.height - 1);
            let si = ((by * base.width + bx) * 4) as usize;
            let di = ((y * sw + x) * 4) as usize;
            bgra[di..di + 3].copy_from_slice(&base.bgra[si..si + 3]);
            bgra[di + 3] = 255;
        }
    }
    let radius = (sw.max(sh) as f32 * 0.04).round().max(6.0) as i32;
    for off in 0..3 {
        box_blur_1ch(&mut bgra, sw, sh, 4, off, radius, 2);
    }
    for px in bgra.chunks_exact_mut(4) {
        px[0] = (f32::from(px[0]) * 0.84) as u8;
        px[1] = (f32::from(px[1]) * 0.84) as u8;
        px[2] = (f32::from(px[2]) * 0.84) as u8;
    }
    CapturedImage { width: sw, height: sh, bgra }
}

/// Output canvas size for a padded `base_w × base_h` under an aspect target
/// (grows the shorter axis; never shrinks below the padded base).
pub fn aspect_dims(base_w: u32, base_h: u32, aspect: i32) -> (u32, u32) {
    let ratio = match aspect {
        1 => 1.0,
        2 => 16.0 / 9.0,
        3 => 9.0 / 16.0,
        4 => 4.0 / 3.0,
        _ => return (base_w, base_h),
    };
    if (base_w as f32 / base_h as f32) < ratio {
        ((base_h as f32 * ratio).round() as u32, base_h)
    } else {
        (base_w, (base_w as f32 / ratio).round() as u32)
    }
}

// Cache keys: the fill (solid/gradient/blur) and the blurred shadow mask are
// cached SEPARATELY. The mask's expensive box-blur depends only on geometry,
// so dragging the shadow Strength slider just re-scales opacity — it never
// re-blurs. This is what keeps the panel fluid.
type FillKey = (u32, u32, i32, (u8, u8, u8), u32);
type ShadowKey = (u32, u32, u32, u32);
pub const BG_SHADOW_MAX: f32 = 0.7;

/// Backdrop fill (no shadow): solid / gradient / blur. Cached by look + size.
pub fn build_fill(full_w: u32, full_h: u32, fill: &BgFill, blur_mini: Option<&CapturedImage>) -> Vec<u8> {
    match fill {
        BgFill::Solid(c) => {
            let mut o = vec![0u8; (full_w * full_h * 4) as usize];
            for px in o.chunks_exact_mut(4) {
                px[0] = c[2];
                px[1] = c[1];
                px[2] = c[0];
                px[3] = 255;
            }
            o
        }
        BgFill::Gradient(a, b) => {
            let mut o = vec![0u8; (full_w * full_h * 4) as usize];
            let denom = (full_w + full_h) as f32;
            for y in 0..full_h {
                for x in 0..full_w {
                    let t = (x + y) as f32 / denom;
                    let lerp = |c0: u8, c1: u8| (f32::from(c1) - f32::from(c0)).mul_add(t, f32::from(c0)) as u8;
                    let i = ((y * full_w + x) * 4) as usize;
                    o[i] = lerp(a[2], b[2]);
                    o[i + 1] = lerp(a[1], b[1]);
                    o[i + 2] = lerp(a[0], b[0]);
                    o[i + 3] = 255;
                }
            }
            o
        }
        BgFill::Blur => blur_mini.map_or_else(
            || vec![40u8; (full_w * full_h * 4) as usize],
            |m| resample_cover(m, full_w, full_h),
        ),
    }
}

/// Blurred alpha mask of the (offset) rounded card silhouette — the shadow's
/// shape. No colour, no strength, so it survives every Strength drag.
#[allow(clippy::too_many_arguments, clippy::similar_names)]
pub fn build_shadow_mask(full_w: u32, full_h: u32, off_x: u32, off_y: u32, iw: u32, ih: u32, radius: f32, pad: u32) -> Vec<u8> {
    let base_scale = (pad.max(16)) as f32;
    let blur_r = (base_scale * 0.6).max(16.0).round() as i32;
    let off_sy = (base_scale * 0.25).max(8.0).round() as i32;
    let mut mask = vec![0u8; (full_w * full_h) as usize];
    for y in 0..ih {
        for x in 0..iw {
            if inside_rounded(x, y, iw, ih, radius) {
                let px = off_x as i32 + x as i32;
                let py = off_y as i32 + off_sy + y as i32;
                if px >= 0 && py >= 0 && (px as u32) < full_w && (py as u32) < full_h {
                    mask[(py as u32 * full_w + px as u32) as usize] = 255;
                }
            }
        }
    }
    box_blur_1ch(&mut mask, full_w, full_h, 1, 0, blur_r, 2);
    mask
}

/// Frame the annotated `img` on the (cached) backdrop: padding, aspect-ratio
/// expansion, drop shadow, and a rounded-corner clip on the card.
pub fn render_background(img: &CapturedImage, st: &EditorState) -> CapturedImage {
    let (iw, ih) = (img.width, img.height);
    let pad = st.bg_padding;
    let radius = st.bg_radius.min(iw.min(ih) / 2) as f32;
    let (base_w, base_h) = (iw + 2 * pad, ih + 2 * pad);
    let (full_w, full_h) = aspect_dims(base_w, base_h, st.bg_aspect);
    let off_x = (full_w - iw) / 2;
    let off_y = (full_h - ih) / 2;
    let fill = bg_fill(st.bg_preset, st.bg_custom);
    // Fill layer (cached by look + size).
    let fkey: FillKey = (full_w, full_h, st.bg_preset, st.bg_custom, st.bg_base_ver);
    let mut out = {
        let mut c = st.bg_fill_cache.borrow_mut();
        if c.as_ref().is_some_and(|(k, _)| *k == fkey) {
            c.as_ref().unwrap().1.clone()
        } else {
            let mini = if matches!(fill, BgFill::Blur) {
                let mut b = st.bg_blur.borrow_mut();
                if b.as_ref().map(|(v, _)| *v) != Some(st.bg_base_ver) {
                    if let Some(base) = st.base.as_ref() {
                        *b = Some((st.bg_base_ver, bg_blur_miniature(base)));
                    }
                }
                b.as_ref().map(|(_, m)| m.clone())
            } else {
                None
            };
            let v = build_fill(full_w, full_h, &fill, mini.as_ref());
            *c = Some((fkey, v.clone()));
            v
        }
    };
    // Drop shadow — the blurred mask caches by geometry; Strength just scales
    // opacity here (no re-blur on drag).
    if st.bg_shadow && st.bg_shadow_strength > 0 {
        let skey: ShadowKey = (full_w, full_h, pad, st.bg_radius);
        let opacity = BG_SHADOW_MAX * (st.bg_shadow_strength as f32 / 100.0);
        let mut sc = st.bg_shadow_cache.borrow_mut();
        if sc.as_ref().is_none_or(|(k, _)| *k != skey) {
            *sc = Some((skey, build_shadow_mask(full_w, full_h, off_x, off_y, iw, ih, radius, pad)));
        }
        let mask = &sc.as_ref().unwrap().1;
        for (px, &mv) in out.chunks_exact_mut(4).zip(mask.iter()) {
            if mv > 0 {
                let k = (f32::from(mv) / 255.0).mul_add(-opacity, 1.0);
                px[0] = (f32::from(px[0]) * k) as u8;
                px[1] = (f32::from(px[1]) * k) as u8;
                px[2] = (f32::from(px[2]) * k) as u8;
            }
        }
    }
    // Composite the screenshot at (off_x, off_y), clipped to the rounded card.
    // Interior rows are a single memcpy; only corner rows pay the mask test.
    for y in 0..ih {
        let di = (((y + off_y) * full_w + off_x) * 4) as usize;
        let si = (y * iw * 4) as usize;
        let span = (iw * 4) as usize;
        if (y as f32) >= radius && (y as f32) < ih as f32 - radius {
            out[di..di + span].copy_from_slice(&img.bgra[si..si + span]);
        } else {
            for x in 0..iw {
                if inside_rounded(x, y, iw, ih, radius) {
                    let d = (((y + off_y) * full_w + (x + off_x)) * 4) as usize;
                    let s = ((y * iw + x) * 4) as usize;
                    out[d..d + 4].copy_from_slice(&img.bgra[s..s + 4]);
                }
            }
        }
    }
    CapturedImage { width: full_w, height: full_h, bgra: out }
}

/// Crop the base image to `(rx, ry, rw, rh)` (screenshot px) and shift the
/// annotations so they stay aligned with the new origin.
pub fn crop_base(st: &mut EditorState, rx: u32, ry: u32, rw: u32, rh: u32) {
    let Some(base) = st.base.as_ref() else { return };
    let (bw, bh) = (base.width, base.height);
    let rx = rx.min(bw.saturating_sub(1));
    let ry = ry.min(bh.saturating_sub(1));
    let rw = rw.clamp(1, bw - rx);
    let rh = rh.clamp(1, bh - ry);
    let mut bgra = Vec::with_capacity((rw * rh * 4) as usize);
    for y in ry..ry + rh {
        let row = ((y * bw + rx) * 4) as usize;
        bgra.extend_from_slice(&base.bgra[row..row + (rw * 4) as usize]);
    }
    st.base = Some(CapturedImage { width: rw, height: rh, bgra });
    let (dx, dy) = (rx as f32, ry as f32);
    for s in &mut st.shapes {
        s.x1 -= dx;
        s.x2 -= dx;
        s.y1 -= dy;
        s.y2 -= dy;
    }
    st.sel = None;
    st.redo.clear();
    st.bg_base_ver += 1;
    *st.bg_fill_cache.borrow_mut() = None;
    *st.bg_shadow_cache.borrow_mut() = None;
    *st.base_px.borrow_mut() = None;
}

/// Bilinear resample of a BGRA image to `nw × nh`.
pub fn resample(src: &CapturedImage, nw: u32, nh: u32) -> CapturedImage {
    let (sw, sh) = (src.width, src.height);
    let mut bgra = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        let fy = ((y as f32 + 0.5) * sh as f32 / nh as f32 - 0.5).clamp(0.0, (sh - 1) as f32);
        let y0 = fy.floor() as u32;
        let y1 = (y0 + 1).min(sh - 1);
        let wy = fy - y0 as f32;
        for x in 0..nw {
            let fx = ((x as f32 + 0.5) * sw as f32 / nw as f32 - 0.5).clamp(0.0, (sw - 1) as f32);
            let x0 = fx.floor() as u32;
            let x1 = (x0 + 1).min(sw - 1);
            let wx = fx - x0 as f32;
            for c in 0..4 {
                let p = |px: u32, py: u32| f32::from(src.bgra[((py * sw + px) * 4 + c) as usize]);
                let top = p(x0, y0) * (1.0 - wx) + p(x1, y0) * wx;
                let bot = p(x0, y1) * (1.0 - wx) + p(x1, y1) * wx;
                bgra[((y * nw + x) * 4 + c) as usize] = (top * (1.0 - wy) + bot * wy).round() as u8;
            }
        }
    }
    CapturedImage { width: nw, height: nh, bgra }
}

/// Resample the base image (and annotations) to exact `nw × nh`.
pub fn resize_to(st: &mut EditorState, nw: u32, nh: u32) {
    let Some(base) = st.base.as_ref() else { return };
    let nw = nw.clamp(16, 8000);
    let nh = nh.clamp(16, 8000);
    let (sx, sy) = (nw as f32 / base.width as f32, nh as f32 / base.height as f32);
    let resized = resample(base, nw, nh);
    st.base = Some(resized);
    for s in &mut st.shapes {
        s.x1 *= sx;
        s.y1 *= sy;
        s.x2 *= sx;
        s.y2 *= sy;
        s.width *= f32::midpoint(sx, sy);
    }
    st.sel = None;
    st.redo.clear();
    st.bg_base_ver += 1;
    *st.bg_fill_cache.borrow_mut() = None;
    *st.bg_shadow_cache.borrow_mut() = None;
    *st.base_px.borrow_mut() = None;
}

/// The final editor image: annotations baked in, framed on a backdrop if on.
pub fn compose(st: &EditorState) -> Option<CapturedImage> {
    st.base.as_ref()?;
    let annotated = rasterize_annotations(st);
    Some(if st.bg_preset == BG_NONE {
        annotated
    } else {
        render_background(&annotated, st)
    })
}

/// Perpendicular distance from a point to a line segment.
pub fn dist_to_segment(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = dx.mul_add(dx, dy * dy);
    if len2 == 0.0 {
        return (px - ax).hypot(py - ay);
    }
    let t = ((px - ax).mul_add(dx, (py - ay) * dy) / len2).clamp(0.0, 1.0);
    (px - (ax + t * dx)).hypot(py - (ay + t * dy))
}

/// The resize-handle anchor points (screenshot px) for a shape: 2 endpoints for
/// line-like shapes, 8 bbox handles for boxes, none for text.
pub fn handle_points(s: &Shape) -> Vec<(f32, f32)> {
    match s.kind {
        0 | 3 | 4 => vec![(s.x1, s.y1), (s.x2, s.y2)],
        6 | 9 | 10 => vec![],
        _ => {
            let (l, t, r, b) = (s.x1.min(s.x2), s.y1.min(s.y2), s.x1.max(s.x2), s.y1.max(s.y2));
            let (mx, my) = (f32::midpoint(l, r), f32::midpoint(t, b));
            vec![(l, t), (mx, t), (r, t), (r, my), (r, b), (mx, b), (l, b), (l, my)]
        }
    }
}

/// Index of the handle near (px, py) within `thresh`, if any.
pub fn hit_handle(s: &Shape, px: f32, py: f32, thresh: f32) -> Option<usize> {
    handle_points(s)
        .iter()
        .position(|&(hx, hy)| (px - hx).hypot(py - hy) <= thresh)
}

/// A copy of `orig` with the given handle dragged to (px, py).
pub fn resize_shape(orig: Shape, handle: i32, px: f32, py: f32) -> Shape {
    if matches!(orig.kind, 0 | 3 | 4) {
        if handle == 0 {
            Shape { x1: px, y1: py, ..orig }
        } else {
            Shape { x2: px, y2: py, ..orig }
        }
    } else {
        let (mut l, mut t, mut r, mut b) =
            (orig.x1.min(orig.x2), orig.y1.min(orig.y2), orig.x1.max(orig.x2), orig.y1.max(orig.y2));
        match handle {
            0 => { l = px; t = py; }
            1 => { t = py; }
            2 => { r = px; t = py; }
            3 => { r = px; }
            4 => { r = px; b = py; }
            5 => { b = py; }
            6 => { l = px; b = py; }
            _ => { l = px; }
        }
        Shape { x1: l, y1: t, x2: r, y2: b, ..orig }
    }
}

/// Push the selected shape's geometry (screenshot px) to the UI so it can draw
/// the selection box + handles, or clear it when nothing is selected.
pub fn update_sel_props(e: &EditorWindow, st: &EditorState) {
    match st.sel.and_then(|i| st.shapes.get(i)) {
        Some(s) => {
            e.set_sel_active(true);
            e.set_sel_kind(s.kind);
            e.set_sel_x1(s.x1);
            e.set_sel_y1(s.y1);
            e.set_sel_x2(s.x2);
            e.set_sel_y2(s.y2);
        }
        None => e.set_sel_active(false),
    }
}

/// Topmost shape under the point (image px), or None. Last drawn = on top.
pub fn hit_test(shapes: &[Shape], px: f32, py: f32) -> Option<usize> {
    for i in (0..shapes.len()).rev() {
        let s = &shapes[i];
        let hit = match s.kind {
            1 | 2 | 5 => {
                let (l, t) = (s.x1.min(s.x2), s.y1.min(s.y2));
                let (r, b) = (s.x1.max(s.x2), s.y1.max(s.y2));
                px >= l - 6.0 && px <= r + 6.0 && py >= t - 6.0 && py <= b + 6.0
            }
            6 => {
                let w = s.text.chars().count() as f32 * s.width * 0.6;
                px >= s.x1 - 4.0 && px <= s.x1 + w + 8.0 && py >= s.y1 - 4.0 && py <= s.width.mul_add(1.3, s.y1)
            }
            9 => (px - s.x1).hypot(py - s.y1) <= s.width + 6.0,
            10 => {
                let tol = s.width.max(8.0);
                let mut i = 0;
                let mut hit = false;
                while i + 3 < s.points.len() {
                    if dist_to_segment(px, py, s.points[i], s.points[i + 1], s.points[i + 2], s.points[i + 3]) <= tol {
                        hit = true;
                        break;
                    }
                    i += 2;
                }
                hit
            }
            _ => dist_to_segment(px, py, s.x1, s.y1, s.x2, s.y2) <= s.width.max(8.0),
        };
        if hit {
            return Some(i);
        }
    }
    None
}

/// Lazily-loaded editor text fonts by family: 0 Sans · 1 Serif · 2 Mono ·
/// 3 Impact. Each tries a couple of system TTFs; family 0 is the fallback.
pub fn editor_font(family: i32) -> Option<&'static ab_glyph::FontVec> {
    static FONTS: std::sync::OnceLock<Vec<Option<ab_glyph::FontVec>>> = std::sync::OnceLock::new();
    let fonts = FONTS.get_or_init(|| {
        let load = |paths: &[&str]| -> Option<ab_glyph::FontVec> {
            paths
                .iter()
                .find_map(|p| std::fs::read(p).ok())
                .and_then(|d| ab_glyph::FontVec::try_from_vec(d).ok())
        };
        vec![
            load(&[r"C:\Windows\Fonts\segoeui.ttf", r"C:\Windows\Fonts\arial.ttf"]),
            load(&[r"C:\Windows\Fonts\times.ttf", r"C:\Windows\Fonts\georgia.ttf"]),
            load(&[r"C:\Windows\Fonts\consola.ttf", r"C:\Windows\Fonts\cour.ttf"]),
            load(&[r"C:\Windows\Fonts\impact.ttf", r"C:\Windows\Fonts\ariblk.ttf"]),
        ]
    });
    let idx = family.clamp(0, 3) as usize;
    fonts.get(idx).and_then(|f| f.as_ref()).or_else(|| fonts[0].as_ref())
}

/// Total advance width of `text` at `size` px (for centring labels).
pub fn text_advance(text: &str, size: f32) -> f32 {
    use ab_glyph::{Font, PxScale, ScaleFont};
    let Some(font) = editor_font(0) else {
        return text.chars().count() as f32 * size * 0.55;
    };
    let scaled = font.as_scaled(PxScale::from(size));
    let mut w = 0.0;
    let mut prev = None;
    for ch in text.chars() {
        let g = font.glyph_id(ch);
        if let Some(p) = prev {
            w += scaled.kern(p, g);
        }
        w += scaled.h_advance(g);
        prev = Some(g);
    }
    w
}

/// A filled number badge (circle + white ring + centred white number) of radius
/// `radius` centred at (cx, cy). Step-marker tool, CleanShot-style.
#[allow(clippy::too_many_arguments)]
pub fn draw_badge(pixmap: &mut tiny_skia::Pixmap, cx: f32, cy: f32, radius: f32, r: u8, g: u8, b: u8, label: &str) {
    use tiny_skia::{FillRule, LineCap, LineJoin, Paint, PathBuilder, Rect as SkRect, Stroke, Transform};
    let Some(rect) = SkRect::from_ltrb(cx - radius, cy - radius, cx + radius, cy + radius) else { return };
    let mut pb = PathBuilder::new();
    pb.push_oval(rect);
    let Some(path) = pb.finish() else { return };
    let mut fill = Paint::default();
    fill.set_color_rgba8(r, g, b, 255);
    fill.anti_alias = true;
    pixmap.fill_path(&path, &fill, FillRule::Winding, Transform::identity(), None);
    // White ring keeps the badge legible on any backdrop.
    let mut ring = Paint::default();
    ring.set_color_rgba8(255, 255, 255, 235);
    ring.anti_alias = true;
    let stroke = Stroke {
        width: (radius * 0.12).max(2.0),
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    };
    pixmap.stroke_path(&path, &ring, &stroke, Transform::identity(), None);
    // Centred white number.
    let size = radius * 1.15;
    let adv = text_advance(label, size);
    draw_text(pixmap, label, cx - adv / 2.0, cy - size * 0.62, size, 255, 255, 255, 0);
}

/// Stroke a freehand polyline (`pts` = flattened x,y) — the pen tool.
pub fn draw_polyline(pixmap: &mut tiny_skia::Pixmap, pts: &[f32], width: f32, r: u8, g: u8, b: u8) {
    use tiny_skia::{LineCap, LineJoin, Paint, PathBuilder, Stroke, Transform};
    if pts.len() < 4 {
        return;
    }
    let mut pb = PathBuilder::new();
    pb.move_to(pts[0], pts[1]);
    let mut i = 2;
    while i + 1 < pts.len() {
        pb.line_to(pts[i], pts[i + 1]);
        i += 2;
    }
    let Some(path) = pb.finish() else { return };
    let mut paint = Paint::default();
    paint.set_color_rgba8(r, g, b, 255);
    paint.anti_alias = true;
    let stroke = Stroke {
        width,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    };
    pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
}

/// Rasterize `text` onto the pixmap at top-left (x, y), `size` px, colour rgb.
#[allow(clippy::too_many_arguments)]
pub fn draw_text(pixmap: &mut tiny_skia::Pixmap, text: &str, x: f32, y: f32, size: f32, r: u8, g: u8, b: u8, family: i32) {
    use ab_glyph::{Font, PxScale, ScaleFont};
    let Some(font) = editor_font(family) else { return };
    let scale = PxScale::from(size);
    let scaled = font.as_scaled(scale);
    let w = pixmap.width() as i32;
    let h = pixmap.height() as i32;
    let baseline = y + scaled.ascent();
    let mut caret = x;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    let px = pixmap.pixels_mut();
    for ch in text.chars() {
        let gid = font.glyph_id(ch);
        if let Some(p) = prev {
            caret += scaled.kern(p, gid);
        }
        let glyph = gid.with_scale_and_position(scale, ab_glyph::point(caret, baseline));
        if let Some(outline) = font.outline_glyph(glyph) {
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, cov| {
                let ix = bounds.min.x as i32 + gx as i32;
                let iy = bounds.min.y as i32 + gy as i32;
                if ix < 0 || iy < 0 || ix >= w || iy >= h || cov <= 0.0 {
                    return;
                }
                let idx = (iy * w + ix) as usize;
                let dst = px[idx];
                let a = cov.min(1.0);
                let inv = 1.0 - a;
                let nr = f32::from(r).mul_add(a, f32::from(dst.red()) * inv) as u8;
                let ng = f32::from(g).mul_add(a, f32::from(dst.green()) * inv) as u8;
                let nb = f32::from(b).mul_add(a, f32::from(dst.blue()) * inv) as u8;
                if let Some(c) = tiny_skia::PremultipliedColorU8::from_rgba(nr, ng, nb, 255) {
                    px[idx] = c;
                }
            });
        }
        caret += scaled.h_advance(gid);
        prev = Some(gid);
    }
}

#[derive(Default)]
pub struct EditorState {
    pub(crate) base: Option<CapturedImage>,
    pub(crate) shapes: Vec<Shape>,
    pub(crate) redo: Vec<Shape>,
    pub(crate) path: Option<PathBuf>,
    // Move tool: selected shape + drag anchor (image px) + its pre-drag copy.
    pub(crate) sel: Option<usize>,
    pub(crate) drag_from: (f32, f32),
    pub(crate) drag_orig: Option<Shape>,
    pub(crate) drag_handle: i32, // -1 = move the whole shape, 0.. = resize via that handle
    // Freehand (pen) in-progress stroke: flattened x,y points + its colour/width.
    pub(crate) pen_draft: Vec<f32>,
    pub(crate) pen_rgb: (u8, u8, u8),
    pub(crate) pen_w: f32,

    // Background frame (CleanShot-style) — see render_background.
    pub(crate) bg_preset: i32,          // 0 none·1 slate·2 snow·3-7 gradients·8 blur·9 custom solid
    pub(crate) bg_custom: (u8, u8, u8), // colour for the custom solid swatch
    pub(crate) bg_padding: u32,         // px around the image
    pub(crate) bg_radius: u32,          // card corner radius (px)
    pub(crate) bg_shadow: bool,
    pub(crate) bg_shadow_strength: u32, // 0..100
    pub(crate) bg_aspect: i32,          // 0 free·1 1:1·2 16:9·3 9:16·4 4:3
    pub(crate) bg_base_ver: u32,        // bumped whenever `base` changes → blur-cache bust
    // Perf caches: the premultiplied base, the blurred miniature (blur preset),
    // and the static background layer (backdrop + shadow).
    pub(crate) base_px: std::cell::RefCell<Option<tiny_skia::Pixmap>>,
    pub(crate) bg_blur: std::cell::RefCell<Option<(u32, CapturedImage)>>,
    pub(crate) bg_fill_cache: std::cell::RefCell<Option<(FillKey, Vec<u8>)>>,
    pub(crate) bg_shadow_cache: std::cell::RefCell<Option<(ShadowKey, Vec<u8>)>>,
}

/// Map widget-space (x, y) to *screenshot* pixels, accounting for the centred
/// fit (32px breathing room, capped at 1×) × zoom and, when the background is
/// on, the padding that insets the screenshot inside the displayed canvas.
/// Returns (screenshot-x, screenshot-y, display-scale).
pub fn map_shot(st: &EditorState, x: f32, y: f32, cw: f32, ch: f32, zoom: f32) -> Option<(f32, f32, f32)> {
    let base = st.base.as_ref()?;
    let (iw, ih) = (base.width, base.height);
    // Displayed canvas = the composed image: padded + aspect-expanded when the
    // frame is on. The base image sits at (img_off_x, img_off_y) within it.
    let (full_w, full_h, img_off_x, img_off_y) = if st.bg_preset == BG_NONE {
        (iw as f32, ih as f32, 0.0, 0.0)
    } else {
        let (bw, bh) = (iw + 2 * st.bg_padding, ih + 2 * st.bg_padding);
        let (fw, fh) = aspect_dims(bw, bh, st.bg_aspect);
        (fw as f32, fh as f32, ((fw - iw) / 2) as f32, ((fh - ih) / 2) as f32)
    };
    let ds = ((cw - 32.0) / full_w).min((ch - 32.0) / full_h).min(1.0) * zoom;
    if ds <= 0.0 {
        return None;
    }
    let off_x = full_w.mul_add(-ds, cw) / 2.0;
    let off_y = full_h.mul_add(-ds, ch) / 2.0;
    Some((
        ((x - off_x) / ds - img_off_x).clamp(0.0, iw as f32),
        ((y - off_y) / ds - img_off_y).clamp(0.0, ih as f32),
        ds,
    ))
}

/// Block-average a rectangular region of the pixmap (the pixelate tool).
// `n` is guarded > 0 before each division, so checked_div would only add noise.
#[allow(clippy::manual_checked_ops)]
pub fn pixelate_region(pixmap: &mut tiny_skia::Pixmap, x1: f32, y1: f32, x2: f32, y2: f32) {
    let w = pixmap.width() as i32;
    let h = pixmap.height() as i32;
    let l = (x1.min(x2)).round().clamp(0.0, w as f32) as i32;
    let t = (y1.min(y2)).round().clamp(0.0, h as f32) as i32;
    let r = (x1.max(x2)).round().clamp(0.0, w as f32) as i32;
    let b = (y1.max(y2)).round().clamp(0.0, h as f32) as i32;
    if r - l < 2 || b - t < 2 {
        return;
    }
    let block = (((r - l).max(b - t)) / 18).clamp(6, 40);
    let px = pixmap.pixels_mut();
    let mut by = t;
    while by < b {
        let mut bx = l;
        while bx < r {
            let (xe, ye) = ((bx + block).min(r), (by + block).min(b));
            let (mut sr, mut sg, mut sb, mut n) = (0u32, 0u32, 0u32, 0u32);
            for y in by..ye {
                for x in bx..xe {
                    let p = px[(y * w + x) as usize];
                    sr += u32::from(p.red());
                    sg += u32::from(p.green());
                    sb += u32::from(p.blue());
                    n += 1;
                }
            }
            if n > 0 {
                if let Some(avg) = tiny_skia::PremultipliedColorU8::from_rgba(
                    (sr / n) as u8,
                    (sg / n) as u8,
                    (sb / n) as u8,
                    255,
                ) {
                    for y in by..ye {
                        for x in bx..xe {
                            px[(y * w + x) as usize] = avg;
                        }
                    }
                }
            }
            bx += block;
        }
        by += block;
    }
}

/// A rounded-rectangle path (tiny-skia has no round-rect builder).
pub fn rounded_rect(l: f32, t: f32, r: f32, b: f32, rad: f32) -> Option<tiny_skia::Path> {
    use tiny_skia::PathBuilder;
    if r - l < 1.0 || b - t < 1.0 {
        return None;
    }
    let rad = rad.clamp(0.0, ((r - l) / 2.0).min((b - t) / 2.0));
    let mut pb = PathBuilder::new();
    pb.move_to(l + rad, t);
    pb.line_to(r - rad, t);
    pb.quad_to(r, t, r, t + rad);
    pb.line_to(r, b - rad);
    pb.quad_to(r, b, r - rad, b);
    pb.line_to(l + rad, b);
    pb.quad_to(l, b, l, b - rad);
    pb.line_to(l, t + rad);
    pb.quad_to(l, t, l + rad, t);
    pb.close();
    pb.finish()
}

/// Draw the committed shapes onto the base image at full resolution.
pub fn rasterize_annotations(st: &EditorState) -> CapturedImage {
    use tiny_skia::{
        BlendMode, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, PremultipliedColorU8,
        Rect as SkRect, Stroke, Transform,
    };
    let Some(base) = st.base.as_ref() else {
        return CapturedImage { width: 1, height: 1, bgra: vec![0; 4] };
    };
    let shapes = &st.shapes;
    // Reuse the premultiplied base across frames (cloning a Pixmap is a memcpy;
    // rebuilding it from BGRA is a per-pixel conversion). Rebuild only when the
    // dimensions change (crop/resize/open clears it).
    let mut pixmap = {
        let mut cache = st.base_px.borrow_mut();
        let fresh = cache
            .as_ref()
            .is_none_or(|p| p.width() != base.width || p.height() != base.height);
        if fresh {
            let Some(mut p) = Pixmap::new(base.width, base.height) else {
                return base.clone();
            };
            for (dst, src) in p.pixels_mut().iter_mut().zip(base.bgra.chunks_exact(4)) {
                if let Some(c) = PremultipliedColorU8::from_rgba(src[2], src[1], src[0], 255) {
                    *dst = c;
                }
            }
            *cache = Some(p);
        }
        cache.as_ref().unwrap().clone()
    };
    for s in shapes {
        // Pixelate operates on the pixels directly — no stroke.
        if s.kind == 5 {
            pixelate_region(&mut pixmap, s.x1, s.y1, s.x2, s.y2);
            continue;
        }
        // Text is glyph-rasterized, not a path.
        if s.kind == 6 {
            draw_text(&mut pixmap, &s.text, s.x1, s.y1, s.width, s.r, s.g, s.b, s.font);
            continue;
        }
        // Number badge: filled circle + centred number (width = radius).
        if s.kind == 9 {
            draw_badge(&mut pixmap, s.x1, s.y1, s.width, s.r, s.g, s.b, &s.text);
            continue;
        }
        // Freehand pen stroke.
        if s.kind == 10 {
            draw_polyline(&mut pixmap, &s.points, s.width, s.r, s.g, s.b);
            continue;
        }
        let highlighter = s.kind == 4;
        let mut paint = Paint::default();
        paint.set_color_rgba8(s.r, s.g, s.b, if highlighter { 110 } else { 255 });
        paint.anti_alias = true;
        if highlighter {
            paint.blend_mode = BlendMode::Multiply;
        }
        let stroke = Stroke {
            width: if highlighter { s.width * 4.0 } else { s.width },
            line_cap: LineCap::Round,
            line_join: LineJoin::Round,
            ..Default::default()
        };
        let (l, t) = (s.x1.min(s.x2), s.y1.min(s.y2));
        let (r, b) = (s.x1.max(s.x2), s.y1.max(s.y2));
        let path = match s.kind {
            1 => {
                let rad = ((r - l).min(b - t) * 0.08).min(40.0);
                let Some(p) = rounded_rect(l, t, r, b, rad) else { continue };
                p
            }
            2 => {
                let Some(rect) = SkRect::from_ltrb(l, t, r, b) else { continue };
                let mut pb = PathBuilder::new();
                pb.push_oval(rect);
                let Some(p) = pb.finish() else { continue };
                p
            }
            3 | 4 => {
                let mut pb = PathBuilder::new();
                pb.move_to(s.x1, s.y1);
                pb.line_to(s.x2, s.y2);
                let Some(p) = pb.finish() else { continue };
                p
            }
            _ => {
                let mut pb = PathBuilder::new();
                pb.move_to(s.x1, s.y1);
                pb.line_to(s.x2, s.y2);
                let ang = (s.y2 - s.y1).atan2(s.x2 - s.x1);
                let hl = (s.width * 4.0).max(14.0);
                for da in [2.5f32, -2.5f32] {
                    pb.move_to(s.x2, s.y2);
                    pb.line_to(s.x2 + hl * (ang + da).cos(), s.y2 + hl * (ang + da).sin());
                }
                let Some(p) = pb.finish() else { continue };
                p
            }
        };
        // Filled rect/ellipse: a translucent wash under the outline.
        if s.filled && (s.kind == 1 || s.kind == 2) {
            let mut fill = Paint::default();
            fill.set_color_rgba8(s.r, s.g, s.b, 64);
            fill.anti_alias = true;
            pixmap.fill_path(&path, &fill, FillRule::Winding, Transform::identity(), None);
        }
        pixmap.stroke_path(&path, &paint, &stroke, Transform::identity(), None);
    }
    // The pen stroke in progress (drawn live until pointer-up commits it).
    if st.pen_draft.len() >= 4 {
        draw_polyline(&mut pixmap, &st.pen_draft, st.pen_w, st.pen_rgb.0, st.pen_rgb.1, st.pen_rgb.2);
    }
    let mut bgra = Vec::with_capacity((base.width * base.height * 4) as usize);
    for px in pixmap.pixels() {
        bgra.extend_from_slice(&[px.blue(), px.green(), px.red(), 255]);
    }
    CapturedImage { width: base.width, height: base.height, bgra }
}

/// Theme-aware checkerboard for the editor canvas (shown around the image when
/// zoomed out). Tiled square pattern; the editor displays it `cover`-fit.
pub fn make_checker(dark: bool) -> CapturedImage {
    let size = 1024u32;
    let tile = 8u32;
    let (lo, hi) = if dark { (32u8, 42u8) } else { (230u8, 246u8) };
    let mut bgra = Vec::with_capacity((size * size * 4) as usize);
    for y in 0..size {
        for x in 0..size {
            let v = if (x / tile + y / tile).is_multiple_of(2) { lo } else { hi };
            bgra.extend_from_slice(&[v, v, v, 255]);
        }
    }
    CapturedImage { width: size, height: size, bgra }
}

pub fn editor_rerender(editor: &slint::Weak<EditorWindow>, state: &Rc<RefCell<EditorState>>) {
    let st = state.borrow();
    if st.base.is_none() {
        return;
    }
    let Some(e) = editor.upgrade() else { return };
    // The displayed `source` is ONLY the annotated base — the background frame
    // is composited live by the GPU scene graph (Rectangle + drop-shadow), so
    // background tweaks never re-rasterize or re-upload. The full bake happens
    // only on save/copy (`compose`).
    let annotated = rasterize_annotations(&st);
    e.set_source(to_slint_image(&annotated));
    // The blur preset is the only one needing a (cheap, cached) bitmap backdrop.
    if st.bg_preset == BG_BLUR {
        let mut b = st.bg_blur.borrow_mut();
        if b.as_ref().map(|(v, _)| *v) != Some(st.bg_base_ver) {
            if let Some(base) = st.base.as_ref() {
                *b = Some((st.bg_base_ver, bg_blur_miniature(base)));
            }
        }
        if let Some((_, m)) = b.as_ref() {
            e.set_bg_blur_image(to_slint_image(m));
        }
    }
    // Mirror the background px geometry into the UI (drives the GPU frame layout).
    e.set_bg_padding(st.bg_padding as i32);
    e.set_bg_radius(st.bg_radius as i32);
    if let Some(b) = st.base.as_ref() {
        e.set_base_w(b.width as i32);
        e.set_base_h(b.height as i32);
        e.set_rz_w(b.width.to_string().into());
        e.set_rz_h(b.height.to_string().into());
    }
    update_sel_props(&e, &st);
}

pub fn open_editor(editor: &slint::Weak<EditorWindow>, state: &Rc<RefCell<EditorState>>, path: PathBuf) {
    let Ok(base) = clipo_capture::decode_to_bgra(&path) else { return };
    {
        let mut st = state.borrow_mut();
        st.base = Some(base);
        st.shapes.clear();
        st.redo.clear();
        st.sel = None;
        st.bg_preset = BG_NONE;
        st.bg_custom = (99, 102, 241);
        st.bg_padding = 32;
        st.bg_radius = 16;
        st.bg_shadow = true;
        st.bg_shadow_strength = 60;
        st.bg_aspect = 0;
        st.bg_base_ver += 1;
        *st.base_px.borrow_mut() = None;
        *st.bg_blur.borrow_mut() = None;
        *st.bg_fill_cache.borrow_mut() = None;
    *st.bg_shadow_cache.borrow_mut() = None;
        st.path = Some(path);
    }
    editor_rerender(editor, state);
    if let Some(e) = editor.upgrade() {
        // Reopen in the last window/maximized state; reset zoom + background.
        let maximized = crate::settings::load_settings().editor_maximized;
        e.set_is_maximized(maximized);
        e.set_zoom(1.0);
        e.set_bg_on(false);
        e.set_bg_preset(BG_NONE);
        e.set_bg_color(slint::Color::from_rgb_u8(99, 102, 241));
        e.set_bg_pad_idx(2);
        e.set_bg_rad_idx(2);
        e.set_bg_shadow(true);
        e.set_bg_shadow_strength(60.0);
        e.set_bg_aspect(0);
        e.set_bg_open(false);
        e.set_bg_cp_open(false);
        e.set_text_editing(false);
        e.set_text_buffer("".into());
        show_remembered(&e, 1000.0, 690.0, maximized);
    }
}
