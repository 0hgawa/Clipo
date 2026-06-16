// Clipo — native Windows screenshot + screen recording, in Slint.
//
// Tray-resident: no main window. The tray menu opens each surface as its OWN
// borderless floating window, and all of it runs in ONE process (Slint is
// single-process). The capture/record engine (DXGI, D3D11, OCR) lives in the
// clipo-capture / clipo-core crates under crates/.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![deny(unsafe_op_in_unsafe_fn)]

use std::time::Duration;

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use clipo_capture::{
    copy_image_to_clipboard, enumerate_capturable_windows, extract_window_icon,
    focus_window_and_bounds, save_jpeg, save_png, CaptureEngine, VideoRecorder, WindowInfo,
    CAPTURE_JPEG_QUALITY,
};
use clipo_core::{CapturedImage, Rect};
use clipo_overlay::{Overlay, OverlayEvent};
use slint::{ComponentHandle, Model};
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetSystemMetrics, IsWindowVisible, SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNORMAL,
};

slint::include_modules!();

mod capture;
mod ocr;
mod recording;
mod editor;
mod history;
mod hotkeys;
mod settings;
mod single_instance;
mod upload;
mod win;
use capture::to_slint_image;
use ocr::run_ocr_for;
use recording::{begin_recording, export_gif, gif_available};
use editor::{EditorState, open_editor, map_shot, Shape, editor_rerender, hit_handle, hit_test, update_sel_props, resize_shape, BG_NONE, crop_base, resize_to, apply_solid, parse_hex, rgb_to_hsv, compose, make_checker};
use history::{rebuild_history, forget_capture, populate_history};
use hotkeys::{HkState, HK_IDS, HK_DEFAULTS, shortcut_conflicts, combo_display};
use settings::{load_settings, upload_capture_blocking, save_settings, LANGS, set_autostart, set_image_association, test_upload_blocking, check_update_blocking, download_and_apply_update, UpdateInfo, Settings};
use win::{
    disable_dwm_transitions, hwnd_of, maximize_work_area, raise_to_front,
    place_at_cursor_topmost, place_bottom_center, restore_window, show_bottom_right, show_fitted,
};
pub(crate) use win::{show_centered, show_remembered};

/// The brand mark for the tray — a 32px straight-RGBA frame rendered from the
/// SVG at build time (tools/gen-icon) and baked into the binary, so there is no
/// file I/O or decode at startup.
fn tray_icon_image() -> Icon {
    let rgba = include_bytes!("../assets/tray-32.rgba").to_vec();
    Icon::from_rgba(rgba, 32, 32).expect("tray icon")
}

/// Decode a window icon (a `data:image/png;base64,…` URL from the engine) into a
/// Slint image for a window-picker row. `None` if it isn't a PNG data URL or
/// fails to decode — the row then shows the placeholder glyph.
fn icon_to_image(data_url: &str) -> Option<slint::Image> {
    use base64::Engine;
    let b64 = data_url.strip_prefix("data:image/png;base64,")?;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?;
    let rgba = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h);
    Some(slint::Image::from_rgba8(buf))
}

/// (Re)start the post-capture toast's auto-dismiss. Restarting cancels any
/// pending dismiss from an earlier capture, so a fresh toast gets a full window.
fn arm_auto_dismiss(actions: &slint::Weak<ActionsWindow>, dismiss: &slint::Timer) {
    let w = actions.clone();
    dismiss.start(slint::TimerMode::SingleShot, Duration::from_secs(load_settings().dismiss_secs()), move || {
        if let Some(a) = w.upgrade() {
            let _ = a.hide();
        }
    });
}

/// Save a capture, copy to clipboard, stash its path, and show the actions
/// window. The stashed path is what the post-capture buttons act on.
fn present_capture(
    actions: &slint::Weak<ActionsWindow>,
    current: &Rc<RefCell<Option<PathBuf>>>,
    img: &CapturedImage,
    dismiss: &slint::Timer,
) {
    let s = load_settings();
    let jpg = s.image_format == 1;
    let path = capture_path(if jpg { "jpg" } else { "png" });
    let saved = if jpg {
        save_jpeg(img, &path, CAPTURE_JPEG_QUALITY)
    } else {
        save_png(img, &path)
    };
    if let Err(e) = saved {
        tracing::error!(path = %path.display(), error = %e, "save capture");
    }
    let _ = clipo_capture::save_thumbnail_jpeg(img, &thumb_path(&path)); // regenerated on demand
    if s.copy_enabled() {
        if let Err(e) = copy_image_to_clipboard(img) {
            tracing::warn!(error = %e, "copy capture to clipboard");
        }
    }
    *current.borrow_mut() = Some(path.clone());
    if let Some(a) = actions.upgrade() {
        if let Ok(thumb) = slint::Image::load_from_path(&path) {
            a.set_thumbnail(thumb);
        }
        a.set_is_video(false);
        a.set_gif_ready(false); // images don't export to GIF
        a.set_upload_ready(s.upload_ready());
        a.set_upload_status(0); // clear any banner from a prior capture
        show_bottom_right(&a);
        arm_auto_dismiss(actions, dismiss);
    }
}

/// Open a file in the OS default app (no console flash, unlike `cmd start`).
fn open_in_default_app(path: &Path) {
    let file = windows::core::HSTRING::from(path.to_string_lossy().as_ref());
    // SAFETY: `file` outlives the call; ShellExecuteW only reads the pointers.
    unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("open"),
            windows::core::PCWSTR(file.as_ptr()),
            windows::core::PCWSTR::null(),
            windows::core::PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

/// Native folder picker for the capture-folder setting (blocking, UI thread).
fn pick_folder() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Choose capture folder")
        .pick_folder()
}

/// Reveal a file in Explorer (selected).
fn reveal_in_explorer(path: &Path) {
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
}

/// Put text on the clipboard.
pub(crate) fn copy_text(text: &str) {
    if let Ok(mut clipboard) = arboard::Clipboard::new() {
        if let Err(e) = clipboard.set_text(text) {
            tracing::warn!(error = %e, "copy text to clipboard");
        }
    }
}

const MEDIA_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp", "gif", "mp4"];

pub(crate) fn is_media(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| MEDIA_EXTS.contains(&e.to_ascii_lowercase().as_str()))
}

pub(crate) fn is_video(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
}

pub(crate) fn is_gif(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("gif"))
}

/// All media files in `dir` (unsorted) — each caller sorts as it needs.
pub(crate) fn media_files_in(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_media(p))
        .collect()
}

/// All media files in `path`'s folder (sorted by name) + the index of `path`,
/// so the viewer can browse siblings with ← →.
fn list_siblings(path: &Path) -> (Vec<PathBuf>, usize) {
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut files = media_files_in(&dir);
    files.sort_by_key(|p| {
        p.file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    });
    let idx = files.iter().position(|p| p == path).unwrap_or(0);
    if files.is_empty() {
        (vec![path.to_path_buf()], 0)
    } else {
        (files, idx)
    }
}

type ViewerState = Rc<RefCell<(Vec<PathBuf>, usize)>>;

fn current_viewer_path(state: &ViewerState) -> Option<PathBuf> {
    let st = state.borrow();
    st.0.get(st.1).cloned()
}

/// Load the current sibling into the viewer (image), or flag it as video.
fn refresh_viewer(viewer: &ViewerWindow, state: &ViewerState) {
    viewer.set_upload_status(0); // clear any banner from a prior image
    let st = state.borrow();
    let Some(path) = st.0.get(st.1) else { return };
    let video = is_video(path);
    viewer.set_is_video(video);
    viewer.set_is_gif(is_gif(path));
    if video {
        viewer.set_media(slint::Image::default());
    } else if let Ok(img) = slint::Image::load_from_path(path) {
        viewer.set_media(img); // first frame, shown instantly; gif then animates
    }
    viewer.set_uploaded(settings::is_uploaded(path));
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    viewer.set_filename(name.into());
    viewer.set_counter(format!("{} / {}", st.1 + 1, st.0.len()).into());
    viewer.set_can_prev(st.1 > 0);
    viewer.set_can_next(st.1 + 1 < st.0.len());
    sync_gif_playback(viewer, path);
}

thread_local! {
    // Viewer GIF animation: a lazy frame iterator + the timer driving it. Only
    // one viewer exists, so a single slot suffices. Streaming (one decoded frame
    // resident at a time) keeps memory flat regardless of clip length.
    static GIF: RefCell<GifAnim> = RefCell::new(GifAnim::default());
}

#[derive(Default)]
struct GifAnim {
    frames: Option<image::Frames<'static>>,
    path: Option<PathBuf>,
    timer: slint::Timer,
}

/// Open `path` as a lazy GIF frame iterator — frames are decoded one-by-one on
/// `next()`, so only the current frame is ever held in memory.
fn open_gif_frames(path: &Path) -> Option<image::Frames<'static>> {
    let file = std::fs::File::open(path).ok()?;
    let dec = image::codecs::gif::GifDecoder::new(std::io::BufReader::new(file)).ok()?;
    Some(image::AnimationDecoder::into_frames(dec))
}

/// One composited RGBA frame → a Slint image (a single copy into a shared buffer).
fn frame_to_image(frame: &image::Frame) -> slint::Image {
    let buf = frame.buffer();
    let mut pb = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(buf.width(), buf.height());
    pb.make_mut_bytes().copy_from_slice(buf.as_raw());
    slint::Image::from_rgba8(pb)
}

/// The gif's frame interval, read from its first frame (uniform for our 15-fps
/// exports), defaulting to 15 fps when a frame carries no delay.
fn gif_interval(path: &Path) -> Duration {
    open_gif_frames(path)
        .and_then(|mut f| f.next())
        .and_then(Result::ok)
        .map(|fr| Duration::from(fr.delay()))
        .filter(|d| !d.is_zero())
        .unwrap_or(Duration::from_millis(66))
}

/// Stop and release the viewer's GIF animation (timer + open file).
fn stop_gif_playback() {
    GIF.with(|g| {
        let mut g = g.borrow_mut();
        g.timer.stop();
        g.frames = None;
        g.path = None;
    });
}

/// Start (for a gif) or stop (for anything else) the viewer's frame animation.
/// Static images never pay for this — the timer only runs while a gif is shown.
fn sync_gif_playback(viewer: &ViewerWindow, path: &Path) {
    if !is_gif(path) {
        stop_gif_playback();
        return;
    }
    let Some(frames) = open_gif_frames(path) else { return };
    let interval = gif_interval(path);
    GIF.with(|g| {
        let mut g = g.borrow_mut();
        g.frames = Some(frames);
        g.path = Some(path.to_path_buf());
    });
    let weak = viewer.as_weak();
    GIF.with(|g| {
        g.borrow().timer.start(slint::TimerMode::Repeated, interval, move || {
            let Some(v) = weak.upgrade() else { return };
            let frame = GIF.with(|g| {
                let mut g = g.borrow_mut();
                // Next frame; at the end, re-open the decoder once to loop.
                if let Some(Ok(fr)) = g.frames.as_mut().and_then(Iterator::next) {
                    return Some(fr);
                }
                let p = g.path.clone()?;
                g.frames = open_gif_frames(&p);
                g.frames.as_mut().and_then(Iterator::next).and_then(Result::ok)
            });
            if let Some(fr) = frame {
                v.set_media(frame_to_image(&fr));
            }
        });
    });
}

/// Sidecar thumbnail path: `%LOCALAPPDATA%\Clipo\thumbs\<filename>.jpg`.
pub(crate) fn thumb_path(src: &Path) -> PathBuf {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Clipo")
        .join("thumbs");
    let _ = std::fs::create_dir_all(&dir);
    let name = src.file_name().and_then(|s| s.to_str()).unwrap_or("thumb");
    dir.join(format!("{name}.jpg"))
}

/// The capture folder — the user's chosen one, or `Pictures\Clipo` by default.
pub(crate) fn capture_dir() -> PathBuf {
    load_settings()
        .capture_folder
        .filter(|s| !s.is_empty()).map_or_else(|| {
            dirs::picture_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("Clipo")
        }, PathBuf::from)
}

/// `<capture-folder>\clipo-<ms>.<ext>`, creating the folder.
pub(crate) fn capture_path(ext: &str) -> PathBuf {
    let base = capture_dir();
    let _ = std::fs::create_dir_all(&base);
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    base.join(format!("clipo-{ms}.{ext}"))
}

/// Open the lightbox viewer on `path`, browsing its folder siblings.
#[allow(clippy::needless_pass_by_value)] // `path` is the item being opened
fn open_in_viewer(viewer: &slint::Weak<ViewerWindow>, state: &ViewerState, path: PathBuf) {
    let (files, idx) = list_siblings(&path);
    *state.borrow_mut() = (files, idx);
    if let Some(v) = viewer.upgrade() {
        refresh_viewer(&v, state);
        v.set_upload_configured(load_settings().upload_ready());
        let _ = v.show();
        raise_to_front(v.window()); // restore if minimized + raise over the foreground
        disable_dwm_transitions(v.window());
        v.set_is_maximized(true);
        if !maximize_work_area(v.window()) {
            let vw = v.as_weak();
            slint::Timer::single_shot(Duration::ZERO, move || {
                if let Some(v) = vw.upgrade() {
                    disable_dwm_transitions(v.window());
                    maximize_work_area(v.window());
                }
            });
        }
    }
}

/// True if Windows is set to dark mode (AppsUseLightTheme = 0). Drives the
/// "System" theme option; defaults to dark if the key is missing.
fn os_prefers_dark() -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
        .map_or(true, |v| v == 0)
}

