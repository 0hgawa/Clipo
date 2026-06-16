//! Capture history: listing, date bucketing + filtering, thumbnails, and
//! populating the grid.
use crate::{capture_dir, is_gif, is_video, media_files_in, thumb_path, HistoryGroup, HistoryItem, HistoryWindow};
use std::cell::RefCell;
use std::path::PathBuf;

pub fn list_captures() -> Vec<PathBuf> {
    let mut files = media_files_in(&capture_dir());
    files.sort_by_key(|p| std::cmp::Reverse(p.metadata().and_then(|m| m.modified()).ok()));
    files
}

/// A loaded history entry. Thumbnails are decoded once (UI thread) and reused
/// across filter changes — re-filtering only clones the ref-counted Image.
pub struct HistoryEntry {
    path: PathBuf,
    thumb: slint::Image,
    modified_ms: i64,
    kind: u8, // 0 = image · 1 = video
}

thread_local! {
    /// The full, unfiltered history (UI thread only — Image isn't Send).
    static HISTORY_ENTRIES: RefCell<Vec<HistoryEntry>> = const { RefCell::new(Vec::new()) };
}

/// Drop a deleted capture from the in-memory thumbnail cache.
pub fn forget_capture(p: &std::path::Path) {
    HISTORY_ENTRIES.with(|s| s.borrow_mut().retain(|e| e.path != p));
}

/// Google-Photos-style bucket for a timestamp: (sort order, label). Lower order
/// sorts first (Today). Months get an order past the fixed buckets.
pub fn history_bucket(ms: i64, now: &chrono::DateTime<chrono::Local>) -> (i32, String) {
    use chrono::{Datelike, Local, TimeZone};
    let dt = Local.timestamp_millis_opt(ms).single().unwrap_or(*now);
    let days = now.date_naive().signed_duration_since(dt.date_naive()).num_days();
    if days <= 0 {
        return (0, "Today".into());
    }
    if days == 1 {
        return (1, "Yesterday".into());
    }
    if days < 7 {
        return (2, "This Week".into());
    }
    if dt.year() == now.year() && dt.month() == now.month() {
        return (3, "This Month".into());
    }
    let months_ago = (now.year() - dt.year()) * 12 + now.month() as i32 - dt.month() as i32;
    let label = if dt.year() == now.year() {
        dt.format("%B").to_string()
    } else {
        dt.format("%B %Y").to_string()
    };
    (3 + months_ago, label)
}

pub fn history_passes_date(ms: i64, now: &chrono::DateTime<chrono::Local>, date_f: i32) -> bool {
    use chrono::{Datelike, Local, TimeZone};
    if date_f == 0 {
        return true;
    }
    let dt = Local.timestamp_millis_opt(ms).single().unwrap_or(*now);
    let days = now.date_naive().signed_duration_since(dt.date_naive()).num_days();
    match date_f {
        1 => days == 0,
        2 => days < 7,
        3 => dt.year() == now.year() && dt.month() == now.month(),
        _ => true,
    }
}

/// Filter (kind + date) and bucket the stored entries, then push the grouped
/// model. Reads the filter selection straight off the window properties.
pub fn rebuild_history(h: &HistoryWindow) {
    let (date_f, kind_f) = (h.get_date_filter(), h.get_kind_filter());
    let now = chrono::Local::now();
    HISTORY_ENTRIES.with(|store| {
        let entries = store.borrow();
        let mut sel: Vec<&HistoryEntry> = entries
            .iter()
            .filter(|e| {
                let kind_ok = kind_f == 0 || (kind_f == 1 && e.kind == 0) || (kind_f == 2 && e.kind == 1);
                kind_ok && history_passes_date(e.modified_ms, &now, date_f)
            })
            .collect();
        sel.sort_by_key(|e| std::cmp::Reverse(e.modified_ms));
        // Loaded once (settings + cache), then a cheap in-memory check per item.
        let is_uploaded = crate::settings::uploaded_lookup();
        let mut groups: std::collections::BTreeMap<i32, (String, Vec<HistoryItem>)> =
            std::collections::BTreeMap::new();
        for e in &sel {
            let (order, label) = history_bucket(e.modified_ms, &now);
            // Card flavour drives the hover actions: 1 video (mp4) → external
            // player + GIF export; 2 gif → in-app viewer (animates), reveal/
            // delete only (edit/ocr/copy would flatten it); 0 image → edit row.
            let kind = if e.kind == 1 { 1 } else { i32::from(is_gif(&e.path)) * 2 };
            groups.entry(order).or_insert_with(|| (label, Vec::new())).1.push(HistoryItem {
                thumb: e.thumb.clone(),
                path: e.path.to_string_lossy().as_ref().into(),
                kind,
                uploaded: is_uploaded(&e.path),
            });
        }
        let model: Vec<HistoryGroup> = groups
            .into_values()
            .map(|(label, items)| HistoryGroup {
                label: label.into(),
                items: slint::ModelRc::new(slint::VecModel::from(items)),
            })
            .collect();
        h.set_shown(sel.len() as i32);
        h.set_total(entries.len() as i32);
        h.set_groups(slint::ModelRc::new(slint::VecModel::from(model)));
    });
}

