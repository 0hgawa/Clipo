//! Clipo daemon — Tauri 2 host: tray, IPC, hotkey dispatch, window
//! flow, capture pipeline glue.

#![deny(unsafe_op_in_unsafe_fn)]

mod upload;

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use clipo_capture::{
    CAPTURE_JPEG_QUALITY, CaptureEngine, CaptureHandle, OcrText, VideoConfig, VideoRecorder,
    copy_image_to_clipboard, decode_bgra_from_bytes, decode_to_bgra,
    enumerate_capturable_windows, extract_text_from_png, extract_video_thumbnail,
    extract_window_icon, focus_window_and_bounds, save_jpeg, save_png, save_thumbnail_jpeg,
};
use clipo_core::{CapturedImage, Rect};
use clipo_overlay::{Overlay, OverlayEvent};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Builder, Emitter, LogicalPosition, Manager, WebviewWindow};
use tauri_plugin_autostart::ManagerExt as _;
use tauri_plugin_global_shortcut::{GlobalShortcutExt, Shortcut, ShortcutState};
use tauri_plugin_store::StoreExt as _;
use tracing_subscriber::{EnvFilter, fmt};
use windows::Win32::Foundation::{HWND, TRUE};
use windows::Win32::Graphics::Dwm::{DWMWA_TRANSITIONS_FORCEDISABLED, DwmSetWindowAttribute};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::PCWSTR;

// ─────── window labels (tauri.conf.json) ───────

const ACTIONS_LABEL: &str = "actions";
const SETTINGS_LABEL: &str = "settings";
const HISTORY_LABEL: &str = "history";
const TIMER_LABEL: &str = "timer";
const QUICK_LABEL: &str = "quick";
const MENU_LABEL: &str = "menu";
const OCR_LABEL: &str = "ocr";
const EDITOR_LABEL: &str = "editor";
const TRAY_MENU_LABEL: &str = "tray-menu";
const RECORDING_BAR_LABEL: &str = "recording-bar";
const WINDOW_PICKER_LABEL: &str = "window-picker";
const TRAY_ICON_ID: &str = "clipo-tray";
const TRAY_DEFAULT_TOOLTIP: &str = "Clipo — Ctrl+Shift+S to capture";

// ─────── recording defaults ───────

// 30 fps default matches Cap / Icecream / ShareX for screen content.
// At our resolution band, 60 fps loads the encoder + GPU video engine
// enough that background apps (YouTube hardware decoder) stutter. Fast-
// motion content would notice 30; tutorial/UI capture (Clipo's case)
// does not.
const RECORDING_FPS: u32 = 30;
// Cap's heuristic: 30 fps of bits + half the extra frames' bits.
// B-frames + the 30 fps shoulder absorb what naive w·h·fps·bp·s overpays
// for screen content (long static stretches + occasional motion).
const RECORDING_BITRATE_MULTIPLIER: f64 = 0.1;
const RECORDING_BITRATE_CAP_BPS: u32 = 80_000_000;
const RECORDING_BITRATE_FLOOR_BPS: u32 = 2_000_000;

// Window geometry — must match tauri.conf.json declarations.
const RECORDING_BAR_WIDTH: f64 = 320.0;
const RECORDING_BAR_HEIGHT: f64 = 44.0;
const RECORDING_BAR_GAP: f64 = 8.0;
const ACTIONS_WIDTH: f64 = 290.0;
const ACTIONS_HEIGHT: f64 = 204.0;
const ACTIONS_MARGIN: f64 = 12.0;
const TIMER_WIDTH: f64 = 320.0;
const TIMER_HEIGHT: f64 = 220.0;
const QUICK_WIDTH: f64 = 340.0;
const QUICK_HEIGHT: f64 = 440.0;
const QUICK_RIGHT_MARGIN: f64 = 12.0;
const MENU_WIDTH: f64 = 660.0;
const MENU_HEIGHT: f64 = 350.0;
const OCR_WIDTH: f64 = 520.0;
const OCR_HEIGHT: f64 = 420.0;
const OCR_ANCHOR_GAP: f64 = 12.0;
const WINDOW_PICKER_WIDTH: f64 = 380.0;
const WINDOW_PICKER_HEIGHT: f64 = 480.0;

// Settle pause between hiding the picker + raising the target and the
// DXGI grab. Picker shadow/fade transitions need to commit, and
// SW_RESTORE on a minimized target runs a ~200 ms de-minimize animation.
// 220 ms covers both. Shorter (~120) trapped the de-minimize; longer
// (>300) reads as a sluggish "ka-thunk".
const WINDOW_PICKER_SETTLE_MS: u64 = 220;

// Tray menu geometry. Height is per-show: recording layout has fewer
// rows and a fixed window would leave a black gap.
const TRAY_MENU_WIDTH: f64 = 278.0;
const TRAY_MENU_ROW_PX: u32 = 32;
const TRAY_MENU_DIVIDER_PX: u32 = 11;
const TRAY_MENU_GAP_PX: u32 = 1;
const TRAY_MENU_PAD_PX: u32 = 6;
const TRAY_MENU_MARGIN: f64 = 6.0;

const SETTINGS_FILE: &str = "settings.json";
const UPLOADS_STORE: &str = "uploads.json";

// ─────── hotkeys ───────

struct HotkeyDef {
    id: &'static str,
    label: &'static str,
    default_combo: &'static str,
}

const HOTKEY_DEFS: &[HotkeyDef] = &[
    // PrintScreen for region matches Lightshot / Greenshot / Snagit —
    // region is the dominant screenshot use case (~80%).
    HotkeyDef { id: "overlay", label: "Capture region", default_combo: "PrintScreen" },
    HotkeyDef { id: "capture", label: "Capture fullscreen", default_combo: "Shift+PrintScreen" },
    HotkeyDef { id: "window", label: "Capture window", default_combo: "CommandOrControl+Shift+W" },
    HotkeyDef { id: "record-fullscreen", label: "Record fullscreen", default_combo: "CommandOrControl+Shift+R" },
    HotkeyDef { id: "ocr", label: "Extract text (OCR)", default_combo: "CommandOrControl+Shift+T" },
    HotkeyDef { id: "quick", label: "Quick Access", default_combo: "CommandOrControl+Shift+A" },
    HotkeyDef { id: "menu", label: "All-in-one menu", default_combo: "CommandOrControl+Shift+K" },
    // Recording-scoped: F-keys work as global hotkeys because they only
    // register during an active session. F8/F9 match Loom/OBS. F11/F12
    // avoided (browser fullscreen / DevTools collisions).
    HotkeyDef { id: "recording-stop", label: "Stop recording", default_combo: "F8" },
    HotkeyDef { id: "recording-pause", label: "Pause/resume recording", default_combo: "F9" },
    HotkeyDef { id: "recording-restart", label: "Restart recording", default_combo: "F10" },
    HotkeyDef { id: "recording-mute-audio", label: "Toggle system audio mute", default_combo: "F7" },
    HotkeyDef { id: "recording-mute-mic", label: "Toggle microphone mute", default_combo: "F6" },
];

/// True when `id` is a recording-scoped hotkey — only registered with
/// the OS during an active session so the user's normal keyboard isn't
/// claimed.
fn is_recording_scoped(id: &str) -> bool {
    matches!(
        id,
        "recording-pause"
            | "recording-stop"
            | "recording-restart"
            | "recording-mute-audio"
            | "recording-mute-mic"
    )
}

struct ShortcutsRuntime {
    bindings: HashMap<String, Shortcut>,
    status: HashMap<String, &'static str>,
}
type ShortcutsState = Mutex<ShortcutsRuntime>;

// ─────── runtime state ───────

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum OverlayMode {
    #[default]
    Capture,
    Ocr,
    Recording,
}

struct ActiveRecordingSession {
    recorder: VideoRecorder,
    path: PathBuf,
    rect: Rect,
    capture_audio: bool,
    capture_mic: bool,
}
type ActiveRecording = Mutex<Option<ActiveRecordingSession>>;
type OverlayModeState = Mutex<OverlayMode>;

/// What the timer should do when it hits zero. Reset to `Photo` on
/// every `timer_complete` so a future ESC-then-tray-Capture flow can't
/// leak whatever the last recording attempt asked for.
#[derive(Debug, Clone, Copy, Default)]
enum TimerTarget {
    #[default]
    Photo,
    Recording(Rect),
}
type TimerTargetState = Mutex<TimerTarget>;

/// Pull-based: the JS poll catches both "result landed before page
/// reset" and "result landed after" — either way `take_ocr_result`
/// returns it exactly once.
type OcrResults = Mutex<HashMap<String, Result<OcrText, String>>>;
type PendingEditorSources = Mutex<HashMap<String, String>>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureSavedPayload {
    path: String,
    filename: String,
    width: u32,
    height: u32,
    /// Sidecar JPEG path; empty when encode failed. UI converts via
    /// `convertFileSrc` — no inline base64, no PNG-sized event.
    thumbnail_path: String,
    /// `"image"` for screenshots / OCR / edited captures; `"video"` for
    /// recordings. UI switches action set + placeholder per kind.
    kind: String,
}
type PendingCaptures = Mutex<HashMap<String, CaptureSavedPayload>>;

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsData {
    #[serde(default = "default_autostart")]
    autostart: bool,
    /// Override the default `Pictures\Clipo` capture folder. Empty/unset
    /// falls back to default. Useful for cloud-sync folders.
    #[serde(default)]
    capture_folder: Option<String>,
    /// `"png"` (lossless, default) or `"jpg"` (smaller, lossy). Unknown
    /// values fall back to PNG. Only affects screenshots — recordings
    /// are always MP4.
    #[serde(default = "default_image_format")]
    image_format: String,
    #[serde(default)]
    upload_service: upload::UploadService,
    /// Pre-recording "3, 2, 1" countdown. Loom / CleanShot / Icecream
    /// default on: lets menus disappear from frame, lets the user
    /// switch to the target app off-record. `serde(default)` bool is
    /// `false`, so the named fn forces "first-run = on" without
    /// stamping over an explicit `false` from a user.
    #[serde(default = "default_recording_countdown")]
    recording_countdown: bool,
    /// WASAPI loopback. Off skips init entirely (zero overhead).
    #[serde(default = "default_capture_audio")]
    capture_audio: bool,
    /// Mic capture. Default off — opting in is deliberate (Windows
    /// surfaces the mic privacy icon while active).
    #[serde(default)]
    capture_mic: bool,
    #[serde(default = "default_recording_fps")]
    recording_fps: u32,
    /// Composite the system cursor onto recorded frames. Default on —
    /// OBS/ShareX/ShadowPlay convention.
    #[serde(default = "default_show_cursor")]
    show_cursor: bool,
    /// Draw expanding rings at mouse clicks. Default off — useful for
    /// explainer content but noisy for general recording.
    #[serde(default)]
    show_mouse_clicks: bool,
    /// 4× pixel magnifier during region selection. Default off — clean
    /// overlay for everyday captures, opt-in for pixel precision.
    #[serde(default)]
    show_magnifier: bool,
    /// Auto-dismiss timer for the actions panel (ms). Pauses on hover
    /// + during slow actions (upload / GIF export).
    #[serde(default = "default_actions_dismiss_ms")]
    actions_dismiss_ms: u32,
    /// Self-timer countdown (s) — drives both the photo timer and the
    /// pre-recording countdown gated by `recording_countdown`.
    #[serde(default = "default_timer_seconds")]
    timer_seconds: u32,
    /// Left-click tray action: "region" (default), "fullscreen", "menu",
    /// "timer", "ocr". Unknown → region.
    #[serde(default = "default_tray_left_click")]
    tray_left_click: String,
    /// User shortcut overrides keyed by `HotkeyDef::id`. Missing keys =
    /// factory default — fresh settings file keeps every shortcut on
    /// its built-in binding without an upgrade dance.
    #[serde(default)]
    shortcuts: HashMap<String, String>,
    /// ISO-639-1 code. JS i18n catalog looks it up; unknown → English.
    #[serde(default = "default_language")]
    language: String,
}

const fn default_autostart() -> bool { true }
const fn default_recording_countdown() -> bool { true }
const fn default_capture_audio() -> bool { true }
const fn default_recording_fps() -> u32 { RECORDING_FPS }
const fn default_show_cursor() -> bool { true }
const fn default_actions_dismiss_ms() -> u32 { 5000 }
const fn default_timer_seconds() -> u32 { 3 }
fn default_tray_left_click() -> String { "region".to_owned() }
fn default_language() -> String { "en".to_owned() }
fn default_image_format() -> String { "png".to_owned() }

impl Default for SettingsData {
    fn default() -> Self {
        Self {
            autostart: default_autostart(),
            capture_folder: None,
            image_format: default_image_format(),
            upload_service: upload::UploadService::default(),
            recording_countdown: default_recording_countdown(),
            capture_audio: default_capture_audio(),
            capture_mic: false,
            recording_fps: default_recording_fps(),
            show_cursor: default_show_cursor(),
            show_mouse_clicks: false,
            show_magnifier: false,
            actions_dismiss_ms: default_actions_dismiss_ms(),
            timer_seconds: default_timer_seconds(),
            tray_left_click: default_tray_left_click(),
            shortcuts: HashMap::new(),
            language: default_language(),
        }
    }
}