/// Wires the shared WindowControls for a framed window: minimize, and a
/// maximize/restore toggle (restore returns to the given logical size, centred).
/// Close stays per-window via its existing `dismiss` callback.
macro_rules! wire_controls {
    // No-persist windows (e.g. the viewer): delegate with a no-op save.
    ($w:ident, $rw:expr, $rh:expr) => {
        wire_controls!($w, $rw, $rh, |_maximized: bool| {});
    };
    // `$save` runs after each maximize/restore toggle with the new state, so a
    // window can remember it across sessions.
    ($w:ident, $rw:expr, $rh:expr, $save:expr) => {
        $w.on_minimize({
            let h = $w.as_weak();
            move || { if let Some(e) = h.upgrade() { e.window().set_minimized(true); } }
        });
        $w.on_maximize({
            let h = $w.as_weak();
            let save = $save;
            move || {
                if let Some(e) = h.upgrade() {
                    let maximized = !e.get_is_maximized();
                    if maximized {
                        maximize_work_area(e.window());
                    } else {
                        restore_window(e.window(), $rw, $rh);
                    }
                    e.set_is_maximized(maximized);
                    save(maximized);
                }
            }
        });
    };
}

/// Wires a window's `drag-move` callback (emitted by a header DragArea) to move
/// the window by the dragged delta — drag a borderless window by its title bar.
macro_rules! wire_drag {
    ($w:expr) => {{
        let w = $w.as_weak();
        $w.on_drag_move(move |dx, dy| {
            if let Some(win) = w.upgrade() {
                let scale = win.window().scale_factor();
                let pos = win.window().position();
                win.window().set_position(slint::PhysicalPosition::new(
                    pos.x + (dx * scale) as i32,
                    pos.y + (dy * scale) as i32,
                ));
            }
        });
    }};
}

/// Handler for a second launch forwarded over the single-instance pipe.
type ReopenFn = Box<dyn Fn(Option<PathBuf>)>;

thread_local! {
    // Set once the windows exist. A later launch, forwarded over the
    // single-instance pipe, calls this on the UI thread: Some(path) opens the
    // viewer, None surfaces the all-in-one menu (CleanShot convention).
    static REOPEN: RefCell<Option<ReopenFn>> = const { RefCell::new(None) };
}

thread_local! {
    // The annotation editor lives here so the theme toggle (and reopen) can
    // reach it. Built eagerly at startup rather than lazily: a freshly-built
    // window isn't laid out on its first show, which left its first open
    // off-centre under show_centered.
    static EDITOR: RefCell<Option<EditorWindow>> = const { RefCell::new(None) };
}

/// Open `path` in the annotation editor (built once at startup, reused here).
fn open_editor_lazy(state: &Rc<RefCell<EditorState>>, path: PathBuf) {
    let weak = EDITOR.with(|cell| {
        cell.borrow().as_ref().expect("editor built at startup").as_weak()
    });
    open_editor(&weak, state, path);
}

/// Open the history window, restoring its remembered window/maximized state.
fn open_history(h: &HistoryWindow) {
    let maximized = load_settings().history_maximized;
    h.set_is_maximized(maximized);
    show_remembered(h, 820.0, 560.0, maximized);
}

thread_local! {
    // OCR result window — also built lazily (most sessions don't run OCR).
    static OCR: RefCell<Option<OcrWindow>> = const { RefCell::new(None) };
    static OCR_DARK: Cell<bool> = const { Cell::new(true) };
    // The newer release found by the update check, consumed by the install
    // action. A thread_local (not an Rc) so the worker-thread closures stay
    // Send — they only touch it from the UI thread, by name.
    static PENDING_UPDATE: RefCell<Option<UpdateInfo>> = const { RefCell::new(None) };
}

/// The OCR window's own callbacks (minimize/dismiss/copy + drag).
fn wire_ocr(ocr_w: &OcrWindow) {
    ocr_w.on_minimize({
        let h = ocr_w.as_weak();
        move || { if let Some(e) = h.upgrade() { e.window().set_minimized(true); } }
    });
    ocr_w.on_dismiss({
        let w = ocr_w.as_weak();
        move || { if let Some(w) = w.upgrade() { let _ = w.hide(); } }
    });
    ocr_w.on_copy_text({
        let w = ocr_w.as_weak();
        move || {
            if let Some(w) = w.upgrade() {
                copy_text(w.get_text().as_str());
                w.set_copied(true); // button flashes a check + "Copied"
                revert_after(&w, 1200, |w| w.set_copied(false));
            }
        }
    });
    wire_drag!(ocr_w);
}

/// Run OCR on `path`, building + wiring + theming the result window on first
/// use (then reused) instead of creating it eagerly at startup.
fn open_ocr_lazy(path: PathBuf) {
    let weak = OCR.with(|cell| {
        if cell.borrow().is_none() {
            let w = OcrWindow::new().expect("ocr window");
            wire_ocr(&w);
            w.global::<Theme>().set_dark(OCR_DARK.with(Cell::get));
            *cell.borrow_mut() = Some(w);
        }
        cell.borrow().as_ref().unwrap().as_weak()
    });
    run_ocr_for(path, weak);
}

/// Logging to stderr: warn+ by default; `CLIPO_LOG=debug` for dev detail.
/// info/debug are dev-only (stripped per slice); warn/error are kept. A plain
/// level filter (no env-filter/regex) keeps the binary lean.
fn install_tracing() {
    let level = match std::env::var("CLIPO_LOG").as_deref() {
        Ok("trace") => tracing::Level::TRACE,
        Ok("debug") => tracing::Level::DEBUG,
        Ok("info") => tracing::Level::INFO,
        Ok("error") => tracing::Level::ERROR,
        _ => tracing::Level::WARN,
    };
    let _ = tracing_subscriber::fmt().with_max_level(level).with_target(false).compact().try_init();
}