/// A thumbnail decoded to straight RGBA bytes on a worker thread. The JPEG
/// decode + BGRA→RGBA swizzle are the costly parts and are `Send` (a
/// `slint::Image` is not), so they run off the UI thread; the UI only wraps it.
struct DecodedThumb {
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

/// Decode a sidecar thumbnail JPEG into RGBA bytes (worker thread). Uses our own
/// decoder, bypassing Slint's path-keyed image cache (which won't reload a file
/// edited in place — slint-ui/slint discussion #2527).
fn decode_thumb(tp: &std::path::Path) -> Option<DecodedThumb> {
    let mut img = clipo_capture::decode_to_bgra(tp).ok()?;
    for px in img.bgra.chunks_exact_mut(4) {
        px.swap(0, 2); // BGRA → RGBA
    }
    Some(DecodedThumb { width: img.width, height: img.height, rgba: img.bgra })
}

/// Wrap worker-decoded RGBA bytes into a `slint::Image` (UI thread; just an
/// allocation + copy, no decode).
fn thumb_from_rgba(d: DecodedThumb) -> slint::Image {
    let DecodedThumb { width, height, rgba } = d;
    let mut pb = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
    pb.make_mut_bytes().copy_from_slice(&rgba);
    slint::Image::from_rgba8(pb)
}

/// Paint the history from cache instantly (if any), then sync on a worker:
/// generate missing sidecar JPEGs and decode only NEW thumbnails, reusing the
/// already-decoded Images for unchanged files. So the first open does the full
/// work, but reopening is instant and costs only what actually changed.
pub fn populate_history(history: &slint::Weak<HistoryWindow>) {
    if let Some(h) = history.upgrade() {
        // Offer the upload action on image cards only when upload is set up.
        h.set_upload_configured(crate::settings::load_settings().upload_ready());
        if HISTORY_ENTRIES.with(|s| !s.borrow().is_empty()) {
            rebuild_history(&h); // instant repaint from the cached Images
        }
    }
    // Snapshot the cached keys so the worker decodes only files we don't already
    // hold an Image for (first open: all; reopen: just new/changed ones).
    let cached: std::collections::HashSet<(PathBuf, i64)> =
        HISTORY_ENTRIES.with(|s| s.borrow().iter().map(|e| (e.path.clone(), e.modified_ms)).collect());
    let history = history.clone();
    std::thread::spawn(move || {
        // Worker: ensure thumbnails exist AND decode them to RGBA bytes here,
        // off the UI thread — only the cheap byte→texture wrap is left for the
        // UI, so opening doesn't block while every JPEG is decoded.
        let raw: Vec<(PathBuf, i64, u8, Option<DecodedThumb>)> = list_captures()
            .iter()
            .take(200)
            .map(|p| {
                let tp = thumb_path(p);
                if !tp.exists() {
                    if let Ok(img) = clipo_capture::decode_to_bgra(p) {
                        let _ = clipo_capture::save_thumbnail_jpeg(&img, &tp);
                    }
                }
                let ms = std::fs::metadata(p)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map_or(0, |d| d.as_millis() as i64);
                let kind = u8::from(is_video(p));
                // Skip files the UI already has an Image for; decode the rest.
                let decoded = if cached.contains(&(p.clone(), ms)) {
                    None
                } else {
                    decode_thumb(&tp)
                };
                (p.clone(), ms, kind, decoded)
            })
            .collect();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(h) = history.upgrade() else { return };
            HISTORY_ENTRIES.with(|s| {
                // Reuse the Image for unchanged files (keyed by path + mtime so an
                // edited-in-place capture misses and uses its fresh decode);
                // otherwise wrap the bytes the worker decoded.
                let mut old: std::collections::HashMap<(PathBuf, i64), slint::Image> =
                    s.borrow_mut().drain(..).map(|e| ((e.path, e.modified_ms), e.thumb)).collect();
                let entries: Vec<HistoryEntry> = raw
                    .into_iter()
                    .map(|(path, modified_ms, kind, decoded)| HistoryEntry {
                        thumb: old
                            .remove(&(path.clone(), modified_ms))
                            .or_else(|| decoded.map(thumb_from_rgba))
                            .unwrap_or_default(),
                        path,
                        modified_ms,
                        kind,
                    })
                    .collect();
                *s.borrow_mut() = entries;
            });
            rebuild_history(&h);
            h.set_loaded(true);
        });
    });
}