fn resolved_combo<'a>(settings: &'a SettingsData, def: &'a HotkeyDef) -> &'a str {
    settings
        .shortcuts
        .get(def.id)
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(def.default_combo)
}

type AppSettings = Mutex<SettingsData>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct CaptureEntry {
    path: String,
    filename: String,
    size_bytes: u64,
    modified_ms: u64,
    kind: &'static str,
    thumbnail_path: Option<String>,
}

// ─────── entry ───────

#[tracing::instrument(name = "clipo::run")]
#[allow(clippy::too_many_lines)]
pub fn run() {
    install_tracing();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "clipo starting");

    // Load + parse hotkeys BEFORE Builder, so State is registered before
    // the pre-declared windows exist. WebView2 starts loading the
    // instant windows are created — an early `invoke` (get_settings,
    // get_recording_state) hits a missing State otherwise.
    let settings = load_settings();
    let autostart = settings.autostart;
    let shortcuts_runtime = build_shortcuts_runtime(&settings);

    Builder::default()
        // First plugin in the chain: must intercept duplicate launches
        // BEFORE any other init runs. The plugin owns the named mutex
        // + named pipe IPC end-to-end — we just consume the callback
        // it fires inside the original instance and open the
        // all-in-one menu (CleanShot X convention for tray-resident
        // apps with no "main window").
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_menu(app);
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_store::Builder::default().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    if event.state() == ShortcutState::Pressed {
                        dispatch_shortcut(app, shortcut);
                    }
                })
                .build(),
        )
        // Updater verifies each release against the bundled minisign
        // pubkey (tauri.conf.json). process plugin relaunches after install.
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .manage::<AppSettings>(Mutex::new(settings))
        .manage::<ShortcutsState>(Mutex::new(shortcuts_runtime))
        .manage::<PendingCaptures>(Mutex::new(HashMap::new()))
        .manage::<OverlayModeState>(Mutex::new(OverlayMode::default()))
        .manage::<OcrResults>(Mutex::new(HashMap::new()))
        .manage::<PendingEditorSources>(Mutex::new(HashMap::new()))
        .manage::<ActiveRecording>(Mutex::new(None))
        .manage::<TimerTargetState>(Mutex::new(TimerTarget::default()))
        .invoke_handler(tauri::generate_handler![
            take_pending_capture,
            copy_capture_image,
            reveal_in_folder,
            open_file,
            get_settings,
            update_settings,
            reset_settings,
            get_build_info,
            list_captures,
            delete_capture,
            export_to_gif,
            timer_complete,
            menu_pick,
            ocr_extract,
            take_ocr_result,
            copy_text_to_clipboard,
            upload_capture,
            write_text_file,
            open_editor,
            take_editor_source,
            save_annotated,
            copy_annotated_to_clipboard,
            tray_menu_pick,
            dismiss_tray_menu,
            close_window_picker,
            capture_window,
            stop_recording,
            discard_recording,
            restart_recording,
            pause_recording,
            resume_recording,
            set_audio_muted,
            set_mic_muted,
            get_recording_state,
            ensure_video_thumbnail,
            ensure_image_thumbnail,
            get_shortcut_status,
            list_hotkey_defs,
            get_active_shortcuts,
        ])
        .setup(move |app| {
            sync_autostart(app.handle(), autostart);
            build_tray(app.handle()).map_err(|e| format!("tray: {e}"))?;

            // Generate the recording-variant tray icon once at boot
            // (red dot bottom-right). default_window_icon returns a
            // borrow scoped to the App; re-wrap into owned Image so
            // IconVariants is 'static state.
            if let Some(default_ref) = app.handle().default_window_icon() {
                let default = tauri::image::Image::new_owned(
                    default_ref.rgba().to_vec(),
                    default_ref.width(),
                    default_ref.height(),
                );
                let recording = generate_recording_icon_variant(&default);
                app.manage(IconVariants { default, recording });
            }

            let engine = CaptureEngine::start().map_err(|e| format!("capture engine: {e}"))?;
            let capture_handle = engine.handle();
            app.manage(capture_handle.clone());
            app.manage(engine);

            let overlay = Overlay::spawn().map_err(|e| format!("overlay spawn: {e}"))?;
            let overlay_events = overlay.events();
            app.manage(overlay);

            let app_for_events = app.handle().clone();
            tauri::async_runtime::spawn_blocking(move || {
                run_overlay_events(&app_for_events, &overlay_events, &capture_handle);
            });

            // Actually register the parsed shortcuts and capture per-id
            // status the Settings UI reads.
            let gs = app.global_shortcut();
            if let Some(state) = app.try_state::<ShortcutsState>() {
                refresh_status(gs, &mut state.lock());
            }

            // Pre-paint the tray menu so first click feels instant. Pays
            // WebView2 init (~50 ms) at startup, invisible for a daemon
            // that runs all day.
            prewarm_tray_menu(app.handle());

            // Skip DWM fade-out on popups: every hide() otherwise gets a
            // 100-200 ms ghost fade that reads as lag vs the click.
            for label in [
                ACTIONS_LABEL, SETTINGS_LABEL, HISTORY_LABEL, TIMER_LABEL,
                QUICK_LABEL, MENU_LABEL, OCR_LABEL, EDITOR_LABEL,
                RECORDING_BAR_LABEL,
            ] {
                if let Some(win) = app.handle().get_webview_window(label) {
                    disable_dwm_transitions(&win);
                }
            }

            tracing::info!("clipo ready");
            Ok(())
        })
        .on_window_event(|window, event| {
            // Pre-declared windows are reusable surfaces — hide instead
            // of close so the next open reuses the same WebView2.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Err(e) = window.hide() {
                    tracing::error!(error = %e, label = window.label(), "hide on close-requested");
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("failed to run clipo");
}

fn install_tracing() {
    use tracing_subscriber::fmt::format::FmtSpan;
    let filter = EnvFilter::try_from_env("CLIPO_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .compact()
        .init();
}

// ─────── shortcuts ───────

fn show_magnifier_pref(app: &AppHandle) -> bool {
    app.try_state::<AppSettings>()
        .is_some_and(|s| s.lock().show_magnifier)
}

#[tracing::instrument(skip(app))]
fn dispatch_shortcut(app: &AppHandle, shortcut: &Shortcut) {
    let Some(id) = find_hotkey_id(app, shortcut) else {
        // User just rebound; OS delivered a queued press before the
        // unregister landed. Safe to drop.
        return;
    };
    match id.as_str() {
        "overlay" => {
            if reject_during_recording(app, "overlay") { return; }
            if let Some(overlay) = app.try_state::<Overlay>() {
                overlay.toggle(show_magnifier_pref(app));
            }
        }
        "capture" => {
            if reject_during_recording(app, "capture") { return; }
            spawn_fullscreen_capture(app);
        }
        // show_window_picker has its own reject_during_recording inside.
        "window" => show_window_picker(app),
        "quick" => show_quick_access(app),
        "menu" => show_menu(app),
        "ocr" => {
            if reject_during_recording(app, "ocr") { return; }
            start_ocr_capture(app);
        }
        "record-fullscreen" => toggle_fullscreen_recording(app),
        // Recording-scoped: OS only delivers while register_recording_shortcuts
        // has them live, but a press queued the instant `stop_recording` ran
        // could land after teardown — re-check `is_recording_active` to swallow it.
        "recording-pause" => {
            if !is_recording_active(app) { return; }
            if let Some(s) = snapshot_recording_state(app) {
                if s.paused { resume_recording(app.clone()); }
                else { pause_recording(app.clone()); }
            }
        }
        "recording-stop" => {
            if is_recording_active(app) { stop_recording(app.clone()); }
        }
        "recording-restart" => {
            if is_recording_active(app) { restart_recording(app.clone()); }
        }
        "recording-mute-audio" => {
            if let Some(s) = snapshot_recording_state(app)
                && s.audio_enabled
            {
                set_audio_muted(app.clone(), !s.audio_muted);
            }
        }
        "recording-mute-mic" => {
            if let Some(s) = snapshot_recording_state(app)
                && s.mic_enabled
            {
                set_mic_muted(app.clone(), !s.mic_muted);
            }
        }
        other => tracing::warn!(id = other, "dispatch_shortcut: unknown hotkey id"),
    }
}

fn find_hotkey_id(app: &AppHandle, shortcut: &Shortcut) -> Option<String> {
    let state = app.try_state::<ShortcutsState>()?;
    let runtime = state.lock();
    runtime
        .bindings
        .iter()
        .find(|(_, sc)| *sc == shortcut)
        .map(|(id, _)| id.clone())
}

fn build_shortcuts_runtime(settings: &SettingsData) -> ShortcutsRuntime {
    let mut bindings = HashMap::with_capacity(HOTKEY_DEFS.len());
    let mut status = HashMap::with_capacity(HOTKEY_DEFS.len());
    for def in HOTKEY_DEFS {
        let combo = resolved_combo(settings, def);
        match combo.parse::<Shortcut>() {
            Ok(sc) => {
                bindings.insert(def.id.to_string(), sc);
            }
            Err(e) => {
                tracing::warn!(id = def.id, combo, error = %e, "invalid combo; using default");
                if let Ok(sc) = def.default_combo.parse::<Shortcut>() {
                    bindings.insert(def.id.to_string(), sc);
                }
                status.insert(def.id.to_string(), "invalid");
            }
        }
    }
    ShortcutsRuntime { bindings, status }
}

fn refresh_status(
    gs: &tauri_plugin_global_shortcut::GlobalShortcut<tauri::Wry>,
    runtime: &mut ShortcutsRuntime,
) {
    for (id, sc) in &runtime.bindings {
        if runtime.status.get(id).copied() == Some("invalid") {
            continue;
        }
        if is_recording_scoped(id) {
            runtime.status.insert(id.clone(), "inactive");
            continue;
        }
        let outcome = match gs.register(*sc) {
            Ok(()) => "active",
            Err(e) => {
                tracing::warn!(id, error = %e, "register hotkey — likely in use by another app");
                "conflict"
            }
        };
        runtime.status.insert(id.clone(), outcome);
    }
}

/// Apply a register/unregister to every recording-scoped binding.
/// Centralised so the start/stop paths can't drift in subtle ways.
fn apply_to_recording_shortcuts<F>(app: &AppHandle, mut op: F)
where
    F: FnMut(
        &tauri_plugin_global_shortcut::GlobalShortcut<tauri::Wry>,
        &String,
        Shortcut,
        &mut HashMap<String, &'static str>,
    ),
{
    let Some(rt_state) = app.try_state::<ShortcutsState>() else { return };
    let gs = app.global_shortcut();
    let mut runtime = rt_state.lock();
    // Snapshot so we can iterate without holding the bindings borrow
    // while mutating status.
    let pending: Vec<(String, Shortcut)> = runtime
        .bindings
        .iter()
        .filter(|(id, _)| is_recording_scoped(id))
        .map(|(id, sc)| (id.clone(), *sc))
        .collect();
    for (id, sc) in pending {
        if runtime.status.get(&id).copied() == Some("invalid") {
            continue;
        }
        op(gs, &id, sc, &mut runtime.status);
    }
}

fn register_recording_shortcuts(app: &AppHandle) {
    apply_to_recording_shortcuts(app, |gs, id, sc, status| {
        let outcome = match gs.register(sc) {
            Ok(()) => "active",
            Err(e) => {
                tracing::warn!(id, error = %e, "register recording hotkey");
                "conflict"
            }
        };
        status.insert(id.clone(), outcome);
    });
}

fn unregister_recording_shortcuts(app: &AppHandle) {
    apply_to_recording_shortcuts(app, |gs, id, sc, status| {
        // Only unregister what we believed active; trying to unregister
        // "inactive" is a no-op (and would log a spurious warn).
        if status.get(id).copied() == Some("active")
            && let Err(e) = gs.unregister(sc)
        {
            tracing::warn!(id, error = %e, "unregister recording hotkey");
        }
        status.insert(id.clone(), "inactive");
    });
}

/// Recording-active gate — DXGI Desktop Duplication is per-output
/// exclusive, so spawning a screenshot while the recorder owns its own
/// duplication fails with DXGI_ERROR_NOT_CURRENTLY_AVAILABLE.
fn is_recording_active(app: &AppHandle) -> bool {
    app.try_state::<ActiveRecording>()
        .is_some_and(|s| s.lock().is_some())
}

fn reject_during_recording(app: &AppHandle, action: &str) -> bool {
    let busy = is_recording_active(app);
    if busy {
        tracing::warn!(action, "capture rejected — recording in progress");
    }
    busy
}

// ─────── capture pipeline ───────

fn toggle_fullscreen_recording(app: &AppHandle) {
    // Same hotkey starts/stops so the user doesn't have to remember
    // a separate stop combo.
    if let Some(state) = app.try_state::<ActiveRecording>()
        && state.lock().is_some()
    {
        stop_recording(app.clone());
        return;
    }
    let Some(monitor) = app.primary_monitor().ok().flatten() else {
        tracing::warn!("record-fullscreen: no primary monitor available");
        return;
    };
    // Tauri reports monitor position/size in physical pixels for the
    // virtual desktop; VideoRecorder expects the same space.
    let pos = monitor.position();
    let size = monitor.size();
    let rect = Rect {
        x: pos.x,
        y: pos.y,
        width: size.width,
        height: size.height,
    };
    start_region_recording(app, rect);
}

fn start_ocr_capture(app: &AppHandle) {
    if let Some(mode) = app.try_state::<OverlayModeState>() {
        *mode.lock() = OverlayMode::Ocr;
    }
    if let Some(overlay) = app.try_state::<Overlay>() {
        overlay.toggle(show_magnifier_pref(app));
    }
}

fn start_recording_capture(app: &AppHandle) {
    if let Some(state) = app.try_state::<ActiveRecording>()
        && state.lock().is_some()
    {
        tracing::warn!("recording already in progress; ignoring start");
        return;
    }
    if let Some(mode) = app.try_state::<OverlayModeState>() {
        *mode.lock() = OverlayMode::Recording;
    }
    if let Some(overlay) = app.try_state::<Overlay>() {
        overlay.toggle(show_magnifier_pref(app));
    }
}

#[tracing::instrument(skip(app))]
fn spawn_fullscreen_capture(app: &AppHandle) {
    let Some(handle) = app.try_state::<CaptureHandle>() else {
        tracing::error!("capture handle missing");
        return;
    };
    let handle = (*handle).clone();
    let app_handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || run_fullscreen_capture(&app_handle, &handle));
}

#[tracing::instrument(skip(app, events, capture))]
fn run_overlay_events(
    app: &AppHandle,
    events: &flume::Receiver<OverlayEvent>,
    capture: &CaptureHandle,
) {
    while let Ok(event) = events.recv() {
        if let Some(overlay) = app.try_state::<Overlay>() {
            overlay.mark_hidden();
        }
        let mode = take_overlay_mode(app);
        match event {
            OverlayEvent::Confirmed(rect) => match mode {
                OverlayMode::Capture => run_region_capture(app, capture, rect),
                OverlayMode::Ocr => run_region_ocr(app, capture, rect),
                OverlayMode::Recording => start_region_recording(app, rect),
            },
            OverlayEvent::Cancelled => tracing::debug!(?mode, "overlay cancelled"),
        }
    }
}

/// Read-and-reset the mode so a cancelled OCR doesn't leak into the
/// next plain Ctrl+Shift+S.
fn take_overlay_mode(app: &AppHandle) -> OverlayMode {
    app.try_state::<OverlayModeState>()
        .map(|state| std::mem::take(&mut *state.lock()))
        .unwrap_or_default()
}

#[tracing::instrument(skip(app, capture))]
fn run_region_capture(app: &AppHandle, capture: &CaptureHandle, rect: Rect) {
    match capture.capture_region(rect) {
        Ok(image) => persist(app, &image),
        Err(e) => tracing::error!(error = %e, ?rect, "region capture failed"),
    }
}

#[tracing::instrument(skip(app, handle))]
fn run_fullscreen_capture(app: &AppHandle, handle: &CaptureHandle) {
    match handle.capture_primary() {
        Ok(image) => persist(app, &image),
        Err(e) => tracing::error!(error = %e, "fullscreen capture failed"),
    }
}

#[tracing::instrument(skip(app, capture))]
fn run_region_ocr(app: &AppHandle, capture: &CaptureHandle, rect: Rect) {
    let image = match capture.capture_region(rect) {
        Ok(i) => i,
        Err(e) => {
            tracing::error!(error = %e, ?rect, "ocr capture failed");
            return;
        }
    };
    // Save alongside normal captures so the OCR result has a permanent
    // source in History — but skip the actions panel: the OCR window
    // is the focal surface here.
    let path = match save_capture_image(app, &image) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "ocr save capture");
            return;
        }
    };
    let sidecar = thumb_cache_path(&path);
    if let Some(sidecar) = sidecar.as_ref()
        && let Err(e) = save_thumbnail_jpeg(&image, sidecar)
    {
        tracing::warn!(error = %e, path = %sidecar.display(), "ocr sidecar thumb save");
    }
    if let Err(e) = copy_image_to_clipboard(&image) {
        tracing::warn!(error = %e, "ocr clipboard image");
    }
    let payload = build_saved_payload(&path, image.width, image.height, sidecar.as_deref(), "image");
    if let Err(e) = app.emit("capture:saved", &payload) {
        tracing::warn!(error = %e, "emit capture:saved (ocr)");
    }
    start_ocr_for_path(app, path.to_string_lossy().into_owned(), Some(rect));
}