fn main() -> Result<(), slint::PlatformError> {
    install_tracing();
    // Single instance: if another Clipo already owns the pipe, forward our
    // argument (image path, or empty) to it and exit — no stacked tray copies.
    let forward = std::env::args()
        .skip(1)
        .map(PathBuf::from)
        .find(|p| is_media(p) && p.is_file())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    // `--updated` marks the relaunch after a self-update: the prior instance is
    // exiting and about to release the single-instance pipe, so wait for it
    // rather than bail as a secondary (which would leave nothing running).
    let updated = std::env::args().any(|a| a == "--updated");
    let acquired = if updated {
        single_instance::acquire_after_update(&forward)
    } else {
        single_instance::acquire(&forward)
    };
    let ipc_server = match acquired {
        single_instance::Instance::Secondary => return Ok(()),
        single_instance::Instance::Primary(server) => server,
    };
    // If a hardware-encoded recording crashed the process last run, its in-flight
    // sentinel survived (a clean finish clears it) — fall back to the software
    // encoder from here on, the way Chromium drops to software after a GPU-process
    // crash. One-time: the sentinel is cleared once acted upon.
    {
        let sentinel = settings::encoder_sentinel_path();
        if sentinel.exists() {
            let mut s = load_settings();
            if !s.software_encoder {
                s.software_encoder = true;
                save_settings(&s);
                tracing::warn!("recording: hardware encoder crashed last run; falling back to software");
            }
            let _ = std::fs::remove_file(&sentinel);
        }
    }
    let menu_w = MenuWindow::new()?;
    // Apply the saved UI language. Slint requires select_bundled_translation to
    // run *after* the first component exists (menu_w above); the @tr strings then
    // re-render reactively and every window built below picks it up. Calling it
    // earlier is a silent no-op — the UI would stay English despite a saved pt/es/…
    {
        let lang = load_settings().language;
        if !lang.is_empty() && lang != "en" {
            if let Err(e) = slint::select_bundled_translation(&lang) {
                tracing::warn!(%lang, error = %e, "select_bundled_translation");
            }
        }
    }
    let actions_w = ActionsWindow::new()?;
    let settings_w = SettingsWindow::new()?;
    let history_w = HistoryWindow::new()?;
    // ocr_w is built lazily on first OCR (see open_ocr_lazy / OCR).
    let viewer_w = ViewerWindow::new()?;
    let rec_bar = RecordingBarWindow::new()?;
    // editor_w is built eagerly just below (after editor_state) — see EDITOR.
    let tray_w = TrayMenuWindow::new()?;
    // Viewer browse list + index (used by history/actions to open the viewer).
    let viewer_state: ViewerState = Rc::new(RefCell::new((Vec::new(), 0)));
    // Active recording (recorder + output path); None when idle.
    let recorder: Rc<RefCell<Option<(VideoRecorder, PathBuf, Rect)>>> = Rc::new(RefCell::new(None));
    // Annotation editor state.
    let editor_state: Rc<RefCell<EditorState>> = Rc::new(RefCell::new(EditorState::default()));
    // Build the editor eagerly (stored in EDITOR) so its first open is centred —
    // a lazily-built window isn't laid out on its first show, so show_centered
    // would place it from stale metrics. apply_theme (below) themes it.
    {
        let editor_w = EditorWindow::new()?;
        wire_editor(&editor_w, &editor_state);
        wire_controls!(editor_w, 900.0, 620.0, |maximized: bool| {
            let mut s = load_settings();
            s.editor_maximized = maximized;
            save_settings(&s);
        });
        EDITOR.with(|cell| *cell.borrow_mut() = Some(editor_w));
    }

    // Dismiss (Esc / close button) hides the window so the tray can reopen it.
    menu_w.on_dismiss({
        let w = menu_w.as_weak();
        move || { if let Some(w) = w.upgrade() { let _ = w.hide(); } }
    });
    actions_w.on_dismiss({
        let w = actions_w.as_weak();
        move || { if let Some(w) = w.upgrade() { let _ = w.hide(); } }
    });
    settings_w.on_dismiss({
        let w = settings_w.as_weak();
        move || { if let Some(w) = w.upgrade() { let _ = w.hide(); } }
    });
    history_w.on_dismiss({
        let w = history_w.as_weak();
        move || { if let Some(w) = w.upgrade() { let _ = w.hide(); } }
    });
    wire_controls!(history_w, 820.0, 560.0, |maximized: bool| {
        let mut s = load_settings();
        s.history_maximized = maximized;
        save_settings(&s);
    });
    wire_controls!(viewer_w, 1280.0, 800.0);
    settings_w.on_minimize({
        let h = settings_w.as_weak();
        move || { if let Some(e) = h.upgrade() { e.window().set_minimized(true); } }
    });
    // History grid actions (filter/delete/gif/edit/ocr/copy/reveal/open).
    wire_history(&history_w, &viewer_w, &viewer_state, &editor_state);

    // Real capture engine (DXGI worker thread) — reused from the Clipo backend.
    let engine = CaptureEngine::start();
    let handle = match &engine {
        Ok(e) => Some(e.handle()),
        Err(e) => {
            tracing::error!(error = %e, "capture engine failed to start");
            None
        }
    };

    // Path of the last saved capture — what the post-capture buttons act on.
    let current: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
    // Auto-dismiss timer for the post-capture toast (shared, restarted per show).
    let actions_dismiss = Rc::new(slint::Timer::default());

    // "Fullscreen" → hide the menu, grab the primary monitor, save + show actions.
    menu_w.on_capture_fullscreen({
        let menu = menu_w.as_weak();
        let actions = actions_w.as_weak();
        let handle = handle.clone();
        let current = current.clone();
        let actions_dismiss = actions_dismiss.clone();
        move || {
            if let Some(m) = menu.upgrade() {
                disable_dwm_transitions(m.window()); // kill the close fade so it's
                let _ = m.hide(); // gone (not mid-fade) before the 40ms grab
            }
            let actions = actions.clone();
            let handle = handle.clone();
            let current = current.clone();
            let actions_dismiss = actions_dismiss.clone();
            // Wait for the menu to leave the screen before grabbing it.
            slint::Timer::single_shot(Duration::from_millis(40), move || {
                let Some(h) = handle.as_ref() else { return };
                match h.capture_primary() {
                    Ok(img) => present_capture(&actions, &current, &img, &actions_dismiss),
                    Err(e) => tracing::error!(error = %e, "capture_primary"),
                }
            });
        }
    });

    // Post-capture actions — all operate on the last saved file.
    wire_actions(&actions_w, &current, &viewer_w, &viewer_state, &editor_state, &actions_dismiss);

    // Viewer (lightbox) — browse siblings, copy/ocr/reveal the current one.
    wire_viewer(&viewer_w, &viewer_state, &editor_state);

    // The region-selection overlay is a dedicated Win32 layered window (the
    // clipo-overlay crate) rather than a Slint window: it freezes the desktop and
    // composites the dim + magnifier + selection with BitBlt/UpdateLayeredWindow,
    // so it never flashes (no per-frame scene rebuild). Results arrive on a
    // channel.
    let overlay = Rc::new(Overlay::spawn().expect("spawn clipo-overlay"));

    // Global hotkeys — registered on the main thread from the saved/default
    // combos; events arrive on a channel polled in the tray-menu timer. Shared
    // so the Settings rebind handler can re-register live and the recording
    // callbacks can add/drop their session F-keys.
    let hk_state = Rc::new(RefCell::new(HkState {
        mgr: global_hotkey::GlobalHotKeyManager::new().ok(),
        by_id: std::collections::HashMap::new(),
        current: [None; 6],
        intended: [const { String::new() }; 6],
        rec_by_id: std::collections::HashMap::new(),
        rec_keys: Vec::new(),
    }));
    {
        let saved = load_settings();
        let mut st = hk_state.borrow_mut();
        for i in 0..6 {
            let combo = saved
                .shortcuts
                .get(HK_IDS[i])
                .cloned()
                .unwrap_or_else(|| HK_DEFAULTS[i].to_string());
            st.bind(i, &combo);
        }
    }

    // Recording can target the full screen or a dragged region. `overlay_records`
    // routes the overlay's selection: true → start a recording of the rect,
    // false → screenshot it. `start_recording` begins a capture of any rect + shows
    // the bar; `open_region_overlay` shows the clipo-overlay selection window
    // (callers set `overlay_records` and hide their own menu first).
    let overlay_records = Rc::new(Cell::new(false));
    // Same overlay, but the selection runs OCR on the crop instead of saving it.
    let overlay_ocr = Rc::new(Cell::new(false));
    let start_recording: Rc<dyn Fn(Rect)> = {
        let bar = rec_bar.as_weak();
        let overlay = overlay.clone();
        let recorder = recorder.clone();
        let hk = hk_state.clone();
        Rc::new(move |rect: Rect| {
            if recorder.borrow().is_some() {
                return; // already recording
            }
            if let Some(state) = begin_recording(rect) {
                *recorder.borrow_mut() = Some(state);
                hk.borrow_mut().register_recording();
                // Mark the captured area on screen (a hole-punched ring just
                // outside the rect, so it isn't part of the recording) — the
                // overlay's click-through recording indicator.
                overlay.show_recording_indicator(rect);
                if let Some(b) = bar.upgrade() {
                    let s = load_settings();
                    b.set_elapsed(0);
                    b.set_paused(false);
                    b.set_audio_muted(false);
                    b.set_mic_muted(false);
                    b.set_audio_on(s.audio_enabled());
                    b.set_mic_on(s.capture_mic);
                    b.set_active(true);
                    let _ = b.show();
                    disable_dwm_transitions(b.window());
                    if !place_bottom_center(b.window()) {
                        let bw = b.as_weak();
                        slint::Timer::single_shot(Duration::ZERO, move || {
                            if let Some(b) = bw.upgrade() {
                                disable_dwm_transitions(b.window());
                                place_bottom_center(b.window());
                            }
                        });
                    }
                }
            }
        })
    };
    // clipo-overlay posts Confirmed(rect)/Cancelled from its own thread over a
    // channel. A repeated timer drains it on the UI thread WHILE a selection is
    // live and stops itself on the first event — so idle stays poll-free. `rect`
    // is already virtual-desktop pixels, so the pixels come straight from a fresh
    // `capture_region` (the overlay's freeze is display-only).
    let overlay_events = overlay.events();
    let overlay_drain = Rc::new(slint::Timer::default());
    let open_region_overlay: Rc<dyn Fn()> = {
        let overlay = overlay.clone();
        let drain = overlay_drain;
        let events = overlay_events;
        let handle = handle.clone();
        let actions = actions_w.as_weak();
        let current = current.clone();
        let actions_dismiss = actions_dismiss.clone();
        let records = overlay_records.clone();
        let ocr_mode = overlay_ocr.clone();
        let start_recording = start_recording.clone();
        Rc::new(move || {
            overlay.toggle(load_settings().magnifier_enabled());
            let overlay = overlay.clone();
            let drain_stop = drain.clone();
            let events = events.clone();
            let handle = handle.clone();
            let actions = actions.clone();
            let current = current.clone();
            let actions_dismiss = actions_dismiss.clone();
            let records = records.clone();
            let ocr_mode = ocr_mode.clone();
            let start_recording = start_recording.clone();
            drain.start(slint::TimerMode::Repeated, Duration::from_millis(16), move || {
                let Ok(event) = events.try_recv() else { return };
                overlay.mark_hidden();
                drain_stop.stop();
                let rect = match event {
                    OverlayEvent::Confirmed(r) => r,
                    OverlayEvent::Cancelled => {
                        records.set(false);
                        ocr_mode.set(false);
                        return;
                    }
                };
                if records.replace(false) {
                    // Defer so the overlay is fully gone before the recorder grabs
                    // its first frame (no dim/ring in it).
                    let start = start_recording.clone();
                    slint::Timer::single_shot(Duration::from_millis(120), move || start(rect));
                    return;
                }
                let Some(h) = handle.as_ref() else { return };
                let img = match h.capture_region(rect) {
                    Ok(i) => i,
                    Err(e) => {
                        tracing::error!(error = %e, ?rect, "region capture");
                        return;
                    }
                };
                if ocr_mode.replace(false) {
                    // OCR isn't a screenshot: PNG to a temp file (no history entry),
                    // then recognise off-thread.
                    let tmp = std::env::temp_dir().join("clipo-ocr.png");
                    if save_png(&img, &tmp).is_ok() {
                        open_ocr_lazy(tmp);
                    }
                } else {
                    present_capture(&actions, &current, &img, &actions_dismiss);
                }
            });
        })
    };
    // OCR a dragged region: arm OCR mode, then open the same selection overlay.
    let start_ocr_region: Rc<dyn Fn()> = {
        let ocr = overlay_ocr;
        let open = open_region_overlay.clone();
        Rc::new(move || {
            ocr.set(true);
            open();
        })
    };

    menu_w.on_capture_region({
        let menu = menu_w.as_weak();
        let records = overlay_records.clone();
        let open = open_region_overlay.clone();
        move || {
            records.set(false);
            if let Some(m) = menu.upgrade() {
                disable_dwm_transitions(m.window()); // kill the close fade so the
                let _ = m.hide(); // menu isn't caught mid-fade in the grab
            }
            open();
        }
    });
    // "Record Region" → same drag-to-select overlay, but the selection starts a
    // recording of the rect instead of a screenshot.
    menu_w.on_capture_record_region({
        let menu = menu_w.as_weak();
        let records = overlay_records;
        let open = open_region_overlay.clone();
        let recorder = recorder.clone();
        move || {
            if recorder.borrow().is_some() {
                return; // already recording
            }
            records.set(true);
            if let Some(m) = menu.upgrade() {
                let _ = m.hide();
            }
            open();
        }
    });

    // "Capture window" → a list of every running top-level window (icon + title
    // + resolution). Click a row → focus that window and capture its bounds. The
    // row index maps back to `win_list` (HWNDs are i64, wider than Slint's i32).
    let picker = WindowPickerWindow::new()?;
    let win_list: Rc<RefCell<Vec<WindowInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let win_model = Rc::new(slint::VecModel::<WinRow>::default());
    picker.set_windows(win_model.clone().into());
    wire_drag!(picker);
    menu_w.on_capture_window({
        let menu = menu_w.as_weak();
        let picker = picker.as_weak();
        let win_list = win_list.clone();
        let model = win_model;
        move || {
            if let Some(m) = menu.upgrade() {
                let _ = m.hide();
            }
            let picker = picker.clone();
            let win_list = win_list.clone();
            let model = model.clone();
            // Defer so the menu is fully gone before we enumerate + show.
            slint::Timer::single_shot(Duration::from_millis(40), move || {
                let wins = enumerate_capturable_windows();
                let rows: Vec<WinRow> = wins
                    .iter()
                    .map(|w| {
                        // Icons resolve inline (a few ms each) — fine for the
                        // handful of top-level windows a desktop has open.
                        let icon = extract_window_icon(w.id).as_deref().and_then(icon_to_image);
                        WinRow {
                            title: w.title.clone().into(),
                            resolution: if w.width > 0 && w.height > 0 {
                                format!("{} × {}", w.width, w.height).into()
                            } else {
                                slint::SharedString::new()
                            },
                            has_icon: icon.is_some(),
                            icon: icon.unwrap_or_default(),
                        }
                    })
                    .collect();
                model.set_vec(rows);
                *win_list.borrow_mut() = wins;
                if let Some(p) = picker.upgrade() {
                    show_centered(&p, 540.0, 460.0);
                }
            });
        }
    });
    picker.on_picked({
        let picker = picker.as_weak();
        let actions = actions_w.as_weak();
        let current = current.clone();
        let actions_dismiss = actions_dismiss.clone();
        let handle = handle.clone();
        move |index| {
            if let Some(p) = picker.upgrade() {
                let _ = p.hide();
            }
            let Some(info) = win_list.borrow().get(index as usize).cloned() else {
                return;
            };
            // Bring the window forward, read its bounds, then grab it once it has
            // finished coming up (so the capture isn't of a stale/occluded frame).
            let Some(rect) = focus_window_and_bounds(info.id) else {
                return;
            };
            let handle = handle.clone();
            let actions = actions.clone();
            let current = current.clone();
            let actions_dismiss = actions_dismiss.clone();
            slint::Timer::single_shot(Duration::from_millis(120), move || {
                let Some(h) = handle.as_ref() else { return };
                match h.capture_region(rect) {
                    Ok(img) => present_capture(&actions, &current, &img, &actions_dismiss),
                    Err(e) => tracing::error!(error = %e, "window capture"),
                }
            });
        }
    });
    picker.on_dismiss({
        let picker = picker.as_weak();
        move || {
            if let Some(p) = picker.upgrade() {
                let _ = p.hide();
            }
        }
    });

    // "Timer" → centered countdown, then a fullscreen grab (time to arrange the
    // screen / open a menu that would close on focus loss). Click/Esc cancels.
    let countdown = CountdownWindow::new()?;
    let countdown_timer = Rc::new(slint::Timer::default());
    menu_w.on_capture_timer({
        let menu = menu_w.as_weak();
        let cd = countdown.as_weak();
        let timer = countdown_timer.clone();
        let actions = actions_w.as_weak();
        let current = current.clone();
        let handle = handle.clone();
        let actions_dismiss = actions_dismiss.clone();
        move || {
            if let Some(m) = menu.upgrade() {
                let _ = m.hide();
            }
            let Some(c) = cd.upgrade() else { return };
            c.set_seconds(load_settings().timer_secs());
            show_centered(&c, 200.0, 200.0);
            let cd = cd.clone();
            let timer2 = timer.clone();
            let actions = actions.clone();
            let current = current.clone();
            let handle = handle.clone();
            let actions_dismiss = actions_dismiss.clone();
            timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
                let Some(c) = cd.upgrade() else { return };
                let s = c.get_seconds() - 1;
                if s <= 0 {
                    timer2.stop();
                    let _ = c.hide();
                    // Capture once the countdown has left the screen.
                    let actions = actions.clone();
                    let current = current.clone();
                    let handle = handle.clone();
                    let actions_dismiss = actions_dismiss.clone();
                    slint::Timer::single_shot(Duration::from_millis(60), move || {
                        let Some(h) = handle.as_ref() else { return };
                        match h.capture_primary() {
                            Ok(img) => present_capture(&actions, &current, &img, &actions_dismiss),
                            Err(e) => tracing::error!(error = %e, "capture_primary timer"),
                        }
                    });
                } else {
                    c.set_seconds(s);
                }
            });
        }
    });
    countdown.on_cancelled({
        let cd = countdown.as_weak();
        let timer = countdown_timer.clone();
        move || {
            timer.stop();
            if let Some(c) = cd.upgrade() {
                let _ = c.hide();
            }
        }
    });

    // Begin recording the primary monitor (fullscreen) + show the bar. Shared by
    // the direct path and the optional pre-recording countdown; region recording
    // goes through `start_recording` directly with the selected rect.
    let record_now: Rc<dyn Fn()> = {
        let start = start_recording.clone();
        Rc::new(move || {
            // SAFETY: GetSystemMetrics takes only a constant index, no pointers.
            let (sw, sh) = unsafe { (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN)) };
            start(Rect { x: 0, y: 0, width: sw as u32 & !1, height: sh as u32 & !1 });
        })
    };
    // "Record Screen" → optional 3-2-1 countdown, then record + show the bar.
    menu_w.on_capture_ocr({
        let menu = menu_w.as_weak();
        let ocr = start_ocr_region.clone();
        move || {
            if let Some(m) = menu.upgrade() {
                disable_dwm_transitions(m.window()); // kill the close fade before the grab
                let _ = m.hide();
            }
            ocr();
        }
    });
    menu_w.on_capture_record({
        let menu = menu_w.as_weak();
        let cd = countdown.as_weak();
        let timer = countdown_timer;
        let record_now = record_now.clone();
        let recorder = recorder.clone();
        move || {
            if recorder.borrow().is_some() {
                return; // already recording
            }
            if let Some(m) = menu.upgrade() {
                let _ = m.hide();
            }
            if load_settings().recording_countdown {
                let Some(c) = cd.upgrade() else { return };
                c.set_seconds(3);
                show_centered(&c, 200.0, 200.0);
                let cd = cd.clone();
                let timer2 = timer.clone();
                let record_now = record_now.clone();
                timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
                    let Some(c) = cd.upgrade() else { return };
                    let s = c.get_seconds() - 1;
                    if s <= 0 {
                        timer2.stop();
                        let _ = c.hide();
                        let rn = record_now.clone();
                        slint::Timer::single_shot(Duration::from_millis(250), move || rn());
                    } else {
                        c.set_seconds(s);
                    }
                });
            } else {
                let rn = record_now.clone();
                slint::Timer::single_shot(Duration::from_millis(250), move || rn());
            }
        }
    });
    menu_w.on_open_history({
        let menu = menu_w.as_weak();
        let hist = history_w.as_weak();
        move || {
            if let Some(m) = menu.upgrade() {
                let _ = m.hide();
            }
            if let Some(h) = hist.upgrade() {
                open_history(&h);
                populate_history(&h.as_weak());
            }
        }
    });
    menu_w.on_open_settings({
        let menu = menu_w.as_weak();
        let sw = settings_w.as_weak();
        let hk = hk_state.clone();
        move || {
            if let Some(m) = menu.upgrade() {
                let _ = m.hide();
            }
            if let Some(s) = sw.upgrade() {
                hk.borrow_mut().refresh();
                s.set_sc_conflicts(shortcut_conflicts(&hk.borrow()));
                show_fitted(&s, 960.0);
            }
        }
    });

    // Stop → finalise the mp4, grab a poster frame, show the actions panel.
    rec_bar.on_stop({
        let bar = rec_bar.as_weak();
        let overlay = overlay.clone();
        let actions = actions_w.as_weak();
        let recorder = recorder.clone();
        let handle = handle;
        let actions_dismiss = actions_dismiss;
        let hk = hk_state.clone();
        move || {
            if let Some(b) = bar.upgrade() {
                b.set_active(false);
                let _ = b.hide();
            }
            overlay.hide_recording_indicator();
            hk.borrow_mut().unregister_recording();
            if let Some((rec, path, rect)) = recorder.borrow_mut().take() {
                if let Err(e) = rec.stop() {
                    tracing::warn!(error = %e, "recording stop");
                }
                // Poster: DXGI is free now; grab one frame of the recorded rect.
                if let Some(h) = handle.as_ref() {
                    if let Ok(frame) = h.capture_region(rect) {
                        let _ = clipo_capture::save_thumbnail_jpeg(&frame, &thumb_path(&path));
                    }
                }
                *current.borrow_mut() = Some(path.clone());
                if let Some(a) = actions.upgrade() {
                    a.set_thumbnail(slint::Image::load_from_path(&thumb_path(&path)).unwrap_or_default());
                    a.set_is_video(true);
                    a.set_gif_ready(gif_available());
                    a.set_upload_status(0);
                    show_bottom_right(&a);
                    arm_auto_dismiss(&actions, &actions_dismiss);
                }
            }
        }
    });
    rec_bar.on_pause_resume({
        let bar = rec_bar.as_weak();
        let recorder = recorder.clone();
        move || {
            if let Some((rec, _, _)) = recorder.borrow().as_ref() {
                if rec.is_paused() { rec.resume(); } else { rec.pause(); }
                if let Some(b) = bar.upgrade() {
                    b.set_paused(rec.is_paused());
                }
            }
        }
    });
    rec_bar.on_mute_audio({
        let bar = rec_bar.as_weak();
        let recorder = recorder.clone();
        move || {
            if let Some((rec, _, _)) = recorder.borrow().as_ref() {
                let muted = !rec.is_audio_muted();
                rec.set_audio_muted(muted);
                if let Some(b) = bar.upgrade() {
                    b.set_audio_muted(muted);
                }
            }
        }
    });
    rec_bar.on_mute_mic({
        let bar = rec_bar.as_weak();
        let recorder = recorder.clone();
        move || {
            if let Some((rec, _, _)) = recorder.borrow().as_ref() {
                let muted = !rec.is_mic_muted();
                rec.set_mic_muted(muted);
                if let Some(b) = bar.upgrade() {
                    b.set_mic_muted(muted);
                }
            }
        }
    });
    rec_bar.on_restart({
        let bar = rec_bar.as_weak();
        let recorder = recorder.clone();
        move || {
            let Some((rec, path, rect)) = recorder.borrow_mut().take() else { return };
            let _ = rec.stop();
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(thumb_path(&path));
            if let Some(state) = begin_recording(rect) {
                *recorder.borrow_mut() = Some(state);
                if let Some(b) = bar.upgrade() {
                    b.set_elapsed(0);
                    b.set_paused(false);
                    b.set_audio_muted(false);
                    b.set_mic_muted(false);
                }
            }
        }
    });
    rec_bar.on_discard({
        let bar = rec_bar.as_weak();
        let recorder = recorder.clone();
        let hk = hk_state.clone();
        move || {
            if let Some(b) = bar.upgrade() {
                b.set_active(false);
                let _ = b.hide();
            }
            overlay.hide_recording_indicator();
            hk.borrow_mut().unregister_recording();
            if let Some((rec, path, _)) = recorder.borrow_mut().take() {
                let _ = rec.stop();
                let _ = std::fs::remove_file(&path);
                let _ = std::fs::remove_file(thumb_path(&path));
            }
        }
    });
    // Drag the bar / windows by their empty chrome.
    wire_drag!(rec_bar);
    wire_drag!(history_w);
    wire_drag!(settings_w);
    wire_drag!(viewer_w);


    let _tray = TrayIconBuilder::new()
        .with_tooltip("Clipo Native (Slint)")
        .with_icon(tray_icon_image())
        .build()
        .expect("build tray icon");

    // Custom tray menu (replaces the OS popup): a click shows a themed card near
    // the cursor; picking an item hides the menu and dispatches the action.
    tray_w.on_dismiss({
        let w = tray_w.as_weak();
        move || {
            if let Some(w) = w.upgrade() {
                let _ = w.hide();
            }
        }
    });
    tray_w.on_pick({
        let tw = tray_w.as_weak();
        let mw = menu_w.as_weak();
        let sw = settings_w.as_weak();
        let hw = history_w.as_weak();
        let bw = rec_bar.as_weak();
        let hk = hk_state.clone();
        let ocr_region = start_ocr_region.clone();
        move |id| {
            if let Some(t) = tw.upgrade() {
                let _ = t.hide();
            }
            match id.as_str() {
                "region" => {
                    if let Some(m) = mw.upgrade() {
                        m.invoke_capture_region();
                    }
                }
                "ocr" => ocr_region(),
                "fullscreen" => {
                    if let Some(m) = mw.upgrade() {
                        m.invoke_capture_fullscreen();
                    }
                }
                "window" => {
                    if let Some(m) = mw.upgrade() {
                        m.invoke_capture_window();
                    }
                }
                "timer" => {
                    if let Some(m) = mw.upgrade() {
                        m.invoke_capture_timer();
                    }
                }
                "record" => {
                    if let Some(m) = mw.upgrade() {
                        m.invoke_capture_record();
                    }
                }
                "record-region" => {
                    if let Some(m) = mw.upgrade() {
                        m.invoke_capture_record_region();
                    }
                }
                "menu" => {
                    if let Some(m) = mw.upgrade() {
                        show_centered(&m, 730.0, 350.0);
                    }
                }
                "history" => {
                    if let Some(h) = hw.upgrade() {
                        open_history(&h);
                        populate_history(&h.as_weak());
                    }
                }
                "open-folder" => open_in_default_app(&capture_dir()),
                "settings" => {
                    if let Some(s) = sw.upgrade() {
                        hk.borrow_mut().refresh();
                        s.set_sc_conflicts(shortcut_conflicts(&hk.borrow()));
                        show_fitted(&s, 960.0);
                    }
                }
                // Recording controls — forward to the bar's handlers so there's
                // a single source of truth for stop/pause/restart/discard/mute.
                "recording-stop" => {
                    if let Some(b) = bw.upgrade() {
                        b.invoke_stop();
                    }
                }
                "recording-pause" => {
                    if let Some(b) = bw.upgrade() {
                        b.invoke_pause_resume();
                    }
                }
                "recording-restart" => {
                    if let Some(b) = bw.upgrade() {
                        b.invoke_restart();
                    }
                }
                "recording-discard" => {
                    if let Some(b) = bw.upgrade() {
                        b.invoke_discard();
                    }
                }
                "recording-mute-audio" => {
                    if let Some(b) = bw.upgrade() {
                        b.invoke_mute_audio();
                    }
                }
                "recording-mute-mic" => {
                    if let Some(b) = bw.upgrade() {
                        b.invoke_mute_mic();
                    }
                }
                "quit" => {
                    let _ = slint::quit_event_loop();
                }
                _ => {}
            }
        }
    });

    let mw = menu_w.as_weak();
    let tw = tray_w.as_weak();
    let hk_state_timer = hk_state.clone();
    let bw_hk = rec_bar.as_weak();
    let recorder_tray = recorder;
    let ocr_region = start_ocr_region.clone();
    let timer = slint::Timer::default();
    let mut tray_settle = 0u8; // skip the focus-loss poll for a few ticks after opening
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(80), move || {
        while let Ok(ev) = global_hotkey::GlobalHotKeyEvent::receiver().try_recv() {
            if ev.state != global_hotkey::HotKeyState::Pressed {
                continue;
            }
            let main_idx = hk_state_timer.borrow().by_id.get(&ev.id).copied();
            let rec_code = hk_state_timer.borrow().rec_by_id.get(&ev.id).copied();
            if let (Some(i), Some(m)) = (main_idx, mw.upgrade()) {
                match i {
                    0 => m.invoke_capture_region(),
                    1 => m.invoke_capture_fullscreen(),
                    2 => m.invoke_capture_window(),
                    3 => m.invoke_capture_record(),
                    4 => show_centered(&m, 730.0, 350.0),
                    _ => ocr_region(),
                }
            } else if let (Some(code), Some(b)) = (rec_code, bw_hk.upgrade()) {
                match code {
                    0 => b.invoke_stop(),
                    1 => b.invoke_pause_resume(),
                    2 => b.invoke_restart(),
                    3 => b.invoke_mute_audio(),
                    _ => b.invoke_mute_mic(),
                }
            }
        }
        // Tray click → left = primary action (capture region), right = menu,
        // following the Greenshot/Lightshot convention. During a
        // recording the overlay can't open, so left falls through to the menu.
        while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
            let TrayIconEvent::Click { position, button_state, button, .. } = ev else {
                continue;
            };
            if button_state != tray_icon::MouseButtonState::Up {
                continue;
            }
            if button == tray_icon::MouseButton::Left && recorder_tray.borrow().is_none() {
                if let Some(m) = mw.upgrade() {
                    match load_settings().tray_left_click.as_str() {
                        "fullscreen" => m.invoke_capture_fullscreen(),
                        "region" => m.invoke_capture_region(),
                        "timer" => m.invoke_capture_timer(),
                        "record" => m.invoke_capture_record(),
                        // Default (incl. unset): open the all-in-one menu.
                        _ => show_centered(&m, 730.0, 350.0),
                    }
                }
                continue;
            }
            let Some(t) = tw.upgrade() else { continue };
            let s = load_settings();
            let combo = |i: usize| {
                combo_display(
                    s.shortcuts
                        .get(HK_IDS[i])
                        .map_or(HK_DEFAULTS[i], String::as_str),
                )
            };
            t.set_sc_region(combo(0));
            t.set_sc_fullscreen(combo(1));
            t.set_sc_window(combo(2));
            t.set_sc_record(combo(3));
            t.set_sc_menu(combo(4));
            t.set_sc_ocr(combo(5));
            // Swap to the recording controls while a capture is running; mirror
            // the bar's live audio/mic state so the mute rows read correctly.
            let recording = recorder_tray.borrow().is_some();
            t.set_recording(recording);
            if recording {
                if let Some(b) = bw_hk.upgrade() {
                    t.set_rec_paused(b.get_paused());
                    t.set_audio_on(b.get_audio_on());
                    t.set_mic_on(b.get_mic_on());
                    t.set_audio_muted(b.get_audio_muted());
                    t.set_mic_muted(b.get_mic_muted());
                }
            }
            let _ = t.show();
            // Kill the DWM open/close fade so the menu appears and vanishes
            // instantly (otherwise it lingers on hide, slower than it opened).
            disable_dwm_transitions(t.window());
            // Small popup anchored at the cursor (not a fullscreen overlay).
            place_at_cursor_topmost(t.window(), position.x as i32, position.y as i32);
            tray_settle = 3; // let it settle before the focus-loss poll kicks in
        }
        // Dismiss the tray menu when it loses focus (click elsewhere) — replaces
        // the old fullscreen click-catcher backdrop.
        if tray_settle > 0 {
            tray_settle -= 1;
        } else if let Some(t) = tw.upgrade() {
            if let Some(hwnd) = hwnd_of(t.window()) {
                // SAFETY: `hwnd` is valid; both calls only read window state.
                let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
                if visible && unsafe { GetForegroundWindow() } != hwnd {
                    let _ = t.hide();
                }
            }
        }
    });

    // Theme: restore the saved mode (default System → follow the OS).
    let os_dark = os_prefers_dark();
    let mode = load_settings().theme_mode;
    let dark = match mode {
        1 => false,
        2 => true,
        _ => os_dark,
    };
    // Slint globals are per-instance: every window owns its own Theme copy, so
    // the chosen mode must be pushed to each one independently.
    let apply_theme: Rc<dyn Fn(bool)> = Rc::new({
        let menu = menu_w.as_weak();
        let actions = actions_w.as_weak();
        let settings = settings_w.as_weak();
        let history = history_w.as_weak();
        let viewer = viewer_w.as_weak();
        let rec = rec_bar.as_weak();
        let countdown = countdown.as_weak();
        let traym = tray_w.as_weak();
        let picker = picker.as_weak();
        move |dark: bool| {
            if let Some(w) = menu.upgrade() { w.global::<Theme>().set_dark(dark); }
            if let Some(w) = actions.upgrade() { w.global::<Theme>().set_dark(dark); }
            if let Some(w) = settings.upgrade() { w.global::<Theme>().set_dark(dark); }
            if let Some(w) = history.upgrade() { w.global::<Theme>().set_dark(dark); }
            if let Some(w) = viewer.upgrade() { w.global::<Theme>().set_dark(dark); }
            if let Some(w) = rec.upgrade() { w.global::<Theme>().set_dark(dark); }
            // OCR is lazy: remember its theme for when it's built; the editor
            // exists from startup, so update it (and OCR if built) live.
            EDITOR.with(|cell| {
                if let Some(w) = cell.borrow().as_ref() {
                    w.global::<Theme>().set_dark(dark);
                    w.set_checker(to_slint_image(&make_checker(dark)));
                }
            });
            OCR_DARK.with(|c| c.set(dark));
            OCR.with(|cell| {
                if let Some(w) = cell.borrow().as_ref() {
                    w.global::<Theme>().set_dark(dark);
                }
            });
            if let Some(w) = countdown.upgrade() { w.global::<Theme>().set_dark(dark); }
            if let Some(w) = traym.upgrade() { w.global::<Theme>().set_dark(dark); }
            if let Some(w) = picker.upgrade() { w.global::<Theme>().set_dark(dark); }
        }
    });
    apply_theme(dark);
    settings_w.set_system_dark(os_dark);
    settings_w.set_theme_mode(mode);
    wire_settings(&settings_w, apply_theme, hk_state);

    // Keep the OS registry in sync with the saved toggles (refreshes the exe
    // path in the Run key / re-registers the association if the binary moved).
    {
        let s = load_settings();
        set_autostart(s.autostart);
        set_image_association(s.open_with_images);
    }

    // Launched with an image path (Explorer "Open with" / CLI)? Open it in
    // the viewer. Otherwise pop the capture menu.
    let arg_media = std::env::args()
        .skip(1)
        .map(PathBuf::from)
        .find(|p| is_media(p) && p.is_file());
    if let Some(path) = arg_media {
        open_in_viewer(&viewer_w.as_weak(), &viewer_state, path);
    } else {
        // Open the capture menu shortly after the loop starts, not here pre-loop:
        // the very first window shown comes up before winit resolves the monitor
        // scale, so showing it immediately lands it un-centred in the top-left.
        // A brief delay lets the loop warm up so it centres correctly.
        let m = menu_w.as_weak();
        slint::Timer::single_shot(Duration::from_millis(150), move || {
            if let Some(m) = m.upgrade() {
                show_centered(&m, 730.0, 350.0);
            }
        });
    }

    // Route later launches (forwarded over the single-instance pipe) to the
    // live windows: an image path opens the viewer, a bare relaunch the menu.
    {
        let vw = viewer_w.as_weak();
        let vs = viewer_state;
        let mw = menu_w.as_weak();
        REOPEN.with(|r| {
            *r.borrow_mut() = Some(Box::new(move |media: Option<PathBuf>| match media {
                Some(p) => open_in_viewer(&vw, &vs, p),
                None => {
                    if let Some(m) = mw.upgrade() {
                        show_centered(&m, 730.0, 350.0);
                    }
                }
            }));
        });
    }
    ipc_server.run(|msg| {
        let _ = slint::invoke_from_event_loop(move || {
            let media = (!msg.is_empty()).then(|| PathBuf::from(&msg));
            REOPEN.with(|r| {
                if let Some(f) = r.borrow().as_ref() {
                    f(media);
                }
            });
        });
    });

    slint::run_event_loop_until_quit()?;
    let _ = timer;
    Ok(())
}