fn start_region_recording(app: &AppHandle, rect: Rect) {
    let countdown_enabled = app
        .try_state::<AppSettings>()
        .is_none_or(|s| s.lock().recording_countdown);
    if countdown_enabled {
        show_timer_for(app, TimerTarget::Recording(rect));
    } else {
        start_region_recording_now(app, rect);
    }
}

#[tracing::instrument(skip(app))]
fn start_region_recording_now(app: &AppHandle, rect: Rect) {
    let path = match resolve_capture_path_with_ext(app, "mp4") {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = e, "recording: resolve path");
            return;
        }
    };
    let (capture_audio, capture_mic, fps, show_cursor, show_clicks) = app
        .try_state::<AppSettings>()
        .map_or((true, false, RECORDING_FPS, true, false), |s| {
            let g = s.lock();
            (g.capture_audio, g.capture_mic, g.recording_fps, g.show_cursor, g.show_mouse_clicks)
        });
    let bitrate_bps = compute_recording_bitrate(rect.width, rect.height, fps);
    let recorder = match VideoRecorder::start(VideoConfig {
        rect,
        output: path.clone(),
        fps,
        bitrate_bps,
        capture_audio,
        capture_mic,
        show_cursor,
        show_clicks,
    }) {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, ?rect, "recording: start failed");
            return;
        }
    };
    tracing::info!(
        path = %path.display(),
        ?rect,
        bitrate_mbps = bitrate_bps / 1_000_000,
        "recording: started"
    );
    if let Some(state) = app.try_state::<ActiveRecording>() {
        *state.lock() = Some(ActiveRecordingSession {
            recorder,
            path,
            rect,
            capture_audio,
            capture_mic,
        });
    }
    // Show overlay BEFORE the bar — both are alwaysOnTop; between
    // topmost windows Windows keeps the more-recently-shown on top.
    // Bar-first put the 37% black dim on top of the bar, washing out
    // the Stop button.
    if let Some(overlay) = app.try_state::<Overlay>() {
        overlay.show_recording_indicator(rect);
    }
    show_recording_bar(app, rect);
    spawn_tray_tooltip_timer(app);
    register_recording_shortcuts(app);
    if let Err(e) = app.emit("recording:started", ()) {
        tracing::warn!(error = %e, "emit recording:started");
    }
    if let Err(e) = app.emit("shortcuts:updated", ()) {
        tracing::warn!(error = %e, "emit shortcuts:updated (recording on)");
    }
}

/// Cap's bitrate heuristic (`crates/enc-mediafoundation/.../h264.rs`).
/// 30 fps of bits + half the extra frames' bits. Clamped so a tiny rect
/// doesn't underflow MF and an 8K accident doesn't produce 200+ Mbps.
#[allow(clippy::cast_possible_truncation)]
fn compute_recording_bitrate(width: u32, height: u32, fps: u32) -> u32 {
    let fps_shoulder = f64::from(fps.saturating_sub(30)) / 2.0 + 30.0;
    let raw = f64::from(width) * f64::from(height) * fps_shoulder * RECORDING_BITRATE_MULTIPLIER;
    let clamped = (raw as i64).clamp(
        i64::from(RECORDING_BITRATE_FLOOR_BPS),
        i64::from(RECORDING_BITRATE_CAP_BPS),
    );
    u32::try_from(clamped).unwrap_or(RECORDING_BITRATE_CAP_BPS)
}

/// Logical-pixel work area + cursor anchor on the monitor that contains
/// `point` (when given) or the primary monitor.
struct WorkArea {
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
}

impl WorkArea {
    fn width(&self) -> f64 { self.right - self.left }
    fn height(&self) -> f64 { self.bottom - self.top }
}

/// Pick the monitor containing `point` (physical pixels) or fall back
/// to primary. Returns work area in LOGICAL pixels so callers can use
/// it with `set_position(LogicalPosition)` and DPI is handled cleanly.
fn work_area_at(app: &AppHandle, point: Option<(f64, f64)>) -> Option<WorkArea> {
    let monitor = point
        .and_then(|(x, y)| app.monitor_from_point(x, y).ok().flatten())
        .or_else(|| app.primary_monitor().ok().flatten())?;
    let scale = monitor.scale_factor();
    let work = monitor.work_area();
    let left = f64::from(work.position.x) / scale;
    let top = f64::from(work.position.y) / scale;
    Some(WorkArea {
        left,
        top,
        right: left + f64::from(work.size.width) / scale,
        bottom: top + f64::from(work.size.height) / scale,
    })
}

fn show_recording_bar(app: &AppHandle, rect: Rect) {
    let Some(win) = app.get_webview_window(RECORDING_BAR_LABEL) else {
        tracing::warn!("recording-bar window missing");
        return;
    };
    let Some(work) = work_area_at(app, Some((f64::from(rect.x), f64::from(rect.y)))) else {
        tracing::warn!("recording-bar: no monitor for placement");
        return;
    };
    let scale = app
        .monitor_from_point(f64::from(rect.x), f64::from(rect.y))
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten())
        .map_or(1.0, |m| m.scale_factor());

    let rect_left = f64::from(rect.x) / scale;
    let rect_top = f64::from(rect.y) / scale;
    let rect_right = rect_left + f64::from(rect.width) / scale;
    let rect_bottom = rect_top + f64::from(rect.height) / scale;
    let Some((x, y)) = pick_recording_bar_position(
        rect_left, rect_top, rect_right, rect_bottom,
        work.left, work.top, work.right, work.bottom,
    ) else {
        // Fullscreen: nowhere off-rect for the bar. Stay hidden rather
        // than bake it into the capture; WDA_EXCLUDEFROMCAPTURE paints
        // a black square under DXGI. Tray + shortcuts drive Stop/Pause.
        if let Err(e) = win.hide() {
            tracing::warn!(error = %e, "recording-bar hide");
        }
        return;
    };
    if let Err(e) = win.set_position(LogicalPosition::new(x, y)) {
        tracing::warn!(error = %e, "recording-bar set_position");
    }
    if let Err(e) = win.show() {
        tracing::warn!(error = %e, "recording-bar show");
    }
}

#[allow(clippy::too_many_arguments)]
fn pick_recording_bar_position(
    rect_left: f64, rect_top: f64, rect_right: f64, rect_bottom: f64,
    work_left: f64, work_top: f64, work_right: f64, work_bottom: f64,
) -> Option<(f64, f64)> {
    let rect_centre_x = f64::midpoint(rect_left, rect_right);
    let rect_centre_y = f64::midpoint(rect_top, rect_bottom);
    let bar_w = RECORDING_BAR_WIDTH;
    let bar_h = RECORDING_BAR_HEIGHT;
    let gap = RECORDING_BAR_GAP;

    let below_y = rect_bottom + gap;
    if below_y + bar_h <= work_bottom {
        let x = (rect_centre_x - bar_w / 2.0).clamp(work_left, work_right - bar_w);
        return Some((x, below_y));
    }
    let above_y = rect_top - gap - bar_h;
    if above_y >= work_top {
        let x = (rect_centre_x - bar_w / 2.0).clamp(work_left, work_right - bar_w);
        return Some((x, above_y));
    }
    let right_x = rect_right + gap;
    if right_x + bar_w <= work_right {
        let y = (rect_centre_y - bar_h / 2.0).clamp(work_top, work_bottom - bar_h);
        return Some((right_x, y));
    }
    let left_x = rect_left - gap - bar_w;
    if left_x >= work_left {
        let y = (rect_centre_y - bar_h / 2.0).clamp(work_top, work_bottom - bar_h);
        return Some((left_x, y));
    }
    None
}

// ─────── recording lifecycle commands ───────

#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Default, serde::Serialize)]
struct RecordingState {
    active: bool,
    paused: bool,
    audio_muted: bool,
    mic_muted: bool,
    audio_enabled: bool,
    mic_enabled: bool,
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn get_recording_state(app: AppHandle) -> RecordingState {
    snapshot_recording_state(&app).unwrap_or_default()
}

fn snapshot_recording_state(app: &AppHandle) -> Option<RecordingState> {
    let state = app.try_state::<ActiveRecording>()?;
    let guard = state.lock();
    let session = guard.as_ref()?;
    let snapshot = RecordingState {
        active: true,
        paused: session.recorder.is_paused(),
        audio_muted: session.recorder.is_audio_muted(),
        mic_muted: session.recorder.is_mic_muted(),
        audio_enabled: session.capture_audio,
        mic_enabled: session.capture_mic,
    };
    drop(guard);
    Some(snapshot)
}

fn emit_recording_state_change(app: &AppHandle) {
    if let Some(snapshot) = snapshot_recording_state(app)
        && let Err(e) = app.emit("recording:state-changed", snapshot)
    {
        tracing::warn!(error = %e, "emit recording:state-changed");
    }
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn pause_recording(app: AppHandle) {
    // Skip emit when already in requested state — the flag is
    // idempotent at the atomic level, but a no-op event would tell
    // surfaces "state changed" when it didn't.
    if mutate_recorder(&app, |r| {
        let prev = r.is_paused();
        r.pause();
        !prev
    }) == Some(true)
    {
        emit_recording_state_change(&app);
    }
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn resume_recording(app: AppHandle) {
    if mutate_recorder(&app, |r| {
        let prev = r.is_paused();
        r.resume();
        prev
    }) == Some(true)
    {
        emit_recording_state_change(&app);
    }
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn set_mic_muted(app: AppHandle, muted: bool) {
    if mutate_recorder(&app, |r| {
        let prev = r.is_mic_muted();
        r.set_mic_muted(muted);
        prev != muted
    }) == Some(true)
    {
        emit_recording_state_change(&app);
    }
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn set_audio_muted(app: AppHandle, muted: bool) {
    if mutate_recorder(&app, |r| {
        let prev = r.is_audio_muted();
        r.set_audio_muted(muted);
        prev != muted
    }) == Some(true)
    {
        emit_recording_state_change(&app);
    }
}

/// Run `f` against the active recorder under the lock. Closure body is
/// the ONLY code that runs while the lock is held — guard drops before
/// this returns so callers (event emit, IPC reply) don't serialise
/// behind recording state.
fn mutate_recorder<F, T>(app: &AppHandle, f: F) -> Option<T>
where
    F: FnOnce(&VideoRecorder) -> T,
{
    let state = app.try_state::<ActiveRecording>()?;
    let guard = state.lock();
    let session = guard.as_ref()?;
    let out = f(&session.recorder);
    drop(guard);
    Some(out)
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn discard_recording(app: AppHandle) {
    let Some(state) = app.try_state::<ActiveRecording>() else { return };
    let session = state.lock().take();
    if let Some(win) = app.get_webview_window(RECORDING_BAR_LABEL) {
        let _ = win.hide();
    }
    if let Some(overlay) = app.try_state::<Overlay>() {
        overlay.hide_recording_indicator();
    }
    let Some(session) = session else { return };
    // Release scoped hotkeys synchronously before the encoder finalises
    // on the blocking pool — next capture is free to claim the same
    // combos the moment we return.
    unregister_recording_shortcuts(&app);
    if let Err(e) = app.emit("recording:stopped", ()) {
        tracing::warn!(error = %e, "emit recording:stopped (discard)");
    }
    if let Err(e) = app.emit("shortcuts:updated", ()) {
        tracing::warn!(error = %e, "emit shortcuts:updated (discard)");
    }
    let ActiveRecordingSession { recorder, path, .. } = session;
    tauri::async_runtime::spawn_blocking(move || {
        // recorder.stop() still called (vs killing the encoder) so the
        // SinkWriter releases the file handle — Windows refuses
        // remove_file for ~250 ms otherwise.
        if let Err(e) = recorder.stop() {
            tracing::error!(error = %e, "recording: stop failed (discard)");
        }
        // Best-effort cleanup; NotFound = encoder failed early or
        // sidecar never written.
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %e, path = %path.display(), "discard: remove mp4");
        }
        if let Some(sidecar) = thumb_cache_path(&path)
            && let Err(e) = std::fs::remove_file(&sidecar)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %e, path = %sidecar.display(), "discard: remove sidecar");
        }
        tracing::info!(path = %path.display(), "recording: discarded");
    });
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn restart_recording(app: AppHandle) {
    let Some(state) = app.try_state::<ActiveRecording>() else { return };
    let Some(session) = state.lock().take() else { return };
    let ActiveRecordingSession { recorder, path, rect, .. } = session;
    let app_clone = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(e) = recorder.stop() {
            tracing::error!(error = %e, "recording: stop failed (restart)");
        }
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %e, path = %path.display(), "restart: remove mp4");
        }
        if let Some(thumb) = thumb_cache_path(&path)
            && let Err(e) = std::fs::remove_file(&thumb)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(error = %e, path = %thumb.display(), "restart: remove thumb");
        }
        tracing::info!(path = %path.display(), ?rect, "recording: restarting");
        start_region_recording_now(&app_clone, rect);
    });
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn stop_recording(app: AppHandle) {
    let Some(state) = app.try_state::<ActiveRecording>() else { return };
    let session = state.lock().take();
    if let Some(win) = app.get_webview_window(RECORDING_BAR_LABEL) {
        let _ = win.hide();
    }
    if let Some(overlay) = app.try_state::<Overlay>() {
        overlay.hide_recording_indicator();
    }
    let Some(session) = session else {
        // Stale Stop (double click, hotkey race) after we already tore
        // down. Emitting here would flip surfaces out of a state they
        // were never in.
        return;
    };
    unregister_recording_shortcuts(&app);
    // Announce state flip immediately — tray menu swaps back from
    // recording layout without waiting for the 250 ms finalise.
    if let Err(e) = app.emit("recording:stopped", ()) {
        tracing::warn!(error = %e, "emit recording:stopped");
    }
    if let Err(e) = app.emit("shortcuts:updated", ()) {
        tracing::warn!(error = %e, "emit shortcuts:updated (recording off)");
    }
    let ActiveRecordingSession { recorder, path, rect, .. } = session;
    // Clone the screenshot pipeline's handle so the blocking worker
    // can grab the thumb frame. The grab MUST happen after
    // `recorder.stop()` — DXGI Desktop Duplication is per-output
    // exclusive; while the recorder is active our screenshot device
    // can't open its own duplication (DXGI_ERROR_NOT_CURRENTLY_AVAILABLE).
    let capture_handle = app.try_state::<CaptureHandle>().map(|h| (*h).clone());
    let app_handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(e) = recorder.stop() {
            tracing::error!(error = %e, "recording: stop failed");
            return;
        }
        tracing::info!("recording: stopped + finalised");

        // One DXGI grab → one sidecar JPEG. Failure leaves
        // thumbnail_path empty; UI renders the Video glyph placeholder.
        let sidecar = capture_handle
            .and_then(|h| {
                h.capture_region(rect)
                    .inspect_err(|e| tracing::warn!(error = %e, ?rect, "recording: thumb DXGI grab failed"))
                    .ok()
            })
            .zip(thumb_cache_path(&path))
            .and_then(|(img, sidecar)| {
                save_thumbnail_jpeg(&img, &sidecar)
                    .inspect_err(|e| tracing::warn!(error = %e, path = %sidecar.display(), "recording: sidecar thumb save"))
                    .ok()
                    .map(|()| sidecar)
            });
        let payload = build_saved_payload(&path, rect.width, rect.height, sidecar.as_deref(), "video");
        show_actions(&app_handle, &payload);
    });
}

/// Build the post-capture payload shared by image + video flows.
/// `kind` = `"image"` or `"video"`; `(width, height)` = pixel dims.
fn build_saved_payload(
    path: &Path,
    width: u32,
    height: u32,
    sidecar: Option<&Path>,
    kind: &str,
) -> CaptureSavedPayload {
    CaptureSavedPayload {
        path: path.to_string_lossy().into_owned(),
        filename: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        width,
        height,
        thumbnail_path: sidecar
            .filter(|p| p.exists())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
        kind: kind.to_string(),
    }
}

// ─────── OCR ───────

fn start_ocr_for_path(app: &AppHandle, path: String, anchor: Option<Rect>) {
    if let Some(state) = app.try_state::<OcrResults>() {
        state.lock().remove(OCR_LABEL);
    }
    show_ocr_window(app, anchor);
    if let Err(e) = app.emit_to(OCR_LABEL, "ocr:start", ()) {
        tracing::warn!(error = %e, "emit ocr:start");
    }

    let app_for_worker = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let outcome = run_ocr(&path);
        if let Err(ref e) = outcome {
            tracing::error!(error = %e, path = %path, "ocr failed");
        }
        if let Some(state) = app_for_worker.try_state::<OcrResults>() {
            state.lock().insert(OCR_LABEL.to_string(), outcome);
        }
        if let Err(e) = app_for_worker.emit_to(OCR_LABEL, "ocr:result", ()) {
            tracing::warn!(error = %e, "emit ocr:result");
        }
    });
}

#[tracing::instrument(skip_all, fields(path = %path))]
fn run_ocr(path: &str) -> Result<OcrText, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    extract_text_from_png(&bytes).map_err(|e| e.to_string())
}

fn show_ocr_window(app: &AppHandle, anchor: Option<Rect>) {
    let Some(win) = app.get_webview_window(OCR_LABEL) else {
        tracing::error!("ocr window missing from manifest");
        return;
    };
    let (x, y) = anchor.map_or_else(
        || center_on_primary(app, OCR_WIDTH, OCR_HEIGHT),
        |rect| ocr_anchored_position(app, rect),
    );
    present(&win, x, y);
}

fn ocr_anchored_position(app: &AppHandle, rect: Rect) -> (f64, f64) {
    let Some(work) = work_area_at(app, None) else { return (0.0, 0.0) };
    let Some(monitor) = app.primary_monitor().ok().flatten() else { return (0.0, 0.0) };
    let scale = monitor.scale_factor();

    let rect_left = f64::from(rect.x) / scale;
    let rect_top = f64::from(rect.y) / scale;
    let rect_w = f64::from(rect.width) / scale;
    let rect_h = f64::from(rect.height) / scale;
    let rect_center_x = rect_left + rect_w / 2.0;
    let rect_bottom = rect_top + rect_h;

    let x = (rect_center_x - OCR_WIDTH / 2.0).clamp(work.left, work.right - OCR_WIDTH);
    let below_y = rect_bottom + OCR_ANCHOR_GAP;
    let y = if below_y + OCR_HEIGHT <= work.bottom {
        below_y
    } else {
        // No room below — try above the selection; clamp to work area
        // as last resort.
        (rect_top - OCR_ANCHOR_GAP - OCR_HEIGHT).max(work.top)
    };
    (x, y)
}

// ─────── persist screenshot ───────

#[tracing::instrument(skip(app, image), fields(w = image.width, h = image.height))]
fn persist(app: &AppHandle, image: &CapturedImage) {
    let path = match save_capture_image(app, image) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(error = %e, "save capture");
            return;
        }
    };
    tracing::info!(path = %path.display(), "capture saved");

    // Sidecar JPEG so every "show a thumb" surface reads ~15 KB instead
    // of decoding the full PNG into VRAM. `image` is still in memory —
    // encode is ~10 ms even at 4K.
    let sidecar = thumb_cache_path(&path);
    if let Some(sidecar) = sidecar.as_ref()
        && let Err(e) = save_thumbnail_jpeg(image, sidecar)
    {
        tracing::warn!(error = %e, path = %sidecar.display(), "image sidecar thumb save");
    }

    let payload = build_saved_payload(&path, image.width, image.height, sidecar.as_deref(), "image");
    show_actions(app, &payload);

    if let Err(e) = copy_image_to_clipboard(image) {
        tracing::warn!(error = %e, "clipboard copy");
    }
}

// ─────── actions panel + windows ───────

#[tracing::instrument(skip(app, payload))]
fn show_actions(app: &AppHandle, payload: &CaptureSavedPayload) {
    let Some(win) = app.get_webview_window(ACTIONS_LABEL) else {
        tracing::error!("actions window missing from manifest");
        return;
    };
    let (x, y) = compute_actions_position(app);

    if let Some(state) = app.try_state::<PendingCaptures>() {
        state.lock().insert(ACTIONS_LABEL.to_string(), payload.clone());
    }

    present(&win, x, y);

    if let Err(e) = app.emit_to(ACTIONS_LABEL, "actions:show", payload) {
        tracing::error!(error = %e, "emit_to actions:show");
    }
    if let Err(e) = app.emit("capture:saved", payload) {
        tracing::error!(error = %e, "emit capture:saved");
    }
}

fn present(win: &WebviewWindow, x: f64, y: f64) {
    if let Err(e) = win.set_position(LogicalPosition::new(x, y)) {
        tracing::error!(error = %e, label = win.label(), "set_position");
    }
    if let Err(e) = win.show() {
        tracing::error!(error = %e, label = win.label(), "show");
    }
    if let Err(e) = win.set_focus() {
        tracing::error!(error = %e, label = win.label(), "set_focus");
    }
}