/// Wire the annotation editor window — tools, colour, geometry, save/copy.
/// All 27 callbacks act on `editor_state`; nothing else is captured.
fn wire_editor(editor_w: &EditorWindow, editor_state: &Rc<RefCell<EditorState>>) {
    editor_w.on_dismiss({
        let w = editor_w.as_weak();
        move || { if let Some(w) = w.upgrade() { let _ = w.hide(); } }
    });
    editor_w.on_add_shape({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |kind, x1, y1, x2, y2, cw, ch, color, stroke, zoom, filled| {
            {
                let mut st = state.borrow_mut();
                // Map both ends to screenshot pixels (handles fit/zoom + bg padding).
                let Some((px1, py1, ds)) = map_shot(&st, x1, y1, cw, ch, zoom) else { return };
                let Some((px2, py2, _)) = map_shot(&st, x2, y2, cw, ch, zoom) else { return };
                if (px1 - px2).abs() < 3.0 && (py1 - py2).abs() < 3.0 { return; }
                // Slider stroke is in screen px; divide by the display scale so it
                // stays resolution-independent on the full-res image.
                st.shapes.push(Shape {
                    kind,
                    x1: px1, y1: py1, x2: px2, y2: py2,
                    r: color.red(), g: color.green(), b: color.blue(),
                    width: (stroke / ds).max(1.0),
                    filled,
                    text: String::new(),
                    points: Vec::new(),
                    font: 0,
                });
                st.redo.clear();
                st.sel = None;
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_add_text({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |x, y, cw, ch, text, color, font_px, font, zoom| {
            if text.trim().is_empty() {
                return;
            }
            {
                let mut st = state.borrow_mut();
                let Some((px, py, ds)) = map_shot(&st, x, y, cw, ch, zoom) else { return };
                st.shapes.push(Shape {
                    kind: 6,
                    x1: px, y1: py, x2: px, y2: py,
                    r: color.red(), g: color.green(), b: color.blue(),
                    width: (font_px / ds).max(6.0),
                    filled: false,
                    text: text.to_string(),
                    points: Vec::new(),
                    font,
                });
                st.redo.clear();
                st.sel = None;
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_add_badge({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |x, y, cw, ch, color, stroke, zoom| {
            {
                let mut st = state.borrow_mut();
                let Some((px, py, ds)) = map_shot(&st, x, y, cw, ch, zoom) else { return };
                // Auto-numbered in creation order; radius tracks the stroke slider.
                let n = st.shapes.iter().filter(|s| s.kind == 9).count() + 1;
                let radius = (stroke * 3.0 / ds).max(12.0);
                st.shapes.push(Shape {
                    kind: 9,
                    x1: px, y1: py, x2: px, y2: py,
                    r: color.red(), g: color.green(), b: color.blue(),
                    width: radius,
                    filled: false,
                    text: n.to_string(),
                    points: Vec::new(),
                    font: 0,
                });
                st.redo.clear();
                st.sel = None;
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_pen_start({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |x, y, cw, ch, color, stroke, zoom| {
            {
                let mut st = state.borrow_mut();
                let Some((px, py, ds)) = map_shot(&st, x, y, cw, ch, zoom) else { return };
                st.pen_draft.clear();
                st.pen_draft.push(px);
                st.pen_draft.push(py);
                st.pen_rgb = (color.red(), color.green(), color.blue());
                st.pen_w = (stroke / ds).max(1.0);
                st.sel = None;
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_pen_extend({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |x, y, cw, ch, zoom| {
            {
                let mut st = state.borrow_mut();
                let Some((px, py, _)) = map_shot(&st, x, y, cw, ch, zoom) else { return };
                let n = st.pen_draft.len();
                // Drop near-duplicate samples so the polyline stays light.
                if n >= 2 && (px - st.pen_draft[n - 2]).hypot(py - st.pen_draft[n - 1]) < 1.5 {
                    return;
                }
                st.pen_draft.push(px);
                st.pen_draft.push(py);
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_pen_commit({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move || {
            {
                let mut st = state.borrow_mut();
                if st.pen_draft.len() >= 4 {
                    let pts = std::mem::take(&mut st.pen_draft);
                    let (r, g, b) = st.pen_rgb;
                    let w = st.pen_w;
                    let (x1, y1) = (pts[0], pts[1]);
                    st.shapes.push(Shape {
                        kind: 10,
                        x1, y1, x2: x1, y2: y1,
                        r, g, b,
                        width: w,
                        filled: false,
                        text: String::new(),
                        points: pts,
                        font: 0,
                    });
                    st.redo.clear();
                } else {
                    st.pen_draft.clear();
                }
                st.sel = None;
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_pick_move({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |x, y, cw, ch, zoom| {
            {
                let mut st = state.borrow_mut();
                let Some((ix, iy, ds)) = map_shot(&st, x, y, cw, ch, zoom) else { return };
                let thresh = 12.0 / ds.max(0.001);
                // Pressing a handle of the current selection starts a resize.
                let on_handle = st
                    .sel
                    .and_then(|i| st.shapes.get(i))
                    .cloned()
                    .and_then(|s| hit_handle(&s, ix, iy, thresh).map(|h| (s, h)));
                if let Some((s, h)) = on_handle {
                    st.drag_handle = h as i32;
                    st.drag_orig = Some(s);
                    st.drag_from = (ix, iy);
                } else {
                    let sel = hit_test(&st.shapes, ix, iy);
                    st.sel = sel;
                    st.drag_handle = -1;
                    st.drag_from = (ix, iy);
                    st.drag_orig = sel.map(|i| st.shapes[i].clone());
                }
            }
            if let Some(e) = editor.upgrade() {
                update_sel_props(&e, &state.borrow());
            }
        }
    });
    editor_w.on_drag_move({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |x, y, cw, ch, zoom| {
            {
                let mut st = state.borrow_mut();
                let (Some(i), Some(orig)) = (st.sel, st.drag_orig.clone()) else { return };
                let Some((ix, iy, _)) = map_shot(&st, x, y, cw, ch, zoom) else { return };
                if st.drag_handle < 0 {
                    let dx = ix - st.drag_from.0;
                    let dy = iy - st.drag_from.1;
                    // Freehand: translate every point with the stroke.
                    let points = if orig.kind == 10 {
                        orig.points.iter().enumerate().map(|(k, v)| if k % 2 == 0 { v + dx } else { v + dy }).collect()
                    } else {
                        orig.points.clone()
                    };
                    st.shapes[i] = Shape {
                        x1: orig.x1 + dx,
                        y1: orig.y1 + dy,
                        x2: orig.x2 + dx,
                        y2: orig.y2 + dy,
                        points,
                        ..orig
                    };
                } else {
                    st.shapes[i] = resize_shape(orig, st.drag_handle, ix, iy);
                }
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_delete_selected({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move || {
            {
                let mut st = state.borrow_mut();
                let Some(i) = st.sel.filter(|&i| i < st.shapes.len()) else { return };
                st.shapes.remove(i);
                st.sel = None;
                st.drag_orig = None;
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_undo({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move || {
            {
                let mut st = state.borrow_mut();
                if let Some(s) = st.shapes.pop() {
                    st.redo.push(s);
                }
                st.sel = None;
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_redo({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move || {
            {
                let mut st = state.borrow_mut();
                if let Some(s) = st.redo.pop() {
                    st.shapes.push(s);
                }
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_bg_off({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move || {
            {
                let mut st = state.borrow_mut();
                if st.base.is_none() {
                    return;
                }
                st.bg_preset = BG_NONE;
            }
            if let Some(e) = editor.upgrade() {
                e.set_bg_on(false);
                e.set_bg_preset(BG_NONE);
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_pick_bg_preset({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |id| {
            {
                let mut st = state.borrow_mut();
                if st.base.is_none() {
                    return;
                }
                st.bg_preset = id;
            }
            if let Some(e) = editor.upgrade() {
                e.set_bg_on(true);
                e.set_bg_preset(id);
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_set_bg_padding({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |px| {
            state.borrow_mut().bg_padding = px.max(0) as u32;
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_set_bg_radius({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |px| {
            state.borrow_mut().bg_radius = px.max(0) as u32;
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_set_bg_shadow({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |on| {
            state.borrow_mut().bg_shadow = on;
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_set_bg_strength({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |v| {
            state.borrow_mut().bg_shadow_strength = v.clamp(0.0, 100.0) as u32;
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_set_bg_aspect({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |id| {
            state.borrow_mut().bg_aspect = id;
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_reset_bg({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move || {
            {
                let mut st = state.borrow_mut();
                st.bg_preset = BG_NONE;
                st.bg_custom = (99, 102, 241);
                st.bg_padding = 32;
                st.bg_radius = 16;
                st.bg_shadow = true;
                st.bg_shadow_strength = 60;
                st.bg_aspect = 0;
            }
            if let Some(e) = editor.upgrade() {
                e.set_bg_on(false);
                e.set_bg_preset(BG_NONE);
                e.set_bg_color(slint::Color::from_rgb_u8(99, 102, 241));
                e.set_bg_pad_idx(2);
                e.set_bg_rad_idx(2);
                e.set_bg_shadow(true);
                e.set_bg_shadow_strength(60.0);
                e.set_bg_aspect(0);
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_crop_apply({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |rxw, ryw, rww, rhw, sw, sh, zoom| {
            {
                let mut st = state.borrow_mut();
                let Some((x1, y1, _)) = map_shot(&st, rxw, ryw, sw, sh, zoom) else { return };
                let Some((x2, y2, _)) = map_shot(&st, rxw + rww, ryw + rhw, sw, sh, zoom) else { return };
                let rw = (x1 - x2).abs().round() as u32;
                let rh = (y1 - y2).abs().round() as u32;
                if rw < 8 || rh < 8 { return; }
                crop_base(&mut st, x1.min(x2).floor() as u32, y1.min(y2).floor() as u32, rw, rh);
            }
            if let Some(e) = editor.upgrade() {
                e.set_zoom(1.0);
                e.set_crop_armed(false);
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_resize_to({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |w, h| {
            if w < 1.0 || h < 1.0 {
                return;
            }
            {
                let mut st = state.borrow_mut();
                resize_to(&mut st, w.round() as u32, h.round() as u32);
            }
            if let Some(e) = editor.upgrade() {
                e.set_zoom(1.0);
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_bg_solid({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |c, sync_hsv| {
            let Some(e) = editor.upgrade() else { return };
            {
                let mut st = state.borrow_mut();
                if st.base.is_none() {
                    return;
                }
                apply_solid(&mut st, &e, c.red(), c.green(), c.blue(), sync_hsv);
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_bg_hex_commit({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move |hex| {
            let Some((r, g, b)) = parse_hex(&hex) else { return };
            let Some(e) = editor.upgrade() else { return };
            {
                let mut st = state.borrow_mut();
                if st.base.is_none() {
                    return;
                }
                apply_solid(&mut st, &e, r, g, b, true);
            }
            editor_rerender(&editor, &state);
        }
    });
    editor_w.on_bg_open_picker({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move || {
            let (r, g, b) = state.borrow().bg_custom;
            let (h, s, v) = rgb_to_hsv(r, g, b);
            let hex = format!("#{r:02X}{g:02X}{b:02X}");
            if let Some(e) = editor.upgrade() {
                e.set_bg_hue(h);
                e.set_bg_sat(s);
                e.set_bg_val(v);
                e.set_bg_color(slint::Color::from_rgb_u8(r, g, b));
                e.set_bg_hex(hex.into());
            }
        }
    });
    editor_w.on_save({
        let editor = editor_w.as_weak();
        let state = editor_state.clone();
        move || {
            {
                let st = state.borrow();
                if let (Some(img), Some(path)) = (compose(&st), st.path.as_ref()) {
                    if let Err(e) = save_png(&img, path) {
                        tracing::error!(path = %path.display(), error = %e, "save edited capture");
                    }
                    let _ = clipo_capture::save_thumbnail_jpeg(&img, &thumb_path(path)); // regenerated on demand
                }
            }
            if let Some(e) = editor.upgrade() {
                let _ = e.hide();
            }
        }
    });
    editor_w.on_copy({
        let state = editor_state.clone();
        move || {
            let st = state.borrow();
            if let Some(img) = compose(&st) {
                if let Err(e) = copy_image_to_clipboard(&img) {
                    tracing::warn!(error = %e, "copy edited capture to clipboard");
                }
            }
        }
    });
}

/// Wire the lightbox viewer — browse siblings + copy/ocr/edit/reveal/upload
/// the current one. Captures only the viewer/editor/ocr handles + their state.
fn wire_viewer(
    viewer_w: &ViewerWindow,
    viewer_state: &ViewerState,
    editor_state: &Rc<RefCell<EditorState>>,
) {
    viewer_w.on_dismiss({
        let w = viewer_w.as_weak();
        move || {
            stop_gif_playback();
            if let Some(w) = w.upgrade() {
                let _ = w.hide();
            }
        }
    });
    viewer_w.on_prev({
        let v = viewer_w.as_weak();
        let state = viewer_state.clone();
        move || {
            {
                let mut s = state.borrow_mut();
                if s.1 > 0 {
                    s.1 -= 1;
                }
            }
            if let Some(v) = v.upgrade() {
                refresh_viewer(&v, &state);
            }
        }
    });
    viewer_w.on_next({
        let v = viewer_w.as_weak();
        let state = viewer_state.clone();
        move || {
            {
                let mut s = state.borrow_mut();
                if s.1 + 1 < s.0.len() {
                    s.1 += 1;
                }
            }
            if let Some(v) = v.upgrade() {
                refresh_viewer(&v, &state);
            }
        }
    });
    viewer_w.on_copy({
        let state = viewer_state.clone();
        let viewer = viewer_w.as_weak();
        move || {
            if let Some(p) = current_viewer_path(&state) {
                if let Ok(img) = clipo_capture::decode_to_bgra(&p) {
                    if let Err(e) = copy_image_to_clipboard(&img) {
                        tracing::warn!(error = %e, "copy to clipboard");
                    } else if let Some(v) = viewer.upgrade() {
                        flash_viewer_copied(&v); // button flashes a check
                    }
                }
            }
        }
    });
    viewer_w.on_ocr({
        let state = viewer_state.clone();
        move || {
            if let Some(p) = current_viewer_path(&state) {
                open_ocr_lazy(p);
            }
        }
    });
    viewer_w.on_reveal({
        let state = viewer_state.clone();
        move || {
            if let Some(p) = current_viewer_path(&state) {
                reveal_in_explorer(&p);
            }
        }
    });
    viewer_w.on_delete({
        let v = viewer_w.as_weak();
        let state = viewer_state.clone();
        move || {
            let Some(path) = current_viewer_path(&state) else { return };
            if let Err(e) = trash::delete(&path) {
                tracing::warn!(error = %e, "delete capture from viewer");
                return;
            }
            forget_capture(&path);
            // Drop it from the browse list and show the next sibling (or the
            // previous one if it was the last); close the viewer if it was the
            // only image. The next sibling shifts into the same index on remove.
            let empty = {
                let mut s = state.borrow_mut();
                let i = s.1;
                if i < s.0.len() {
                    s.0.remove(i);
                }
                if s.1 >= s.0.len() && s.1 > 0 {
                    s.1 -= 1;
                }
                s.0.is_empty()
            };
            if let Some(v) = v.upgrade() {
                if empty {
                    stop_gif_playback();
                    let _ = v.hide();
                } else {
                    refresh_viewer(&v, &state);
                }
            }
        }
    });
    viewer_w.on_open_external({
        let state = viewer_state.clone();
        move || {
            if let Some(p) = current_viewer_path(&state) {
                open_in_default_app(&p);
            }
        }
    });
    viewer_w.on_edit({
        let state = viewer_state.clone();
        let est = editor_state.clone();
        let viewer = viewer_w.as_weak();
        move || {
            if let Some(p) = current_viewer_path(&state) {
                if let Some(v) = viewer.upgrade() {
                    let _ = v.hide();
                }
                open_editor_lazy(&est, p);
            }
        }
    });
    viewer_w.on_upload({
        let state = viewer_state.clone();
        let viewer = viewer_w.as_weak();
        move || {
            let Some(path) = current_viewer_path(&state) else { return };
            let s = load_settings();
            if !s.upload_ready() {
                return;
            }
            if let Some(v) = viewer.upgrade() {
                v.set_upload_status(0);
                v.set_upload_phase(1); // button spins
            }
            let viewer = viewer.clone();
            std::thread::spawn(move || {
                let res = upload_capture_blocking(&s, &path);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(v) = viewer.upgrade() else { return };
                    match res {
                        Ok(_) => {
                            v.set_uploaded(true);
                            flash_viewer_done(&v); // button flashes a check
                        }
                        Err(e) => {
                            v.set_upload_phase(0);
                            v.set_upload_detail(e.into());
                            v.set_upload_status(2); // upload failed
                        }
                    }
                });
            });
        }
    });
    viewer_w.on_copy_link({
        let state = viewer_state.clone();
        let viewer = viewer_w.as_weak();
        move || {
            let Some(path) = current_viewer_path(&state) else { return };
            if !settings::copy_uploaded_link(&path) {
                return;
            }
            if let Some(v) = viewer.upgrade() {
                v.set_upload_status(0);
                flash_viewer_done(&v);
            }
        }
    });
}

/// Wire the post-capture actions panel — copy/ocr/open/reveal/upload/gif of
/// the last saved capture (its path lives in `current`).
fn wire_actions(
    actions_w: &ActionsWindow,
    current: &Rc<RefCell<Option<PathBuf>>>,
    viewer_w: &ViewerWindow,
    viewer_state: &ViewerState,
    editor_state: &Rc<RefCell<EditorState>>,
    actions_dismiss: &Rc<slint::Timer>,
) {
    actions_w.on_edit({
        let state = editor_state.clone();
        let actions = actions_w.as_weak();
        let current = current.clone();
        move || {
            if let Some(p) = current.borrow().as_ref().cloned() {
                open_editor_lazy(&state, p);
            }
            if let Some(a) = actions.upgrade() { let _ = a.hide(); }
        }
    });
    actions_w.on_copy({
        let actions = actions_w.as_weak();
        let current = current.clone();
        move || {
            if let Some(p) = current.borrow().as_ref() {
                match clipo_capture::decode_to_bgra(p) {
                    Ok(img) => {
                        if let Err(e) = copy_image_to_clipboard(&img) {
                            tracing::warn!(error = %e, "copy to clipboard");
                        } else if let Some(a) = actions.upgrade() {
                            a.set_copied(true); // button flashes a check
                            revert_after(&a, 1200, |a| a.set_copied(false));
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "copy decode"),
                }
            }
            if let Some(a) = actions.upgrade() { let _ = a.hide(); }
        }
    });
    actions_w.on_ocr({
        let actions = actions_w.as_weak();
        let current = current.clone();
        move || {
            let Some(path) = current.borrow().as_ref().cloned() else { return };
            open_ocr_lazy(path);
            if let Some(a) = actions.upgrade() { let _ = a.hide(); }
        }
    });
    actions_w.on_open_viewer({
        let viewer = viewer_w.as_weak();
        let state = viewer_state.clone();
        let current = current.clone();
        let actions = actions_w.as_weak();
        move || {
            let Some(path) = current.borrow().as_ref().cloned() else { return };
            open_in_viewer(&viewer, &state, path);
            if let Some(a) = actions.upgrade() { let _ = a.hide(); }
        }
    });
    actions_w.on_reveal({
        let current = current.clone();
        let actions = actions_w.as_weak();
        move || {
            if let Some(p) = current.borrow().as_ref() {
                reveal_in_explorer(p);
            }
            if let Some(a) = actions.upgrade() { let _ = a.hide(); }
        }
    });
    actions_w.on_open_external({
        let current = current.clone();
        let actions = actions_w.as_weak();
        move || {
            if let Some(p) = current.borrow().as_ref() {
                open_in_default_app(p);
            }
            if let Some(a) = actions.upgrade() { let _ = a.hide(); }
        }
    });
    actions_w.on_upload({
        let current = current.clone();
        let actions = actions_w.as_weak();
        move || {
            let Some(path) = current.borrow().as_ref().cloned() else { return };
            let s = load_settings();
            if !s.upload_ready() {
                return;
            }
            if let Some(a) = actions.upgrade() {
                a.set_upload_busy(true);
                a.set_upload_status(1); // uploading
            }
            // Upload off the UI thread; the banner shows a spinner. Success closes
            // the toast; an error turns the banner red and keeps it up.
            let actions = actions.clone();
            std::thread::spawn(move || {
                let res = upload_capture_blocking(&s, &path);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(a) = actions.upgrade() else { return };
                    a.set_upload_busy(false);
                    match res {
                        Ok(_) => {
                            a.set_upload_status(0);
                            let _ = a.hide();
                        }
                        Err(e) => {
                            a.set_upload_detail(e.into());
                            a.set_upload_status(2); // upload failed
                        }
                    }
                });
            });
        }
    });
    actions_w.on_export_gif({
        let current = current.clone();
        let actions = actions_w.as_weak();
        move || {
            let Some(path) = current.borrow().as_ref().cloned() else { return };
            if let Some(a) = actions.upgrade() {
                a.set_upload_status(0);
                a.set_gif_busy(true); // spin the GIF button while ffmpeg runs
            }
            let actions = actions.clone();
            std::thread::spawn(move || {
                let res = export_gif(&path);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(a) = actions.upgrade() else { return };
                    a.set_gif_busy(false);
                    match res {
                        Ok(_) => a.set_upload_status(3), // gif saved
                        Err(e) => {
                            a.set_upload_detail(e.into());
                            a.set_upload_status(4); // gif failed
                        }
                    }
                });
            });
        }
    });
    // Pause auto-dismiss while the pointer is over the toast; re-arm on leave.
    actions_w.on_hover_changed({
        let actions = actions_w.as_weak();
        let dismiss = actions_dismiss.clone();
        move |hovering| {
            if hovering {
                dismiss.stop();
            } else {
                arm_auto_dismiss(&actions, &dismiss);
            }
        }
    });
}

/// Wire the history grid's per-item actions (filter/delete/gif/edit/ocr/
/// copy/reveal/open). Captures the history/editor/ocr handles + editor state.
/// Mark one history card as uploaded in place (its button morphs to "copy
/// link") without rebuilding the grid — a rebuild recreates the delegates and
/// drops the hover state, so the action row would vanish until the pointer
/// re-enters.
fn mark_history_uploaded(h: &HistoryWindow, path: &str) {
    let groups = h.get_groups();
    for gi in 0..groups.row_count() {
        let Some(group) = groups.row_data(gi) else { continue };
        for ii in 0..group.items.row_count() {
            if let Some(mut item) = group.items.row_data(ii) {
                if item.path.as_str() == path {
                    item.uploaded = true;
                    group.items.set_row_data(ii, item);
                    return;
                }
            }
        }
    }
}

/// Flash the viewer's upload button into its "done" (check) state, settling
/// back to idle after a beat. Clears any leftover error banner.
/// Run `reset` on a window after `ms` (if it's still alive). Shared by every
/// "flash a check, then revert" confirmation (Copy / Upload across the viewer,
/// post-capture panel and history).
fn revert_after<W: ComponentHandle + 'static>(c: &W, ms: u64, reset: impl Fn(&W) + 'static) {
    let cw = c.as_weak();
    slint::Timer::single_shot(Duration::from_millis(ms), move || {
        if let Some(c) = cw.upgrade() {
            reset(&c);
        }
    });
}

fn flash_viewer_done(v: &ViewerWindow) {
    v.set_upload_status(0);
    v.set_upload_phase(2);
    revert_after(v, 1200, |v| v.set_upload_phase(0));
}

/// Flash a check on the viewer's Copy button after copying the image, then
/// revert — same confirmation idiom as the upload/copy-link check.
fn flash_viewer_copied(v: &ViewerWindow) {
    v.set_copied(true);
    revert_after(v, 1200, |v| v.set_copied(false));
}

/// Flash one history card's upload button into its "done" (check) state, then
/// clear it — unless a newer upload has since claimed the slot.
fn flash_history_done(h: &HistoryWindow, path: &str) {
    h.set_upload_status(0);
    h.set_done_path(path.into());
    let hw = h.as_weak();
    let p = path.to_string();
    slint::Timer::single_shot(Duration::from_millis(1200), move || {
        if let Some(h) = hw.upgrade() {
            if h.get_done_path().as_str() == p {
                h.set_done_path(String::new().into());
            }
        }
    });
}

fn wire_history(
    history_w: &HistoryWindow,
    viewer_w: &ViewerWindow,
    viewer_state: &ViewerState,
    editor_state: &Rc<RefCell<EditorState>>,
) {
    history_w.on_open({
        let viewer = viewer_w.as_weak();
        let state = viewer_state.clone();
        move |path| {
            open_in_viewer(&viewer, &state, PathBuf::from(path.as_str()));
        }
    });
    history_w.on_filter_changed({
        let w = history_w.as_weak();
        move || { if let Some(h) = w.upgrade() { rebuild_history(&h); } }
    });
    history_w.on_delete_capture({
        let w = history_w.as_weak();
        move |path| {
            let p = PathBuf::from(path.as_str());
            if let Err(e) = trash::delete(&p) {
                tracing::warn!(error = %e, "delete capture");
                return;
            }
            let _ = std::fs::remove_file(thumb_path(&p)); // sidecar cache, not worth recycling
            forget_capture(&p);
            if let Some(h) = w.upgrade() { rebuild_history(&h); }
        }
    });
    history_w.set_gif_ready(gif_available());
    history_w.on_export_gif({
        let w = history_w.as_weak();
        move |path| {
            let src = PathBuf::from(path.as_str());
            if let Some(h) = w.upgrade() {
                h.set_busy_path(path.as_str().into()); // that card's GIF button spins
            }
            let w = w.clone();
            // ffmpeg is slow; run off-thread, then re-scan so the new .gif appears.
            std::thread::spawn(move || {
                let res = export_gif(&src);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(h) = w.upgrade() {
                        h.set_busy_path(String::new().into());
                    }
                    match res {
                        Ok(_) => populate_history(&w),
                        Err(e) => tracing::warn!(error = %e, "gif export"),
                    }
                });
            });
        }
    });
    history_w.on_edit_capture({
        let state = editor_state.clone();
        move |path| open_editor_lazy(&state, PathBuf::from(path.as_str()))
    });
    history_w.on_ocr_capture(move |path| open_ocr_lazy(PathBuf::from(path.as_str())));
    history_w.on_copy_capture({
        let w = history_w.as_weak();
        move |path| {
            if let Ok(img) = clipo_capture::decode_to_bgra(&PathBuf::from(path.as_str())) {
                if let Err(e) = copy_image_to_clipboard(&img) {
                    tracing::warn!(error = %e, "copy to clipboard");
                } else if let Some(h) = w.upgrade() {
                    h.set_copied_path(path); // that card's Copy flashes a check
                    revert_after(&h, 1200, |h| h.set_copied_path(String::new().into()));
                }
            }
        }
    });
    history_w.on_reveal_capture(move |path| reveal_in_explorer(&PathBuf::from(path.as_str())));
    history_w.on_open_external_capture(move |path| open_in_default_app(&PathBuf::from(path.as_str())));
    history_w.on_upload_capture({
        let w = history_w.as_weak();
        move |path| {
            let s = load_settings();
            if !s.upload_ready() {
                return;
            }
            let path_str = path.to_string();
            let path = PathBuf::from(path.as_str());
            if let Some(h) = w.upgrade() {
                h.set_upload_status(0);
                h.set_done_path(String::new().into());
                h.set_busy_path(path_str.as_str().into()); // that card spins
            }
            let w = w.clone();
            std::thread::spawn(move || {
                let res = upload_capture_blocking(&s, &path);
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(h) = w.upgrade() else { return };
                    h.set_busy_path(String::new().into());
                    match res {
                        Ok(_) => {
                            // In-place morph to "copy link" — no rebuild, so the
                            // hovered card keeps its action row.
                            mark_history_uploaded(&h, &path_str);
                            flash_history_done(&h, &path_str); // card flashes a check
                        }
                        Err(e) => {
                            h.set_upload_detail(e.into());
                            h.set_upload_status(2); // upload failed
                        }
                    }
                });
            });
        }
    });
    history_w.on_copy_link_capture({
        let w = history_w.as_weak();
        move |path| {
            if !settings::copy_uploaded_link(&PathBuf::from(path.as_str())) {
                return;
            }
            if let Some(h) = w.upgrade() {
                flash_history_done(&h, path.as_str());
            }
        }
    });
}

/// Wire the Settings window — theme, all toggles/selectors, the cloud-upload
/// modal, shortcuts (rebind/reset) and reset-all. `apply_theme` + `hk_state`
/// are moved in (reset-all consumes both; every other user cloned earlier).
fn wire_settings(
    settings_w: &SettingsWindow,
    apply_theme: Rc<dyn Fn(bool)>,
    hk_state: Rc<RefCell<HkState>>,
) {
    settings_w.on_theme_changed({
        let apply_theme = apply_theme.clone();
        move |mode| {
            let mut s = load_settings();
            s.theme_mode = mode;
            save_settings(&s);
            apply_theme(match mode { 1 => false, 2 => true, _ => os_prefers_dark() });
        }
    });

    // Storage tab: capture folder + image format + the cloud-upload modal.
    {
        let s0 = load_settings();
        // General + recording toggles (read before the String fields move out).
        settings_w.set_autostart(s0.autostart);
        settings_w.set_tray_left_click(match s0.tray_left_click.as_str() {
            "fullscreen" => 1,
            "region" => 0,
            "timer" => 3,
            "record" => 4,
            // Default (incl. unset): the all-in-one menu.
            _ => 2,
        });
        settings_w.set_language_idx(LANGS.iter().position(|&c| c == s0.language).unwrap_or(0) as i32);
        settings_w.set_highlight_cursor(s0.highlight_cursor);
        settings_w.set_rec_cursor(s0.record_cursor_enabled());
        settings_w.set_magnifier(s0.magnifier_enabled());
        settings_w.set_open_with_images(s0.open_with_images);
        settings_w.set_copy_clipboard(s0.copy_enabled());
        settings_w.set_rec_audio(s0.audio_enabled());
        settings_w.set_rec_mic(s0.capture_mic);
        settings_w.set_rec_fps_idx(i32::from(s0.fps() == 60));
        settings_w.set_timer_idx(match s0.timer_secs() {
            5 => 1,
            10 => 2,
            _ => 0,
        });
        settings_w.set_dismiss_idx(match s0.dismiss_secs() {
            3 => 0,
            10 => 2,
            _ => 1,
        });
        settings_w.set_rec_countdown(s0.recording_countdown);
        settings_w.set_rec_software(s0.software_encoder);
        settings_w.set_capture_folder(capture_dir().to_string_lossy().to_string().into());
        settings_w.set_image_format(s0.image_format);
        settings_w.set_upload_provider(s0.upload_provider);
        settings_w.set_upload_configured(s0.upload_ready());
        settings_w.set_s3_endpoint(s0.s3_endpoint.into());
        settings_w.set_s3_region(s0.s3_region.into());
        settings_w.set_r2_account_id(s0.r2_account_id.into());
        settings_w.set_r2_access_key(s0.r2_access_key_id.into());
        settings_w.set_r2_secret(s0.r2_secret_access_key.into());
        settings_w.set_r2_bucket(s0.r2_bucket.into());
        settings_w.set_r2_public_url(s0.r2_public_url.into());
    }
    settings_w.on_toggle_autostart(|on| {
        let mut s = load_settings();
        s.autostart = on;
        save_settings(&s);
        set_autostart(on);
    });
    settings_w.on_toggle_highlight_cursor(|on| {
        let mut s = load_settings();
        s.highlight_cursor = on;
        save_settings(&s);
    });
    settings_w.on_toggle_record_cursor(|on| {
        let mut s = load_settings();
        s.record_cursor = Some(on);
        save_settings(&s);
    });
    settings_w.on_toggle_open_with(|on| {
        let mut s = load_settings();
        s.open_with_images = on;
        save_settings(&s);
        set_image_association(on);
    });
    settings_w.on_toggle_copy_clipboard(|on| {
        let mut s = load_settings();
        s.copy_clipboard = Some(on);
        save_settings(&s);
    });
    settings_w.on_set_tray_left_click(|i| {
        let mut s = load_settings();
        s.tray_left_click = match i {
            1 => "fullscreen",
            2 => "menu",
            3 => "timer",
            4 => "record",
            _ => "region",
        }
        .to_string();
        save_settings(&s);
    });
    settings_w.on_set_language(|i| {
        let code = LANGS.get(i as usize).copied().unwrap_or("en");
        let mut s = load_settings();
        s.language = code.to_string();
        save_settings(&s);
        // Switch the bundled translation live; open @tr strings re-render.
        if let Err(e) = slint::select_bundled_translation(code) {
            tracing::warn!(lang = %code, error = %e, "select_bundled_translation");
        }
    });
    settings_w.on_toggle_magnifier(|on| {
        let mut s = load_settings();
        s.magnifier = Some(on);
        save_settings(&s);
    });
    settings_w.on_toggle_rec_audio(|on| {
        let mut s = load_settings();
        s.capture_audio = Some(on);
        save_settings(&s);
    });
    settings_w.on_toggle_rec_mic(|on| {
        let mut s = load_settings();
        s.capture_mic = on;
        save_settings(&s);
    });
    settings_w.on_toggle_rec_software(|on| {
        let mut s = load_settings();
        s.software_encoder = on;
        save_settings(&s);
    });
    settings_w.on_set_rec_fps(|i| {
        let mut s = load_settings();
        s.recording_fps = if i == 1 { 60 } else { 30 };
        save_settings(&s);
    });
    settings_w.on_set_timer(|i| {
        let mut s = load_settings();
        s.timer_seconds = match i {
            1 => 5,
            2 => 10,
            _ => 3,
        };
        save_settings(&s);
    });
    settings_w.on_set_dismiss(|i| {
        let mut s = load_settings();
        s.dismiss_seconds = match i {
            0 => 3,
            2 => 10,
            _ => 5,
        };
        save_settings(&s);
    });
    settings_w.on_toggle_rec_countdown(|on| {
        let mut s = load_settings();
        s.recording_countdown = on;
        save_settings(&s);
    });
    settings_w.on_pick_folder({
        let w = settings_w.as_weak();
        move || {
            if let Some(dir) = pick_folder() {
                let path = dir.to_string_lossy().to_string();
                let mut s = load_settings();
                s.capture_folder = Some(path.clone());
                save_settings(&s);
                if let Some(sw) = w.upgrade() {
                    sw.set_capture_folder(path.into());
                }
            }
        }
    });
    settings_w.on_reset_folder({
        let w = settings_w.as_weak();
        move || {
            let mut s = load_settings();
            s.capture_folder = None;
            save_settings(&s);
            if let Some(sw) = w.upgrade() {
                sw.set_capture_folder(capture_dir().to_string_lossy().to_string().into());
            }
        }
    });
    settings_w.on_image_format_changed(|fmt| {
        let mut s = load_settings();
        s.image_format = fmt;
        save_settings(&s);
    });
    settings_w.on_provider_changed(|p| {
        let mut s = load_settings();
        s.upload_provider = p;
        save_settings(&s);
    });
    settings_w.on_disconnect_upload({
        let w = settings_w.as_weak();
        move || {
            let Some(sw) = w.upgrade() else { return };
            let mut s = load_settings();
            s.r2_account_id.clear();
            s.r2_access_key_id.clear();
            s.r2_secret_access_key.clear();
            s.r2_bucket.clear();
            s.r2_public_url.clear();
            s.s3_endpoint.clear();
            s.s3_region.clear();
            save_settings(&s);
            sw.set_r2_account_id("".into());
            sw.set_r2_access_key("".into());
            sw.set_r2_secret("".into());
            sw.set_r2_bucket("".into());
            sw.set_r2_public_url("".into());
            sw.set_s3_endpoint("".into());
            sw.set_s3_region("".into());
            sw.set_upload_configured(false);
            sw.set_upload_status(5); // disconnected
        }
    });
    settings_w.on_apply_upload({
        let w = settings_w.as_weak();
        move || {
            let Some(sw) = w.upgrade() else { return };
            let mut s = load_settings();
            s.upload_provider = sw.get_upload_provider();
            // Trim stray whitespace (e.g. a leading space dragged in by a paste).
            // SigV4 signs the exact key string, so a space would break the upload.
            s.s3_endpoint = sw.get_s3_endpoint().trim().to_string();
            s.s3_region = sw.get_s3_region().trim().to_string();
            s.r2_account_id = sw.get_r2_account_id().trim().to_string();
            s.r2_access_key_id = sw.get_r2_access_key().trim().to_string();
            s.r2_secret_access_key = sw.get_r2_secret().trim().to_string();
            s.r2_bucket = sw.get_r2_bucket().trim().to_string();
            s.r2_public_url = sw.get_r2_public_url().trim().to_string();
            save_settings(&s);
            // Reflect the cleaned values back so the fields stop showing the
            // stray whitespace.
            sw.set_s3_endpoint(s.s3_endpoint.clone().into());
            sw.set_s3_region(s.s3_region.clone().into());
            sw.set_r2_account_id(s.r2_account_id.clone().into());
            sw.set_r2_access_key(s.r2_access_key_id.clone().into());
            sw.set_r2_secret(s.r2_secret_access_key.clone().into());
            sw.set_r2_bucket(s.r2_bucket.clone().into());
            sw.set_r2_public_url(s.r2_public_url.clone().into());
            sw.set_upload_configured(s.upload_ready());
            if !s.upload_ready() {
                sw.set_upload_status(6); // fields incomplete
                return;
            }
            sw.set_upload_status(7); // testing
            let wk = sw.as_weak();
            std::thread::spawn(move || {
                let res = test_upload_blocking(&s);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(sw) = wk.upgrade() {
                        match res {
                            // Success closes the modal; the Storage row already
                            // shows the connected state.
                            Ok(()) => sw.set_upload_editing(false),
                            Err(e) => {
                                sw.set_upload_detail(e.into());
                                sw.set_upload_status(8); // test failed
                            }
                        }
                    }
                });
            });
        }
    });

    // Shortcuts tab: show the saved/default combo per row, and rebind live.
    {
        let saved = load_settings();
        for i in 0..6 {
            let combo = saved
                .shortcuts
                .get(HK_IDS[i])
                .cloned()
                .unwrap_or_else(|| HK_DEFAULTS[i].to_string());
            let d = combo_display(&combo);
            match i {
                0 => settings_w.set_sc_region(d),
                1 => settings_w.set_sc_fullscreen(d),
                2 => settings_w.set_sc_window(d),
                3 => settings_w.set_sc_record(d),
                4 => settings_w.set_sc_menu(d),
                _ => settings_w.set_sc_ocr(d),
            }
        }
        settings_w.set_sc_conflicts(shortcut_conflicts(&hk_state.borrow()));
    }
    settings_w.on_rebind({
        let w = settings_w.as_weak();
        let st = hk_state.clone();
        move |index, ctrl, shift, alt, text| {
            let Some(sw) = w.upgrade() else { return };
            sw.set_recording(-1);
            let idx = index as usize;
            // Capture supports modifier + letter/digit; need at least one modifier.
            if idx >= 6 || !(ctrl || shift || alt) {
                return;
            }
            let key = text.trim().to_uppercase();
            if key.len() != 1 || !key.chars().all(|c| c.is_ascii_alphanumeric()) {
                return;
            }
            let mut combo = String::new();
            if ctrl {
                combo.push_str("Ctrl+");
            }
            if shift {
                combo.push_str("Shift+");
            }
            if alt {
                combo.push_str("Alt+");
            }
            combo.push_str(&key);
            // Bind even if it conflicts: save + show the combo and flag the row,
            // so a clash is visible rather than a silent no-op (matches the bar's
            // honesty). bind() leaves it unregistered, which conflicts() reports.
            st.borrow_mut().bind(idx, &combo);
            let mut s = load_settings();
            s.shortcuts.insert(HK_IDS[idx].to_string(), combo.clone());
            save_settings(&s);
            let d = combo_display(&combo);
            match idx {
                0 => sw.set_sc_region(d),
                1 => sw.set_sc_fullscreen(d),
                2 => sw.set_sc_window(d),
                3 => sw.set_sc_record(d),
                4 => sw.set_sc_menu(d),
                _ => sw.set_sc_ocr(d),
            }
            sw.set_sc_conflicts(shortcut_conflicts(&st.borrow()));
        }
    });
    settings_w.on_reset_shortcuts({
        let w = settings_w.as_weak();
        let st = hk_state.clone();
        move || {
            let Some(sw) = w.upgrade() else { return };
            sw.set_recording(-1);
            {
                let mut state = st.borrow_mut();
                for (i, &combo) in HK_DEFAULTS.iter().enumerate() {
                    state.bind(i, combo);
                }
            }
            let mut s = load_settings();
            s.shortcuts.clear();
            save_settings(&s);
            sw.set_sc_region(combo_display(HK_DEFAULTS[0]));
            sw.set_sc_fullscreen(combo_display(HK_DEFAULTS[1]));
            sw.set_sc_window(combo_display(HK_DEFAULTS[2]));
            sw.set_sc_record(combo_display(HK_DEFAULTS[3]));
            sw.set_sc_menu(combo_display(HK_DEFAULTS[4]));
            sw.set_sc_ocr(combo_display(HK_DEFAULTS[5]));
            sw.set_sc_conflicts(shortcut_conflicts(&st.borrow()));
        }
    });

    // About tab: build identity + diagnostics + factory reset.
    settings_w.set_app_version(env!("CARGO_PKG_VERSION").into());
    settings_w.set_app_commit(env!("CLIPO_COMMIT").into());
    settings_w.set_app_commit_date(env!("CLIPO_COMMIT_DATE").into());
    settings_w.on_copy_diagnostic(|| {
        copy_text(&format!(
            "Clipo {}\nCommit: {}\nBuilt: {}",
            env!("CARGO_PKG_VERSION"),
            env!("CLIPO_COMMIT"),
            env!("CLIPO_COMMIT_DATE"),
        ));
    });
    settings_w.on_check_updates({
        let w = settings_w.as_weak();
        move || {
            let Some(sw) = w.upgrade() else { return };
            sw.set_update_status(1); // checking
            let wk = sw.as_weak();
            std::thread::spawn(move || {
                let res = check_update_blocking();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(sw) = wk.upgrade() {
                        match res {
                            Ok(Some(info)) => {
                                sw.set_update_detail(info.version.clone().into());
                                sw.set_update_status(3); // available
                                PENDING_UPDATE.with(|p| *p.borrow_mut() = Some(info));
                            }
                            Ok(None) => sw.set_update_status(2), // up to date
                            Err(e) => {
                                sw.set_update_detail(e.into());
                                sw.set_update_status(4); // check failed
                            }
                        }
                    }
                });
            });
        }
    });
    settings_w.on_install_update({
        let w = settings_w.as_weak();
        move || {
            let Some(sw) = w.upgrade() else { return };
            let Some(info) = PENDING_UPDATE.with(|p| p.borrow().clone()) else { return };
            sw.set_update_status(5); // downloading
            let wk = sw.as_weak();
            std::thread::spawn(move || {
                let res = download_and_apply_update(&info);
                let _ = slint::invoke_from_event_loop(move || {
                    match res {
                        Ok(()) => {
                            // The running exe was swapped in place. Relaunch it
                            // (detached, with --updated so its single-instance
                            // guard waits for us to release the pipe) and exit so
                            // the old image is freed.
                            if let Ok(exe) = std::env::current_exe() {
                                use std::os::windows::process::CommandExt;
                                const DETACHED_PROCESS: u32 = 0x0000_0008;
                                const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
                                let _ = std::process::Command::new(exe)
                                    .arg("--updated")
                                    .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
                                    .spawn();
                            }
                            std::process::exit(0);
                        }
                        Err(e) => {
                            if let Some(sw) = wk.upgrade() {
                                sw.set_update_detail(e.into());
                                sw.set_update_status(6); // install failed
                            }
                        }
                    }
                });
            });
        }
    });
    settings_w.on_reset_all({
        let w = settings_w.as_weak();
        let st = hk_state;
        let apply_theme = apply_theme;
        move || {
            let Some(sw) = w.upgrade() else { return };
            save_settings(&Settings::default());
            sw.set_theme_mode(0);
            apply_theme(os_prefers_dark());
            sw.set_r2_account_id("".into());
            sw.set_r2_access_key("".into());
            sw.set_r2_secret("".into());
            sw.set_r2_bucket("".into());
            sw.set_r2_public_url("".into());
            sw.set_upload_status(0);
            set_autostart(false);
            set_image_association(false);
            sw.set_autostart(false);
            sw.set_highlight_cursor(false);
            sw.set_rec_cursor(true);
            sw.set_open_with_images(false);
            sw.set_copy_clipboard(true);
            sw.set_rec_audio(true);
            sw.set_rec_mic(false);
            sw.set_rec_fps_idx(0);
            sw.set_timer_idx(0);
            sw.set_dismiss_idx(1);
            sw.set_rec_countdown(false);
            {
                let mut state = st.borrow_mut();
                for (i, &combo) in HK_DEFAULTS.iter().enumerate() {
                    state.bind(i, combo);
                }
            }
            sw.set_sc_region(combo_display(HK_DEFAULTS[0]));
            sw.set_sc_fullscreen(combo_display(HK_DEFAULTS[1]));
            sw.set_sc_window(combo_display(HK_DEFAULTS[2]));
            sw.set_sc_record(combo_display(HK_DEFAULTS[3]));
            sw.set_sc_menu(combo_display(HK_DEFAULTS[4]));
            sw.set_sc_ocr(combo_display(HK_DEFAULTS[5]));
            sw.set_sc_conflicts(shortcut_conflicts(&st.borrow()));
            sw.set_recording(-1);
        }
    });
}

// Pure-logic unit tests. `#[cfg(test)]` is compiled only under `cargo test`, so
// none of this ships in the release binary — zero runtime cost to the app. The
// tests stay fast + deterministic: no Win32/Slint/DXGI, no I/O, no sleeps.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{editor::*, recording::recording_bitrate, settings::*};
    use std::path::Path;

    #[test]
    fn settings_defaults_are_opt_out_friendly() {
        // Missing JSON document → defaults; the opt-OUT toggles read as "on".
        let s: Settings = serde_json::from_str("{}").unwrap();
        assert!(s.copy_enabled());
        assert!(s.audio_enabled());
        assert!(s.magnifier_enabled());
        assert_eq!(s.fps(), 30);
        assert_eq!(Settings::default().fps(), 30);
    }

    #[test]
    fn settings_fps_is_30_unless_60() {
        let s = Settings { recording_fps: 60, ..Default::default() };
        assert_eq!(s.fps(), 60);
        let s = Settings { recording_fps: 0, ..Default::default() };
        assert_eq!(s.fps(), 30);
    }

    #[test]
    fn settings_serde_uses_camelcase_and_roundtrips() {
        let s = Settings {
            tray_left_click: "menu".into(),
            language: "pt".into(),
            recording_fps: 60,
            ..Default::default()
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"trayLeftClick\":\"menu\""), "{json}");
        assert!(json.contains("\"language\":\"pt\""));
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tray_left_click, "menu");
        assert_eq!(back.language, "pt");
        assert_eq!(back.fps(), 60);
    }

    #[test]
    fn recording_bitrate_clamps_and_scales() {
        assert_eq!(recording_bitrate(100, 100, 30), 2_000_000); // floor
        assert_eq!(recording_bitrate(7680, 4320, 60), 80_000_000); // ceiling
        assert!((2_000_000..=80_000_000).contains(&recording_bitrate(1920, 1080, 30)));
        assert!(recording_bitrate(1920, 1080, 60) > recording_bitrate(1920, 1080, 30));
    }

    #[test]
    fn combo_display_formats() {
        assert_eq!(combo_display("Ctrl+Shift+S").as_str(), "Ctrl + Shift + S");
        assert_eq!(combo_display("PrintScreen").as_str(), "PrtSc");
        assert_eq!(combo_display("Ctrl+PrintScreen").as_str(), "Ctrl + PrtSc");
    }

    #[test]
    fn parse_semver_and_compare() {
        assert_eq!(parse_semver("1.2.3"), (1, 2, 3));
        assert_eq!(parse_semver("v1.2.3"), (1, 2, 3));
        assert_eq!(parse_semver("1.2.3-beta.1+build"), (1, 2, 3));
        assert_eq!(parse_semver("2.0"), (2, 0, 0));
        assert!(parse_semver("1.0.1") > parse_semver("1.0.0"));
        assert!(parse_semver("1.2.0") > parse_semver("1.1.9"));
    }

    #[test]
    fn aspect_dims_keeps_or_fits_ratio() {
        assert_eq!(aspect_dims(800, 600, 0), (800, 600)); // 0 = original
        assert_eq!(aspect_dims(100, 50, 1), (100, 100)); // 1:1
        let (w, h) = aspect_dims(1000, 1000, 2); // 16:9 widens a square
        assert_eq!(h, 1000);
        assert!((w as f32 / h as f32 - 16.0 / 9.0).abs() < 0.01);
    }

    #[test]
    fn is_media_matches_known_extensions() {
        assert!(is_media(Path::new("a.png")));
        assert!(is_media(Path::new("a.MP4"))); // case-insensitive
        assert!(is_media(Path::new("a.jpeg")));
        assert!(!is_media(Path::new("a.txt")));
        assert!(!is_media(Path::new("noext")));
    }

    #[test]
    fn lang_and_hotkey_tables_are_consistent() {
        assert_eq!(LANGS.len(), 12);
        assert_eq!(LANGS[0], "en");
        let mut seen = std::collections::HashSet::new();
        assert!(LANGS.iter().all(|c| seen.insert(c)), "language codes unique");
        assert_eq!(LANGS[LANGS.iter().position(|&c| c == "pt").unwrap()], "pt");
        assert_eq!(HK_IDS.len(), HK_DEFAULTS.len());
    }
}