fn compute_actions_position(app: &AppHandle) -> (f64, f64) {
    work_area_at(app, None).map_or((ACTIONS_MARGIN, ACTIONS_MARGIN), |w| {
        (
            w.right - ACTIONS_WIDTH - ACTIONS_MARGIN,
            w.bottom - ACTIONS_HEIGHT - ACTIONS_MARGIN,
        )
    })
}

fn show_chrome_window(app: &AppHandle, label: &str) {
    let Some(win) = app.get_webview_window(label) else {
        tracing::error!(label, "chrome window missing from manifest");
        return;
    };
    // Centre before showing — pre-declared windows otherwise land at
    // CW_USEDEFAULT (cascade down-right) and "open History" arrives
    // off-centre with the bottom clipped.
    if let Err(e) = win.center() {
        tracing::warn!(error = %e, label, "center");
    }
    if let Err(e) = win.show() {
        tracing::error!(error = %e, label, "show");
    }
    if let Err(e) = win.unminimize() {
        tracing::error!(error = %e, label, "unminimize");
    }
    if let Err(e) = win.set_focus() {
        tracing::error!(error = %e, label, "set_focus");
    }
}

#[tracing::instrument(skip(app))]
fn show_timer(app: &AppHandle) {
    show_timer_for(app, TimerTarget::Photo);
}

fn show_timer_for(app: &AppHandle, target: TimerTarget) {
    // Stash the target BEFORE raising the window — a concurrent
    // show_timer_for would otherwise overwrite ours.
    if let Some(state) = app.try_state::<TimerTargetState>() {
        *state.lock() = target;
    }
    let Some(win) = app.get_webview_window(TIMER_LABEL) else {
        tracing::error!("timer window missing from manifest");
        return;
    };
    let (x, y) = center_on_primary(app, TIMER_WIDTH, TIMER_HEIGHT);
    present(&win, x, y);
    let seconds = app
        .try_state::<AppSettings>()
        .map_or_else(default_timer_seconds, |s| s.lock().timer_seconds);
    if let Err(e) = app.emit_to(TIMER_LABEL, "timer:start", seconds) {
        tracing::error!(error = %e, "emit timer:start");
    }
}

#[tracing::instrument(skip(app))]
fn show_quick_access(app: &AppHandle) {
    let Some(win) = app.get_webview_window(QUICK_LABEL) else {
        tracing::error!("quick window missing from manifest");
        return;
    };
    let (x, y) = right_centered_on_primary(app, QUICK_WIDTH, QUICK_HEIGHT, QUICK_RIGHT_MARGIN);
    present(&win, x, y);
}

#[tracing::instrument(skip(app))]
fn show_menu(app: &AppHandle) {
    let Some(win) = app.get_webview_window(MENU_LABEL) else {
        tracing::error!("menu window missing from manifest");
        return;
    };
    let (x, y) = center_on_primary(app, MENU_WIDTH, MENU_HEIGHT);
    present(&win, x, y);
}

#[tracing::instrument(skip(app))]
fn show_window_picker(app: &AppHandle) {
    if reject_during_recording(app, "window-picker") {
        return;
    }
    let Some(win) = app.get_webview_window(WINDOW_PICKER_LABEL) else {
        tracing::error!("window-picker window missing from manifest");
        return;
    };
    let windows = enumerate_capturable_windows();
    tracing::debug!(count = windows.len(), "window-picker: enumerated");
    let (x, y) = center_on_primary(app, WINDOW_PICKER_WIDTH, WINDOW_PICKER_HEIGHT);
    present(&win, x, y);

    let ids: Vec<i64> = windows.iter().map(|w| w.id).collect();
    if let Err(e) = app.emit_to(WINDOW_PICKER_LABEL, "picker:show", windows) {
        tracing::error!(error = %e, "emit picker:show");
    }
    // Icon extraction is 5-10 ms per window — inline would push picker
    // show-time past the 100 ms perceptual budget. Lazy load: list
    // appears instant with placeholder glyphs, picker:icon events swap
    // in real icons as they land.
    let app_clone = app.clone();
    std::thread::Builder::new()
        .name("clipo-window-icons".into())
        .spawn(move || {
            for id in ids {
                if let Some(data_url) = extract_window_icon(id) {
                    let payload = serde_json::json!({ "id": id, "dataUrl": data_url });
                    if let Err(e) = app_clone.emit_to(WINDOW_PICKER_LABEL, "picker:icon", payload) {
                        tracing::warn!(error = %e, ?id, "emit picker:icon");
                    }
                }
            }
        })
        .ok();
}

fn hide_window(app: &AppHandle, label: &str) {
    if let Some(win) = app.get_webview_window(label)
        && let Err(e) = win.hide()
    {
        tracing::warn!(error = %e, label, "hide");
    }
}

fn center_on_primary(app: &AppHandle, width: f64, height: f64) -> (f64, f64) {
    work_area_at(app, None).map_or((0.0, 0.0), |w| {
        (
            w.left + (w.width() - width) / 2.0,
            w.top + (w.height() - height) / 2.0,
        )
    })
}

fn right_centered_on_primary(
    app: &AppHandle,
    width: f64,
    height: f64,
    right_margin: f64,
) -> (f64, f64) {
    work_area_at(app, None).map_or((0.0, 0.0), |w| {
        (
            w.right - width - right_margin,
            w.top + (w.height() - height) / 2.0,
        )
    })
}

// ─────── settings persistence ───────

fn settings_path() -> Result<PathBuf, String> {
    let base = dirs::data_local_dir().ok_or_else(|| "could not resolve LocalAppData".to_string())?;
    Ok(base.join("Clipo").join(SETTINGS_FILE))
}

fn load_settings() -> SettingsData {
    let Ok(path) = settings_path() else { return SettingsData::default() };
    let Ok(bytes) = std::fs::read(&path) else { return SettingsData::default() };
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        tracing::warn!(error = %e, path = %path.display(), "settings.json malformed; using defaults");
        SettingsData::default()
    })
}

fn save_settings(settings: &SettingsData) -> Result<(), String> {
    let path = settings_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(settings).map_err(|e| format!("serialize settings: {e}"))?;
    std::fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))
}

fn sync_autostart(app: &AppHandle, enabled: bool) {
    let manager = app.autolaunch();
    let already = manager.is_enabled().unwrap_or(false);
    if already == enabled {
        return;
    }
    let result = if enabled { manager.enable() } else { manager.disable() };
    if let Err(e) = result {
        tracing::warn!(error = %e, enabled, "autostart sync");
    }
}

// ─────── Tauri commands ───────

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn take_pending_capture(
    label: String,
    state: tauri::State<'_, PendingCaptures>,
) -> Option<CaptureSavedPayload> {
    state.lock().remove(&label)
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn copy_capture_image(path: String) -> Result<(), String> {
    let captured = decode_to_bgra(Path::new(&path)).map_err(|e| format!("decode: {e}"))?;
    copy_image_to_clipboard(&captured).map_err(|e| format!("clipboard: {e}"))
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn reveal_in_folder(path: String) -> Result<(), String> {
    let arg = format!("/select,\"{path}\"");
    shell_execute("open", "explorer.exe", Some(&arg))
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn open_file(path: String) -> Result<(), String> {
    shell_execute("open", &path, None)
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn get_settings(state: tauri::State<'_, AppSettings>) -> SettingsData {
    state.lock().clone()
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn update_settings(
    app: AppHandle,
    settings: SettingsData,
    state: tauri::State<'_, AppSettings>,
) -> Result<SettingsData, String> {
    let (previous_autostart, previous_language) = {
        let g = state.lock();
        (g.autostart, g.language.clone())
    };
    if settings.autostart != previous_autostart {
        sync_autostart(&app, settings.autostart);
    }
    apply_shortcut_changes(&app, &state, &settings);
    save_settings(&settings)?;
    let language_changed = settings.language != previous_language;
    *state.lock() = settings.clone();
    // Emit AFTER releasing the lock so a listener that calls
    // get_settings from the same surface doesn't deadlock.
    if language_changed
        && let Err(e) = app.emit("settings:language-changed", settings.language.clone())
    {
        tracing::warn!(error = %e, "emit settings:language-changed");
    }
    Ok(settings)
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn reset_settings(
    app: AppHandle,
    state: tauri::State<'_, AppSettings>,
) -> Result<SettingsData, String> {
    update_settings(app, SettingsData::default(), state)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BuildInfo {
    version: &'static str,
    commit: &'static str,
    commit_date: &'static str,
}

#[tauri::command]
const fn get_build_info() -> BuildInfo {
    BuildInfo {
        version: env!("CARGO_PKG_VERSION"),
        commit: match option_env!("CLIPO_GIT_COMMIT") {
            Some(c) => c,
            None => "dev",
        },
        commit_date: match option_env!("CLIPO_COMMIT_DATE") {
            Some(d) => d,
            None => "unknown",
        },
    }
}

fn apply_shortcut_changes(
    app: &AppHandle,
    settings_state: &tauri::State<'_, AppSettings>,
    next: &SettingsData,
) {
    let Some(rt_state) = app.try_state::<ShortcutsState>() else { return };
    let gs = app.global_shortcut();
    let current_settings = settings_state.lock().clone();
    let recording_active = is_recording_active(app);
    let mut runtime = rt_state.lock();
    let mut changed = false;
    for def in HOTKEY_DEFS {
        let prev_combo = resolved_combo(&current_settings, def);
        let next_combo = resolved_combo(next, def);
        if prev_combo == next_combo {
            continue;
        }
        changed = true;
        let scoped = is_recording_scoped(def.id);
        let was_live = !scoped || recording_active;
        // Drop the old registration before trying the new one — OS
        // treats them as distinct slots, not a replace. Skip when the
        // previous slot was never registered (scoped + no recording).
        if was_live
            && let Some(prev_sc) = runtime.bindings.get(def.id).copied()
            && let Err(e) = gs.unregister(prev_sc)
        {
            tracing::warn!(id = def.id, error = %e, "unregister old hotkey");
        }
        let Ok(new_sc) = next_combo.parse::<Shortcut>() else {
            runtime.bindings.remove(def.id);
            runtime.status.insert(def.id.to_string(), "invalid");
            continue;
        };
        let outcome = if scoped && !recording_active {
            // Stored but deliberately not registered — next
            // register_recording_shortcuts picks it up.
            "inactive"
        } else {
            match gs.register(new_sc) {
                Ok(()) => "active",
                Err(e) => {
                    tracing::warn!(id = def.id, combo = next_combo, error = %e, "register new hotkey");
                    "conflict"
                }
            }
        };
        runtime.bindings.insert(def.id.to_string(), new_sc);
        runtime.status.insert(def.id.to_string(), outcome);
    }
    drop(runtime);
    if changed
        && let Err(e) = app.emit("shortcuts:updated", ())
    {
        tracing::warn!(error = %e, "emit shortcuts:updated");
    }
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn get_shortcut_status(app: AppHandle) -> HashMap<String, &'static str> {
    app.try_state::<ShortcutsState>()
        .map(|s| s.lock().status.clone())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct HotkeyInfo {
    id: &'static str,
    label: &'static str,
    default_combo: &'static str,
}

#[tauri::command]
fn list_hotkey_defs() -> Vec<HotkeyInfo> {
    HOTKEY_DEFS
        .iter()
        .map(|d| HotkeyInfo {
            id: d.id,
            label: d.label,
            default_combo: d.default_combo,
        })
        .collect()
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn get_active_shortcuts(app: AppHandle) -> HashMap<String, String> {
    let settings = app
        .try_state::<AppSettings>()
        .map(|s| s.lock().clone())
        .unwrap_or_default();
    HOTKEY_DEFS
        .iter()
        .map(|d| (d.id.to_string(), resolved_combo(&settings, d).to_string()))
        .collect()
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn list_captures(app: AppHandle) -> Result<Vec<CaptureEntry>, String> {
    let folder = captures_dir(&app)?;
    let read = match std::fs::read_dir(&folder) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read_dir {}: {e}", folder.display())),
    };

    let mut entries: Vec<CaptureEntry> = read
        .filter_map(Result::ok)
        .filter_map(|de| {
            let path = de.path();
            let kind = match path
                .extension()
                .and_then(|s| s.to_str())
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("png" | "jpg" | "jpeg" | "webp") => "image",
                Some("gif") => "gif",
                Some("mp4") => "video",
                _ => return None,
            };
            let meta = de.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            // Legacy `<stem>.thumb.jpg` cleanup. Thumbnails moved to
            // %LOCALAPPDATA%\Clipo\thumbs\; any sidecar JPEG still in
            // Pictures is a leftover from the old layout — self-clean
            // during the scan.
            if kind == "image"
                && Path::new(path.file_stem().unwrap_or_default())
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("thumb"))
            {
                let _ = std::fs::remove_file(&path);
                return None;
            }
            let modified_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|d| u64::try_from(d.as_millis()).ok())
                .unwrap_or(0);
            // Surfaces sidecar JPEG path if present — grid renders
            // 15 KB thumb instead of decoding 4K source into VRAM.
            let thumbnail_path = thumb_cache_path(&path)
                .filter(|p| p.exists())
                .map(|p| p.to_string_lossy().into_owned());
            Some(CaptureEntry {
                path: path.to_string_lossy().into_owned(),
                filename: path.file_name()?.to_string_lossy().into_owned(),
                size_bytes: meta.len(),
                modified_ms,
                kind,
                thumbnail_path,
            })
        })
        .collect();

    entries.sort_by_key(|b| std::cmp::Reverse(b.modified_ms));
    Ok(entries)
}

/// Cache path in `%LOCALAPPDATA%\Clipo\thumbs\<full source filename>.jpg`.
/// Including the source extension (e.g. `…-091145.mp4.jpg`,
/// `…-091145.png.jpg`) avoids stem-collision between a PNG capture
/// and an MP4 recording taken in the same second.
fn thumb_cache_path(media: &Path) -> Option<PathBuf> {
    let filename = media.file_name()?;
    let mut name = filename.to_owned();
    name.push(".jpg");
    let base = dirs::data_local_dir()?;
    Some(base.join("Clipo").join("thumbs").join(name))
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn delete_capture(app: AppHandle, path: String) -> Result<(), String> {
    let path_ref = Path::new(&path);
    std::fs::remove_file(path_ref).map_err(|e| format!("remove {path}: {e}"))?;
    // remove_file swallows NotFound — older captures without thumbs are fine.
    if let Some(cache) = thumb_cache_path(path_ref) {
        let _ = std::fs::remove_file(cache);
    }
    if let Err(e) = app.emit("capture:deleted", &path) {
        tracing::warn!(error = %e, "emit capture:deleted");
    }
    Ok(())
}

/// Convert MP4 to sibling .gif. Single-pass filter chain: `split`
/// duplicates the stream so palettegen + paletteuse run against the
/// same frames (two-pass quality, one process). 15 fps + 720 px sweet
/// spot for sharing — under ~5 MB for ~10 s clips while smooth for
/// screen content. `bayer` dither beats default `sierra2_4a` for
/// synthetic UI graphics (no false colour banding).
#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
async fn export_to_gif(app: AppHandle, path: String) -> Result<String, String> {
    let ffmpeg = clipo_capture::locate_ffmpeg().ok_or_else(|| "ffmpeg not found".to_owned())?;
    let src = PathBuf::from(&path);
    let dst = src.with_extension("gif");
    let dst_str = dst.to_string_lossy().to_string();

    let app_emit = app.clone();
    let dst_for_thread = dst.clone();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let filter = "fps=15,scale=720:-1:flags=lanczos,split[s0][s1];\
                      [s0]palettegen=max_colors=128[p];\
                      [s1][p]paletteuse=dither=bayer";
        let status = std::process::Command::new(&ffmpeg)
            .arg("-y")
            .arg("-i")
            .arg(&src)
            .arg("-vf")
            .arg(filter)
            .arg(&dst_for_thread)
            .status()
            .map_err(|e| format!("spawn ffmpeg: {e}"))?;
        if !status.success() {
            return Err(format!("ffmpeg exited with {status}"));
        }
        if let Err(e) = app_emit.emit("capture:saved", ()) {
            tracing::warn!(error = %e, "emit capture:saved (gif)");
        }
        tracing::info!(src = %src.display(), dst = %dst_for_thread.display(), "gif: exported");
        Ok(())
    })
    .await
    .map_err(|e| format!("gif join: {e}"))??;

    Ok(dst_str)
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
async fn ensure_video_thumbnail(path: String) -> Result<String, String> {
    let mp4 = PathBuf::from(&path);
    let sidecar = thumb_cache_path(&mp4).ok_or_else(|| format!("invalid video path: {path}"))?;
    if sidecar.exists() {
        return Ok(sidecar.to_string_lossy().into_owned());
    }
    let sidecar_for_worker = sidecar.clone();
    tauri::async_runtime::spawn_blocking(move || {
        extract_video_thumbnail(&mp4, &sidecar_for_worker).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("ensure_video_thumbnail join: {e}"))??;
    Ok(sidecar.to_string_lossy().into_owned())
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
async fn ensure_image_thumbnail(path: String) -> Result<String, String> {
    let image_path = PathBuf::from(&path);
    let sidecar = thumb_cache_path(&image_path).ok_or_else(|| format!("invalid image path: {path}"))?;
    if sidecar.exists() {
        return Ok(sidecar.to_string_lossy().into_owned());
    }
    let sidecar_for_worker = sidecar.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let img = decode_to_bgra(&image_path).map_err(|e| e.to_string())?;
        save_thumbnail_jpeg(&img, &sidecar_for_worker).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("ensure_image_thumbnail join: {e}"))??;
    Ok(sidecar.to_string_lossy().into_owned())
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn timer_complete(app: AppHandle) {
    hide_window(&app, TIMER_LABEL);
    // mem::take resets target to Photo so a future ESC-then-tray-Capture
    // flow doesn't fire whatever the last recording attempt asked for.
    let target = app
        .try_state::<TimerTargetState>()
        .map(|s| std::mem::take(&mut *s.lock()))
        .unwrap_or_default();
    match target {
        TimerTarget::Photo => spawn_fullscreen_capture(&app),
        TimerTarget::Recording(rect) => start_region_recording_now(&app, rect),
    }
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn menu_pick(app: AppHandle, action: String) {
    hide_window(&app, MENU_LABEL);
    match action.as_str() {
        "region" => {
            if reject_during_recording(&app, "menu region") { return; }
            if let Some(overlay) = app.try_state::<Overlay>() {
                overlay.toggle(show_magnifier_pref(&app));
            }
        }
        "fullscreen" => {
            if reject_during_recording(&app, "menu fullscreen") { return; }
            spawn_fullscreen_capture(&app);
        }
        "record-fullscreen" => toggle_fullscreen_recording(&app),
        "timer" => {
            if reject_during_recording(&app, "menu timer") { return; }
            show_timer(&app);
        }
        "ocr" => {
            if reject_during_recording(&app, "menu ocr") { return; }
            start_ocr_capture(&app);
        }
        "window" => show_window_picker(&app),
        "history" => show_chrome_window(&app, HISTORY_LABEL),
        "settings" => show_chrome_window(&app, SETTINGS_LABEL),
        other => tracing::warn!(action = other, "menu_pick: unknown action"),
    }
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn ocr_extract(app: AppHandle, path: String) {
    start_ocr_for_path(&app, path, None);
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn take_ocr_result(
    label: String,
    state: tauri::State<'_, OcrResults>,
) -> Option<Result<OcrText, String>> {
    state.lock().remove(&label)
}

/// `navigator.clipboard.writeText` from localhost-origin WebView2
/// triggers a "site wants to write to clipboard" prompt — bypassing
/// JS entirely with Win32 avoids the prompt.
#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn copy_text_to_clipboard(text: String) -> Result<(), String> {
    write_text_to_clipboard(&text)
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
async fn upload_capture(app: AppHandle, path: String) -> Result<String, String> {
    let service = app
        .try_state::<AppSettings>()
        .map(|s| s.lock().upload_service)
        .unwrap_or_default();
    let filename = Path::new(&path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("capture.png")
        .to_string();
    let bytes = std::fs::read(&path).map_err(|e| format!("read {path}: {e}"))?;
    let url = tauri::async_runtime::spawn_blocking(move || {
        upload::upload(service, &bytes, &filename).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| format!("upload join: {e}"))??;
    // Best-effort clipboard copy — never blocks upload success, just
    // logs Win32 OpenClipboard contention.
    if let Err(e) = write_text_to_clipboard(&url) {
        tracing::warn!(error = %e, "upload url clipboard");
    }
    // Persist path → URL via tauri-plugin-store: handles JSON
    // persistence, debounced writes, cross-window change notifications.
    match app.store(UPLOADS_STORE) {
        Ok(store) => {
            store.set(path.clone(), serde_json::Value::String(url.clone()));
            if let Err(e) = store.save() {
                tracing::warn!(error = %e, "store save uploads");
            }
        }
        Err(e) => tracing::warn!(error = %e, "open uploads store"),
    }
    Ok(url)
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn write_text_file(path: String, contents: String) -> Result<(), String> {
    std::fs::write(&path, contents).map_err(|e| format!("write {path}: {e}"))
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn open_editor(app: AppHandle, path: String) {
    if let Some(state) = app.try_state::<PendingEditorSources>() {
        state.lock().insert(EDITOR_LABEL.to_string(), path.clone());
    }
    show_chrome_window(&app, EDITOR_LABEL);
    if let Err(e) = app.emit_to(EDITOR_LABEL, "editor:open", &path) {
        tracing::warn!(error = %e, "emit editor:open");
    }
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn take_editor_source(
    label: String,
    state: tauri::State<'_, PendingEditorSources>,
) -> Option<String> {
    state.lock().remove(&label)
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn save_annotated(app: AppHandle, path: String, bytes: Vec<u8>) -> Result<(), String> {
    std::fs::write(&path, bytes).map_err(|e| format!("write {path}: {e}"))?;
    if let Err(e) = app.emit("capture:saved", &path) {
        tracing::warn!(error = %e, "emit capture:saved (edited)");
    }
    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn copy_annotated_to_clipboard(bytes: Vec<u8>) -> Result<(), String> {
    let image = decode_bgra_from_bytes(&bytes).map_err(|e| format!("decode: {e}"))?;
    copy_image_to_clipboard(&image).map_err(|e| format!("clipboard: {e}"))
}

/// Win32 CF_UNICODETEXT write. Shared helper for future "copy filename"
/// or similar commands so the wide-string + clipboard-lock dance
/// stays in one place.
fn write_text_to_clipboard(text: &str) -> Result<(), String> {
    use windows::Win32::Foundation::{HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};

    const CF_UNICODETEXT: u32 = 13;

    let utf16: Vec<u16> = OsStr::new(text).encode_wide().chain([0]).collect();
    let bytes = std::mem::size_of_val(utf16.as_slice());

    // SAFETY: HGLOBAL allocation; we copy into it and hand it to the
    // clipboard which takes ownership on SetClipboardData success.
    let handle: HGLOBAL =
        unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) }.map_err(|e| format!("GlobalAlloc: {e}"))?;
    let ptr = unsafe { GlobalLock(handle) };
    if ptr.is_null() {
        return Err("GlobalLock returned null".into());
    }
    // SAFETY: handle just allocated to fit `bytes`.
    unsafe {
        std::ptr::copy_nonoverlapping(utf16.as_ptr().cast::<u8>(), ptr.cast::<u8>(), bytes);
        let _ = GlobalUnlock(handle);
    }

    // SAFETY: OpenClipboard / SetClipboardData / CloseClipboard paired.
    unsafe { OpenClipboard(None) }.map_err(|e| format!("OpenClipboard: {e}"))?;
    let commit = (|| {
        unsafe { EmptyClipboard() }.map_err(|e| format!("EmptyClipboard: {e}"))?;
        unsafe { SetClipboardData(CF_UNICODETEXT, Some(HANDLE(handle.0))) }
            .map(|_| ())
            .map_err(|e| format!("SetClipboardData: {e}"))
    })();
    let _ = unsafe { CloseClipboard() };
    commit
}

// ─────── path resolution ───────

/// Single source of truth for the screenshot format (PNG / JPEG) so
/// every capture flow (region, OCR) stays in sync with `image_format`.
fn save_capture_image(app: &AppHandle, image: &CapturedImage) -> Result<PathBuf, String> {
    let jpeg = app
        .try_state::<AppSettings>()
        .is_some_and(|s| s.lock().image_format == "jpg");
    let ext = if jpeg { "jpg" } else { "png" };
    let path = resolve_capture_path_with_ext(app, ext)?;
    if jpeg {
        save_jpeg(image, &path, CAPTURE_JPEG_QUALITY).map_err(|e| e.to_string())?;
    } else {
        save_png(image, &path).map_err(|e| e.to_string())?;
    }
    Ok(path)
}

fn resolve_capture_path_with_ext(app: &AppHandle, ext: &str) -> Result<PathBuf, String> {
    // SAFETY: GetLocalTime writes into SYSTEMTIME we pass.
    let st = unsafe { GetLocalTime() };
    let file = format!(
        "clipo-{:04}{:02}{:02}-{:02}{:02}{:02}.{ext}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond,
    );
    let folder = captures_dir(app)?;
    std::fs::create_dir_all(&folder).map_err(|e| format!("create captures dir: {e}"))?;
    Ok(folder.join(file))
}

fn captures_dir(app: &AppHandle) -> Result<PathBuf, String> {
    if let Some(state) = app.try_state::<AppSettings>() {
        let custom = state.lock().capture_folder.clone();
        if let Some(p) = custom.filter(|s| !s.is_empty()) {
            return Ok(PathBuf::from(p));
        }
    }
    let pictures = dirs::picture_dir()
        .ok_or_else(|| "could not resolve user's Pictures folder".to_string())?;
    Ok(pictures.join("Clipo"))
}

fn open_captures_folder(app: &AppHandle) {
    let Ok(folder) = captures_dir(app) else { return };
    let _ = std::fs::create_dir_all(&folder);
    if let Err(e) = shell_execute("open", &folder.to_string_lossy(), None) {
        tracing::warn!(error = %e, "open captures folder");
    }
}

/// Wrapper over ShellExecuteW. Win32 contract: HINSTANCE > 32 = success.
fn shell_execute(verb: &str, file: &str, params: Option<&str>) -> Result<(), String> {
    let verb_w: Vec<u16> = OsStr::new(verb).encode_wide().chain([0]).collect();
    let file_w: Vec<u16> = OsStr::new(file).encode_wide().chain([0]).collect();
    let params_w: Option<Vec<u16>> = params.map(|p| OsStr::new(p).encode_wide().chain([0]).collect());
    let params_pcwstr = params_w.as_ref().map_or(PCWSTR::null(), |v| PCWSTR(v.as_ptr()));
    // SAFETY: nul-terminated UTF-16 buffers live until call returns.
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb_w.as_ptr()),
            PCWSTR(file_w.as_ptr()),
            params_pcwstr,
            None,
            SW_SHOWNORMAL,
        )
    };
    if (result.0 as isize) <= 32 {
        Err(format!("ShellExecuteW({file}) returned {}", result.0 as isize))
    } else {
        Ok(())
    }
}

// ─────── tray icon + menu ───────

struct IconVariants {
    default: tauri::image::Image<'static>,
    recording: tauri::image::Image<'static>,
}

/// Composite a red recording dot onto the icon's bottom-right corner.
/// Dot radius scales with the icon's shorter side so it stays visible
/// at any size (Windows picks 16/32/256 at render time). Inner red
/// disk + 1-px white ring for contrast on dark + light tray themes.
#[allow(clippy::cast_possible_wrap)]
fn generate_recording_icon_variant(default: &tauri::image::Image<'_>) -> tauri::image::Image<'static> {
    let width = default.width();
    let height = default.height();
    let mut rgba = default.rgba().to_vec();
    let dot_radius = i32::try_from((width.min(height) / 5).max(3)).unwrap_or(3);
    let margin = (dot_radius / 3).max(1);
    let width_i = i32::try_from(width).unwrap_or(i32::MAX);
    let height_i = i32::try_from(height).unwrap_or(i32::MAX);
    let center_x = width_i - dot_radius - margin;
    let center_y = height_i - dot_radius - margin;
    let outer_r_sq = dot_radius * dot_radius;
    let inner_r = (dot_radius - 1).max(1);
    let inner_r_sq = inner_r * inner_r;
    for y in 0..height_i {
        for x in 0..width_i {
            let dx = x - center_x;
            let dy = y - center_y;
            let dist_sq = dx * dx + dy * dy;
            if dist_sq > outer_r_sq {
                continue;
            }
            #[allow(clippy::cast_sign_loss)]
            let idx = ((y as u32) * width + (x as u32)) as usize * 4;
            // Inner: Material Red A400 #FF1744 — matches `--color-danger`
            // from app.css so the tray's recording dot reads as the
            // same red as the rest of the recording UI (bar, overlay).
            // Outer ring: white for contrast on dark trays.
            let (r, g, b) = if dist_sq <= inner_r_sq {
                (255, 23, 68)
            } else {
                (255, 255, 255)
            };
            rgba[idx] = r;
            rgba[idx + 1] = g;
            rgba[idx + 2] = b;
            rgba[idx + 3] = 255;
        }
    }
    tauri::image::Image::new_owned(rgba, width, height)
}

/// Live tray tooltip with recording elapsed time. Plain std::thread
/// (not tokio) because the work is pure blocking (sleep + lock +
/// Shell_NotifyIcon); pinning a worker avoids the runtime dep AND
/// waking the reactor every second. Sleeping thread ~8 KB stack and
/// 0 CPU. Exits when ActiveRecording clears (stop/discard/restart).
fn spawn_tray_tooltip_timer(app: &AppHandle) {
    let app = app.clone();
    std::thread::Builder::new()
        .name("clipo-tray-tooltip".to_owned())
        .spawn(move || {
            // Swap to recording icon at start so tray shows "active"
            // before the first second tick.
            if let Some(tray) = app.tray_by_id(TRAY_ICON_ID)
                && let Some(variants) = app.try_state::<IconVariants>()
                && let Err(e) = tray.set_icon(Some(variants.recording.clone()))
            {
                tracing::warn!(error = %e, "tray set_icon (recording)");
            }
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                // mutate_recorder returns None when no recording active
                // — exit signal (stop/discard/restart clear ActiveRecording).
                let Some(elapsed) = mutate_recorder(&app, VideoRecorder::elapsed_secs) else { break };
                let mins = elapsed / 60;
                let secs = elapsed % 60;
                let text = format!("Recording {mins:02}:{secs:02}");
                if let Some(tray) = app.tray_by_id(TRAY_ICON_ID)
                    && let Err(e) = tray.set_tooltip(Some(&text))
                {
                    tracing::warn!(error = %e, "tray set_tooltip");
                }
            }
            // Restore default icon + tooltip together so the tray
            // returns to idle in one frame, not two.
            if let Some(tray) = app.tray_by_id(TRAY_ICON_ID) {
                if let Some(variants) = app.try_state::<IconVariants>()
                    && let Err(e) = tray.set_icon(Some(variants.default.clone()))
                {
                    tracing::warn!(error = %e, "tray set_icon (default)");
                }
                if let Err(e) = tray.set_tooltip(Some(TRAY_DEFAULT_TOOLTIP)) {
                    tracing::warn!(error = %e, "tray reset tooltip");
                }
            }
        })
        .map_or_else(
            |e| tracing::warn!(error = %e, "spawn tray tooltip thread"),
            drop,
        );
}

fn perform_tray_left_click(app: &AppHandle) {
    let action = app
        .try_state::<AppSettings>()
        .map(|s| s.lock().tray_left_click.clone())
        .unwrap_or_default();
    match action.as_str() {
        "fullscreen" => spawn_fullscreen_capture(app),
        "menu" => show_menu(app),
        "timer" => show_timer(app),
        "ocr" => start_ocr_capture(app),
        // "region" + any unrecognized
        _ => {
            if let Some(overlay) = app.try_state::<Overlay>() {
                overlay.toggle(show_magnifier_pref(app));
            }
        }
    }
}

#[tracing::instrument(skip(app))]
fn build_tray(app: &AppHandle) -> tauri::Result<()> {
    TrayIconBuilder::with_id(TRAY_ICON_ID)
        .tooltip(TRAY_DEFAULT_TOOLTIP)
        .icon(app.default_window_icon().cloned().unwrap_or_else(|| {
            tauri::image::Image::new_owned(vec![0; 16 * 16 * 4], 16, 16)
        }))
        .on_tray_icon_event(|tray, event| {
            // Left = primary action (capture region), right = menu —
            // matches Greenshot / Lightshot convention. During a
            // recording the overlay can't open (DXGI per-output
            // exclusive); fall through to menu so the click never
            // silently no-ops.
            if let TrayIconEvent::Click {
                button_state: MouseButtonState::Up,
                button,
                position,
                ..
            } = event
            {
                let app = tray.app_handle();
                if button == MouseButton::Left && !is_recording_active(app) {
                    perform_tray_left_click(app);
                } else {
                    show_tray_menu(app, position.x, position.y);
                }
            }
        })
        .build(app)?;
    Ok(())
}

/// Pre-paint the tray-menu webview so first user-triggered open is
/// instant. Cost is paid at startup — invisible for a daemon that runs
/// all day. Off-screen render exercises the WebView pipeline; OS never
/// composites the visible pixels.
fn prewarm_tray_menu(app: &AppHandle) {
    let Some(win) = app.get_webview_window(TRAY_MENU_LABEL) else {
        tracing::warn!("tray-menu window missing — skip prewarm");
        return;
    };
    disable_dwm_transitions(&win);
    if let Err(e) = win.set_position(LogicalPosition::new(-30000.0, -30000.0)) {
        tracing::warn!(error = %e, "prewarm set_position");
    }
    if let Err(e) = win.show() {
        tracing::warn!(error = %e, "prewarm show");
        return;
    }
    if let Err(e) = win.hide() {
        tracing::warn!(error = %e, "prewarm hide");
    }
}

/// Tell DWM to skip its open/close/minimise/maximise animations. Same
/// flag the Windows shell sets on its own popup menus. Without this,
/// each hide() triggers a 100-200 ms ghost fade that reads as lag.
fn disable_dwm_transitions(win: &WebviewWindow) {
    let Ok(tauri_hwnd) = win.hwnd() else {
        tracing::warn!(label = win.label(), "hwnd unavailable for DWM transitions");
        return;
    };
    // Tauri re-exports windows-0.61's HWND; workspace pins 0.59. Both
    // are `pub struct HWND(*mut c_void)` — bridge via raw pointer.
    let hwnd = HWND(tauri_hwnd.0.cast());
    let value = TRUE;
    // SAFETY: BOOL by-pointer with size; HWND owned by Tauri outlives
    // this synchronous call.
    let res = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            std::ptr::from_ref(&value).cast(),
            4u32,
        )
    };
    if let Err(e) = res {
        tracing::warn!(error = %e, label = win.label(), "DWMWA_TRANSITIONS_FORCEDISABLED");
    }
}

/// Natural height (logical px) for `items` rows + `dividers`. Mirrors
/// the flex column in TrayMenuPage.svelte — keeps the window flush
/// with content so neither layout (default / recording) shows a dead
/// strip under the last item.
fn tray_menu_height(items: u32, dividers: u32) -> f64 {
    let gaps = (items + dividers).saturating_sub(1);
    f64::from(
        items * TRAY_MENU_ROW_PX
            + dividers * TRAY_MENU_DIVIDER_PX
            + gaps * TRAY_MENU_GAP_PX
            + 2 * TRAY_MENU_PAD_PX,
    )
}

fn show_tray_menu(app: &AppHandle, cursor_x: f64, cursor_y: f64) {
    let Some(win) = app.get_webview_window(TRAY_MENU_LABEL) else {
        tracing::error!("tray-menu window missing from manifest");
        return;
    };
    // Pick the cursor's monitor (not primary) — taskbar on secondary
    // display would otherwise hand back wrong work area / scale.
    let Some(work) = work_area_at(app, Some((cursor_x, cursor_y))) else {
        tracing::error!("no monitor for tray menu");
        return;
    };
    let scale = app
        .monitor_from_point(cursor_x, cursor_y)
        .ok()
        .flatten()
        .or_else(|| app.primary_monitor().ok().flatten())
        .map_or(1.0, |m| m.scale_factor());
    let cursor_logical_x = cursor_x / scale;
    let cursor_logical_y = cursor_y / scale;

    // Pick the layout the Svelte page is about to render — recording
    // grows/shrinks by mute branches; default height would leave a
    // black gap.
    let height = app
        .try_state::<ActiveRecording>()
        .and_then(|state| {
            state
                .lock()
                .as_ref()
                .map(|s| u32::from(s.capture_audio) + u32::from(s.capture_mic))
        })
        .map_or_else(
            // Default: 13 items + 4 dividers (region, fullscreen, window,
            // timer, ocr | record-fs, record-region | menu, quick |
            // history, open-folder, settings | quit).
            || tray_menu_height(13, 4),
            // Recording: 8 base (Stop/Pause/Restart/Discard/History/
            // Open/Settings/Quit) + 2 dividers. Each enabled branch
            // (system, mic) adds a mute row; first one also adds a divider.
            |mutes| tray_menu_height(8 + mutes, 2 + u32::from(mutes > 0)),
        );
    if let Err(e) = win.set_size(tauri::LogicalSize::new(TRAY_MENU_WIDTH, height)) {
        tracing::warn!(error = %e, "tray-menu set_size");
    }

    let mut x = cursor_logical_x - TRAY_MENU_WIDTH / 2.0;
    x = x.clamp(work.left + TRAY_MENU_MARGIN, work.right - TRAY_MENU_WIDTH - TRAY_MENU_MARGIN);
    // Prefer ABOVE the cursor (tray sits at screen bottom); fall back
    // below if not enough room.
    let above_y = cursor_logical_y - height - TRAY_MENU_MARGIN;
    let y = if above_y >= work.top + TRAY_MENU_MARGIN {
        above_y
    } else {
        (cursor_logical_y + TRAY_MENU_MARGIN).min(work.bottom - height - TRAY_MENU_MARGIN)
    };

    if let Err(e) = win.set_position(LogicalPosition::new(x, y)) {
        tracing::warn!(error = %e, "tray-menu set_position");
    }
    if let Err(e) = win.show() {
        tracing::error!(error = %e, "tray-menu show");
    }
    if let Err(e) = win.set_focus() {
        tracing::warn!(error = %e, "tray-menu set_focus");
    }
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn dismiss_tray_menu(app: AppHandle) {
    hide_window(&app, TRAY_MENU_LABEL);
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn close_window_picker(app: AppHandle) {
    hide_window(&app, WINDOW_PICKER_LABEL);
}

/// Focus the chosen window and capture its frame. Ordering matters:
/// 1. focus_window_and_bounds runs while picker still owns foreground
///    (SetForegroundWindow is permitted from foreground process).
/// 2. Hide picker — removes shadow + bar from upcoming frame.
/// 3. Clamp to single-monitor rect (DXGI output_containing requires it).
/// 4. Settle delay + capture on blocking pool; WINDOW_PICKER_SETTLE_MS
///    is what the compositor needs to commit picker takedown + target
///    promotion.
#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn capture_window(app: AppHandle, id: i64) {
    if reject_during_recording(&app, "capture_window") {
        hide_window(&app, WINDOW_PICKER_LABEL);
        return;
    }
    let Some(rect) = focus_window_and_bounds(id) else {
        tracing::warn!(?id, "window-picker: target HWND gone or no DWM bounds");
        hide_window(&app, WINDOW_PICKER_LABEL);
        return;
    };
    hide_window(&app, WINDOW_PICKER_LABEL);
    let Some(clamped) = clamp_rect_to_monitor(&app, rect) else {
        tracing::warn!(?rect, "window-picker: rect's centre is on no known monitor");
        return;
    };
    let Some(capture) = app.try_state::<CaptureHandle>().map(|h| (*h).clone()) else {
        tracing::error!("capture handle missing");
        return;
    };
    tauri::async_runtime::spawn_blocking(move || {
        std::thread::sleep(std::time::Duration::from_millis(WINDOW_PICKER_SETTLE_MS));
        run_region_capture(&app, &capture, clamped);
    });
}

fn clamp_rect_to_monitor(app: &AppHandle, rect: Rect) -> Option<Rect> {
    let monitors: Vec<Rect> = app
        .available_monitors()
        .ok()?
        .into_iter()
        .map(|m| {
            let p = m.position();
            let s = m.size();
            Rect {
                x: p.x,
                y: p.y,
                width: s.width,
                height: s.height,
            }
        })
        .collect();
    clamp_rect_to_monitors(&monitors, rect)
}

/// Pure half of `clamp_rect_to_monitor`. Pick the monitor containing
/// the rect's centre and intersect — same way Snipping Tool / CleanShot
/// / ShareX resolve a window dragged half-way across the bezel.
#[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
fn clamp_rect_to_monitors(monitors: &[Rect], rect: Rect) -> Option<Rect> {
    let cx = rect.x + (rect.width as i32) / 2;
    let cy = rect.y + (rect.height as i32) / 2;
    let monitor = monitors.iter().find(|m| {
        let mw = m.width as i32;
        let mh = m.height as i32;
        cx >= m.x && cx < m.x + mw && cy >= m.y && cy < m.y + mh
    })?;
    let mw = monitor.width as i32;
    let mh = monitor.height as i32;
    let left = rect.x.max(monitor.x);
    let top = rect.y.max(monitor.y);
    let right = (rect.x + rect.width as i32).min(monitor.x + mw);
    let bottom = (rect.y + rect.height as i32).min(monitor.y + mh);
    let width = (right - left).max(0) as u32;
    let height = (bottom - top).max(0) as u32;
    if width == 0 || height == 0 {
        return None;
    }
    Some(Rect {
        x: left,
        y: top,
        width,
        height,
    })
}

#[allow(clippy::needless_pass_by_value)]
#[tauri::command]
fn tray_menu_pick(app: AppHandle, action: String) {
    hide_window(&app, TRAY_MENU_LABEL);
    match action.as_str() {
        "recording-stop" => stop_recording(app),
        "recording-pause" => pause_recording(app),
        "recording-resume" => resume_recording(app),
        "recording-restart" => restart_recording(app),
        "recording-discard" => discard_recording(app),
        // Toggle actions: read current flag via snapshot, invert, route
        // through standard set_* — single state-change event fan-out
        // path, every surface stays in sync.
        "recording-toggle-audio-mute" => {
            if let Some(s) = snapshot_recording_state(&app) {
                set_audio_muted(app, !s.audio_muted);
            }
        }
        "recording-toggle-mic-mute" => {
            if let Some(s) = snapshot_recording_state(&app) {
                set_mic_muted(app, !s.mic_muted);
            }
        }
        "capture-region" => {
            if reject_during_recording(&app, "tray region") { return; }
            if let Some(overlay) = app.try_state::<Overlay>() {
                overlay.toggle(show_magnifier_pref(&app));
            }
        }
        "capture-fullscreen" => {
            if reject_during_recording(&app, "tray fullscreen") { return; }
            spawn_fullscreen_capture(&app);
        }
        "capture-window" => show_window_picker(&app),
        "capture-record" => start_recording_capture(&app),
        "capture-record-fullscreen" => toggle_fullscreen_recording(&app),
        "capture-timer" => {
            if reject_during_recording(&app, "tray timer") { return; }
            show_timer(&app);
        }
        "capture-ocr" => {
            if reject_during_recording(&app, "tray ocr") { return; }
            start_ocr_capture(&app);
        }
        "menu" => show_menu(&app),
        "quick" => show_quick_access(&app),
        "history" => show_chrome_window(&app, HISTORY_LABEL),
        "open-folder" => open_captures_folder(&app),
        "settings" => show_chrome_window(&app, SETTINGS_LABEL),
        "quit" => {
            tracing::info!("tray quit");
            app.exit(0);
        }
        other => tracing::warn!(item = other, "unhandled tray menu item"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use clipo_core::Rect;
    use tauri_plugin_global_shortcut::Shortcut;

    use super::{
        HOTKEY_DEFS, RECORDING_BITRATE_CAP_BPS, RECORDING_BITRATE_FLOOR_BPS, SettingsData,
        clamp_rect_to_monitors, compute_recording_bitrate, is_recording_scoped,
        pick_recording_bar_position, tray_menu_height,
    };

    #[test]
    fn hotkey_def_ids_are_unique() {
        let mut seen = HashSet::new();
        for def in HOTKEY_DEFS {
            assert!(seen.insert(def.id), "duplicate hotkey id: {}", def.id);
        }
    }

    #[test]
    fn hotkey_def_defaults_all_parse_as_shortcuts() {
        for def in HOTKEY_DEFS {
            assert!(
                def.default_combo.parse::<Shortcut>().is_ok(),
                "default_combo {:?} for id {:?} did not parse",
                def.default_combo,
                def.id,
            );
        }
    }

    #[test]
    fn recording_scope_matches_recording_prefix_exactly() {
        let mut scoped = 0;
        let mut unscoped = 0;
        for def in HOTKEY_DEFS {
            let flag = is_recording_scoped(def.id);
            let has_prefix = def.id.starts_with("recording-");
            assert_eq!(
                flag, has_prefix,
                "scope flag != recording- prefix for {}: flag={}, prefix={}",
                def.id, flag, has_prefix,
            );
            if flag { scoped += 1 } else { unscoped += 1 }
        }
        assert_eq!(scoped, 5);
        assert_eq!(unscoped, 7);
    }

    #[test]
    fn bitrate_1080p_30fps_within_band() {
        assert_eq!(compute_recording_bitrate(1920, 1080, 30), 6_220_800);
    }

    #[test]
    fn bitrate_60fps_exceeds_30fps_via_shoulder() {
        let b30 = compute_recording_bitrate(1920, 1080, 30);
        let b60 = compute_recording_bitrate(1920, 1080, 60);
        assert!(b60 > b30, "60fps ({b60}) should exceed 30fps ({b30})");
    }

    #[test]
    fn bitrate_clamps_to_floor_for_tiny_rect() {
        assert_eq!(compute_recording_bitrate(100, 100, 30), RECORDING_BITRATE_FLOOR_BPS);
    }

    #[test]
    fn bitrate_clamps_to_cap_for_8k() {
        assert_eq!(compute_recording_bitrate(7680, 4320, 60), RECORDING_BITRATE_CAP_BPS);
    }

    const WORK: (f64, f64, f64, f64) = (0.0, 0.0, 1920.0, 1080.0);

    fn at(actual: f64, expected: f64) {
        assert!((actual - expected).abs() < 1e-6, "got {actual}, want {expected}");
    }

    #[test]
    fn bar_sits_below_rect_when_there_is_room() {
        let (x, y) = pick_recording_bar_position(100.0, 100.0, 300.0, 200.0, WORK.0, WORK.1, WORK.2, WORK.3)
            .expect("room below the rect");
        at(y, 208.0);
        at(x, 40.0);
    }

    #[test]
    fn bar_flips_above_when_no_room_below() {
        let (_, y) = pick_recording_bar_position(100.0, 1000.0, 300.0, 1050.0, WORK.0, WORK.1, WORK.2, WORK.3)
            .expect("room above the rect");
        at(y, 948.0);
    }

    #[test]
    fn bar_goes_right_when_no_vertical_room() {
        let (x, _) = pick_recording_bar_position(100.0, 0.0, 300.0, 1080.0, WORK.0, WORK.1, WORK.2, WORK.3)
            .expect("room right of the rect");
        at(x, 308.0);
    }

    #[test]
    fn bar_hidden_when_fullscreen() {
        assert!(
            pick_recording_bar_position(0.0, 0.0, 1920.0, 1080.0, WORK.0, WORK.1, WORK.2, WORK.3)
                .is_none()
        );
    }

    #[test]
    fn tray_height_default_matches_thirteen_items() {
        at(tray_menu_height(13, 4), 488.0);
    }

    #[test]
    fn tray_height_recording_layouts() {
        at(tray_menu_height(8, 2), 299.0);
        at(tray_menu_height(9, 3), 344.0);
        at(tray_menu_height(10, 3), 377.0);
    }

    #[test]
    fn tray_height_recording_always_shorter_than_default() {
        let default = tray_menu_height(12, 4);
        for mutes in 0..=2 {
            let h = tray_menu_height(8 + mutes, 2 + u32::from(mutes > 0));
            assert!(h < default, "recording mutes={mutes} height {h} >= default {default}");
        }
    }

    #[test]
    fn clamp_passes_through_when_rect_fully_inside_one_monitor() {
        let monitors = [Rect { x: 0, y: 0, width: 1920, height: 1080 }];
        let rect = Rect { x: 100, y: 200, width: 400, height: 300 };
        assert_eq!(clamp_rect_to_monitors(&monitors, rect), Some(rect));
    }

    #[test]
    fn clamp_picks_monitor_containing_rect_centre_and_clips_left_edge() {
        let monitors = [
            Rect { x: 0, y: 0, width: 1920, height: 1080 },
            Rect { x: 1920, y: 0, width: 1920, height: 1080 },
        ];
        let rect = Rect { x: 1820, y: 100, width: 600, height: 400 };
        let clamped = clamp_rect_to_monitors(&monitors, rect).expect("centre on right monitor");
        assert_eq!(clamped, Rect { x: 1920, y: 100, width: 500, height: 400 });
    }

    #[test]
    fn clamp_returns_none_when_centre_is_outside_every_monitor() {
        let monitors = [Rect { x: 0, y: 0, width: 1920, height: 1080 }];
        let rect = Rect { x: 5000, y: 100, width: 200, height: 200 };
        assert!(clamp_rect_to_monitors(&monitors, rect).is_none());
    }

    #[test]
    fn settings_default_matches_documented_values() {
        let s = SettingsData::default();
        assert!(s.autostart);
        assert!(s.capture_folder.is_none());
        assert_eq!(s.image_format, "png");
        assert!(s.recording_countdown);
        assert!(s.capture_audio);
        assert!(!s.capture_mic);
        assert_eq!(s.recording_fps, super::RECORDING_FPS);
        assert!(s.show_cursor);
        assert!(!s.show_mouse_clicks);
        assert!(!s.show_magnifier);
        assert_eq!(s.actions_dismiss_ms, 5000);
        assert_eq!(s.timer_seconds, 3);
        assert_eq!(s.tray_left_click, "region");
        assert_eq!(s.language, "en");
        assert!(s.shortcuts.is_empty());
    }

    #[test]
    fn settings_round_trip_through_json_preserves_every_field() {
        // Struct-literal init (vs `..Default::default()`) so adding a
        // field without listing it here is a compile error.
        let mut shortcuts = std::collections::HashMap::new();
        shortcuts.insert(String::from("overlay"), String::from("F2"));
        let original = SettingsData {
            autostart: false,
            capture_folder: Some(String::from("D:/Captures")),
            image_format: String::from("jpg"),
            upload_service: super::upload::UploadService::default(),
            recording_countdown: false,
            capture_audio: false,
            capture_mic: true,
            recording_fps: 60,
            show_cursor: false,
            show_mouse_clicks: true,
            show_magnifier: false,
            actions_dismiss_ms: 10_000,
            timer_seconds: 5,
            tray_left_click: String::from("ocr"),
            shortcuts,
            language: String::from("pt"),
        };

        let json = serde_json::to_string(&original).expect("serialize");
        let parsed: SettingsData = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed, original);
    }
}
