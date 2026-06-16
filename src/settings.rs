//! User settings: JSON persistence, upload/update helpers, autostart and the
//! "open images with Clipo" file association.
use crate::{copy_text, upload};
use std::path::{Path, PathBuf};

/// serde default for the opt-OUT boolean toggles (default ON).
const fn default_true() -> bool {
    true
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// 0 = system · 1 = light · 2 = dark.
    pub(crate) theme_mode: i32,
    // Bring-your-own-keys Cloudflare R2 upload (empty = disabled). The endpoint
    // is derived from the account id; region is always "auto" for R2.
    pub(crate) r2_account_id: String,
    pub(crate) r2_access_key_id: String,
    pub(crate) r2_secret_access_key: String,
    pub(crate) r2_bucket: String,
    pub(crate) r2_public_url: String,
    // Storage: where captures are saved (None → Pictures\Clipo) and their format.
    pub(crate) capture_folder: Option<String>,
    pub(crate) image_format: i32, // 0 PNG · 1 JPG
    // Upload provider: 0 Cloudflare R2 (account id → endpoint) · 1 generic S3
    // (explicit endpoint + region). Credentials below are shared by both.
    pub(crate) upload_provider: i32,
    pub(crate) s3_endpoint: String,
    pub(crate) s3_region: String,
    // Global-hotkey overrides keyed by action id (region/fullscreen/window/
    // record/menu). Absent → the action's default combo.
    pub(crate) shortcuts: std::collections::HashMap<String, String>,
    // General toggles. `copy_clipboard`, `autostart` and `open_with_images` are
    // opt-OUT (default ON); the rest opt-in.
    #[serde(default = "default_true")]
    pub(crate) autostart: bool,
    pub(crate) highlight_cursor: bool,    // draw click rings while recording (show_clicks)
    #[serde(default = "default_true")]
    pub(crate) open_with_images: bool,    // register the HKCU "Open with Clipo" association
    pub(crate) copy_clipboard: Option<bool>,
    // Tray-icon left-click action: "" / "region" → region · "fullscreen" ·
    // "menu" (all-in-one) · "timer". Right-click always opens the tray menu.
    pub(crate) tray_left_click: String,
    // Remembered window state (no UI option): the history/editor windows reopen
    // maximized or windowed as they were last left. Default false = windowed.
    pub(crate) history_maximized: bool,
    pub(crate) editor_maximized: bool,
    // UI language (ISO-639-1: en/pt/es/fr/de/it/ja/ko/zh/ru/hi/ar). "" → en.
    pub(crate) language: String,
    // Show the zoom loupe during region selection (opt-OUT: None = on).
    pub(crate) magnifier: Option<bool>,
    // Recording options. `capture_audio` is opt-OUT (None = on).
    pub(crate) capture_audio: Option<bool>,
    pub(crate) capture_mic: bool,
    pub(crate) record_cursor: Option<bool>, // draw the mouse cursor into the video (opt-OUT)
    pub(crate) recording_fps: i32,        // 0/30 → 30 · 60 → 60
    pub(crate) recording_countdown: bool, // 3-2-1 before recording starts
    pub(crate) timer_seconds: i32,        // self-timer countdown (0 → default 3)
    pub(crate) dismiss_seconds: i32,      // post-capture panel auto-dismiss (0 → default 7)
    // Force the software H.264 encoder for recording. Auto-enabled when the GPU's
    // hardware encoder faults natively and crashes the process (see
    // encoder_sentinel_path); also user-toggleable for compatibility.
    pub(crate) software_encoder: bool,
}

impl Settings {
    /// Auto-copy every capture to the clipboard (defaults on when unset).
    pub(crate) fn copy_enabled(&self) -> bool {
        self.copy_clipboard.unwrap_or(true)
    }
    /// System-audio loopback capture (defaults on when unset).
    pub(crate) fn audio_enabled(&self) -> bool {
        self.capture_audio.unwrap_or(true)
    }
    /// Record the mouse cursor into the video (defaults on when unset).
    pub(crate) fn record_cursor_enabled(&self) -> bool {
        self.record_cursor.unwrap_or(true)
    }
    /// Show the region-selection magnifier loupe (defaults on when unset).
    pub(crate) fn magnifier_enabled(&self) -> bool {
        self.magnifier.unwrap_or(true)
    }
    /// Self-timer countdown length in seconds (defaults to 3 when unset).
    pub(crate) const fn timer_secs(&self) -> i32 {
        if self.timer_seconds <= 0 {
            3
        } else {
            self.timer_seconds
        }
    }
    /// Post-capture panel auto-dismiss in seconds (defaults to 5 when unset).
    pub(crate) const fn dismiss_secs(&self) -> u64 {
        if self.dismiss_seconds <= 0 {
            5
        } else {
            self.dismiss_seconds as u64
        }
    }
    /// Recording frame rate (30 default, 60 opt-in).
    pub(crate) const fn fps(&self) -> u32 {
        if self.recording_fps == 60 {
            60
        } else {
            30
        }
    }
    /// True once every field the active provider needs is filled.
    pub(crate) fn upload_ready(&self) -> bool {
        let creds_set = ![
            &self.r2_access_key_id,
            &self.r2_secret_access_key,
            &self.r2_bucket,
            &self.r2_public_url,
        ]
        .iter()
        .any(|f| f.is_empty());
        let loc_set = if self.upload_provider == 1 {
            !self.s3_endpoint.is_empty() && !self.s3_region.is_empty()
        } else {
            !self.r2_account_id.is_empty()
        };
        creds_set && loc_set
    }
}

/// Resolve the upload endpoint + region for the active provider.
pub fn upload_location(s: &Settings) -> (String, String) {
    if s.upload_provider == 1 {
        (s.s3_endpoint.clone(), s.s3_region.clone())
    } else {
        (
            format!("https://{}.r2.cloudflarestorage.com", s.r2_account_id),
            "auto".to_string(),
        )
    }
}

// ─────── global hotkeys ───────

/// Action ids (index = row order in the Shortcuts tab) and their default combos.
// UI languages (ISO-639-1), in the same order as the Settings dropdown labels.
pub const LANGS: [&str; 12] = [
    "en", "pt", "es", "fr", "de", "it", "ja", "ko", "zh", "ru", "hi", "ar",
];


// ─────── update check + self-update ───────

/// The release feed: a small JSON manifest published with each GitHub release.
/// Schema: `{ version, platforms: { "windows-x86_64": { url, signature } } }`,
/// where `signature` is the raw minisign `.minisig` text for the new `clipo.exe`.
pub const UPDATE_FEED: &str = "https://github.com/0hgawa/Clipo/releases/latest/download/latest.json";

/// minisign public key (the base64 key line) the downloaded `clipo.exe` must be
/// signed with. The matching secret key (`.keys/clipo.key`) signs it at release
/// time (see installer/build.ps1). A tampered or unsigned download is rejected
/// before it ever replaces the running binary.
const UPDATE_PUBKEY: &str = "RWQVALBm0h7qFoqFzRiHilRh0PZodqrBZKlPcF6SJg/nTuhSStmOIMzP";

/// A newer release: version, the new `clipo.exe` URL, and its minisign
/// signature — all that's needed to download, verify and swap it in.
#[derive(Clone)]
pub struct UpdateInfo {
    pub version: String,
    url: String,
    signature: String,
}

/// (major, minor, patch) from a semver-ish string, ignoring pre-release/build.
pub fn parse_semver(v: &str) -> (u32, u32, u32) {
    let mut it = v.trim().trim_start_matches('v').split(['.', '-', '+']);
    let mut next = || it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (next(), next(), next())
}

/// Check the release feed; `Some(UpdateInfo)` when a newer build is published.
/// Blocking — call from a worker thread.
pub fn check_update_blocking() -> Result<Option<UpdateInfo>, String> {
    let body = upload::get(UPDATE_FEED).map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let version = json.get("version").and_then(|v| v.as_str()).ok_or("no version in feed")?;
    if parse_semver(version) <= parse_semver(env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    let plat = json.pointer("/platforms/windows-x86_64").ok_or("no windows-x86_64 in feed")?;
    let url = plat.get("url").and_then(|v| v.as_str()).ok_or("no url in feed")?;
    let signature = plat.get("signature").and_then(|v| v.as_str()).ok_or("no signature in feed")?;
    Ok(Some(UpdateInfo {
        version: version.to_owned(),
        url: url.to_owned(),
        signature: signature.to_owned(),
    }))
}

/// Download the new `clipo.exe`, verify its minisign signature against the
/// embedded public key, and swap it in for the running executable in place (the
/// per-user install dir is writable without elevation). The caller then
/// relaunches and exits so the new image takes over. Only the app binary is
/// fetched — the ffmpeg sidecar and shortcuts the first-run installer placed are
/// left untouched. Blocking — call from a worker thread.
pub fn download_and_apply_update(info: &UpdateInfo) -> Result<(), String> {
    let bytes = upload::download(&info.url).map_err(|e| e.to_string())?;
    verify_signature(&bytes, &info.signature)?;
    let staged = std::env::temp_dir().join("clipo-update.exe");
    std::fs::write(&staged, &bytes).map_err(|e| format!("write update: {e}"))?;
    self_replace::self_replace(&staged).map_err(|e| format!("replace executable: {e}"))?;
    let _ = std::fs::remove_file(&staged);
    Ok(())
}

/// Reject a download whose minisign signature doesn't match the embedded key.
fn verify_signature(bytes: &[u8], signature: &str) -> Result<(), String> {
    use minisign_verify::{PublicKey, Signature};
    let pk = PublicKey::from_base64(UPDATE_PUBKEY).map_err(|e| format!("public key: {e}"))?;
    let sig = Signature::decode(signature).map_err(|e| format!("signature: {e}"))?;
    pk.verify(bytes, &sig, false)
        .map_err(|_| "signature does not match — refusing to install the download".to_owned())
}

/// path→public-URL cache (`%LOCALAPPDATA%\Clipo\uploads.json`) so re-uploading
/// the same file (e.g. again from history) just returns its known URL.
fn uploads_cache_path() -> PathBuf {
    settings_path().with_file_name("uploads.json")
}

/// Keyed by destination + path: changing the bucket/public URL misses the cache.
fn cache_key(public_url: &str, path: &Path) -> String {
    format!("{public_url}|{}", path.display())
}

fn cached_upload_url(public_url: &str, path: &Path) -> Option<String> {
    let body = std::fs::read_to_string(uploads_cache_path()).ok()?;
    let map: std::collections::HashMap<String, String> = serde_json::from_str(&body).ok()?;
    map.get(&cache_key(public_url, path)).cloned()
}

fn remember_upload_url(public_url: &str, path: &Path, url: &str) {
    let p = uploads_cache_path();
    let mut map: std::collections::HashMap<String, String> = std::fs::read_to_string(&p)
        .ok()
        .and_then(|b| serde_json::from_str(&b).ok())
        .unwrap_or_default();
    map.insert(cache_key(public_url, path), url.to_string());
    if let Ok(json) = serde_json::to_string_pretty(&map) {
        if let Err(e) = std::fs::write(&p, json) {
            tracing::warn!(error = %e, "write upload cache");
        }
    }
}

/// A fast "already uploaded under the current bucket?" check — loads settings +
/// the cache once, then tests paths in memory. Always false when upload is unset.
pub fn uploaded_lookup() -> impl Fn(&Path) -> bool {
    let public = load_settings().r2_public_url;
    let cache: std::collections::HashMap<String, String> = std::fs::read_to_string(uploads_cache_path())
        .ok()
        .and_then(|b| serde_json::from_str(&b).ok())
        .unwrap_or_default();
    move |path: &Path| !public.is_empty() && cache.contains_key(&cache_key(&public, path))
}

/// Whether `path` has already been uploaded under the current bucket.
pub fn is_uploaded(path: &Path) -> bool {
    uploaded_lookup()(path)
}

/// Re-copy the cached public URL for `path` (no network). Returns false if the
/// capture hasn't been uploaded under the current bucket.
pub fn copy_uploaded_link(path: &Path) -> bool {
    let s = load_settings();
    cached_upload_url(&s.r2_public_url, path).is_some_and(|url| {
        copy_text(&url);
        true
    })
}

/// Upload `path` to the user's R2 bucket (blocking; call off the UI thread).
/// On success copies the public URL to the clipboard and returns it. A cache
/// hit skips the network entirely.
pub fn upload_capture_blocking(s: &Settings, path: &Path) -> Result<String, String> {
    if let Some(url) = cached_upload_url(&s.r2_public_url, path) {
        copy_text(&url);
        return Ok(url);
    }
    let (endpoint, region) = upload_location(s);
    let key = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("invalid capture path")?
        .to_owned();
    let ext = path
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default();
    let content_type = if ext.eq_ignore_ascii_case("mp4") {
        "video/mp4"
    } else if ext.eq_ignore_ascii_case("jpg") || ext.eq_ignore_ascii_case("jpeg") {
        "image/jpeg"
    } else if ext.eq_ignore_ascii_case("gif") {
        "image/gif"
    } else {
        "image/png"
    };
    let bytes = std::fs::read(path).map_err(|e| format!("read: {e}"))?;
    let target = upload::S3Target {
        endpoint: &endpoint,
        region: &region,
        access_key_id: &s.r2_access_key_id,
        secret_access_key: &s.r2_secret_access_key,
        bucket: &s.r2_bucket,
        public_url: &s.r2_public_url,
    };
    let url = upload::upload(&target, &bytes, &key, content_type).map_err(|e| e.to_string())?;
    copy_text(&url);
    remember_upload_url(&s.r2_public_url, path, &url);
    Ok(url)
}

/// Validate the upload config by PUTting a tiny marker object — surfaces the
/// exact provider error (NoSuchBucket, AccessDenied, …). Blocking.
pub fn test_upload_blocking(s: &Settings) -> Result<(), String> {
    let (endpoint, region) = upload_location(s);
    let target = upload::S3Target {
        endpoint: &endpoint,
        region: &region,
        access_key_id: &s.r2_access_key_id,
        secret_access_key: &s.r2_secret_access_key,
        bucket: &s.r2_bucket,
        public_url: &s.r2_public_url,
    };
    upload::upload(&target, b"clipo connection test", "clipo-connection-test.txt", "text/plain")
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn settings_path() -> PathBuf {
    let dir = dirs::data_local_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Clipo");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("settings.json")
}

/// Marker that a hardware-encoded recording is in flight. The recorder writes it
/// before touching the GPU's H.264 encoder and clears it on a clean finish; a
/// native encoder fault kills the process before the clear runs, so if this file
/// survives a launch the previous recording crashed — and we fall back to the
/// software encoder (the Chromium GPU-crash → software-fallback pattern).
pub fn encoder_sentinel_path() -> PathBuf {
    settings_path().with_file_name("encoder-hw.inflight")
}

/// Register/unregister "Clipo" under HKCU…\Run so it launches at login.
pub fn set_autostart(enabled: bool) {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE};
    let hkcu = winreg::RegKey::predef(HKEY_CURRENT_USER);
    let Ok(run) = hkcu.open_subkey_with_flags(
        r"Software\Microsoft\Windows\CurrentVersion\Run",
        KEY_SET_VALUE | KEY_QUERY_VALUE,
    ) else {
        return;
    };
    if enabled {
        if let Ok(exe) = std::env::current_exe() {
            if let Err(e) = run.set_value("Clipo", &format!("\"{}\"", exe.display())) {
                tracing::warn!(error = %e, "enable autostart");
            }
        }
    } else if let Err(e) = run.delete_value("Clipo") {
        tracing::warn!(error = %e, "disable autostart");
    }
}

/// Image extensions Clipo offers to open. Per-user (HKCU), reversible, no admin.
pub const ASSOC_EXTS: [&str; 6] = ["png", "jpg", "jpeg", "gif", "webp", "bmp"];
pub const ASSOC_PROGID: &str = "Clipo.Image";

/// Add/remove the HKCU "Open with Clipo" association (a ProgID + OpenWithProgids
/// entries). Doesn't hijack the system default — Clipo just appears in "Open with".
pub fn set_image_association(enabled: bool) {
    use winreg::enums::{HKEY_CURRENT_USER, KEY_SET_VALUE};
    let hkcu = winreg::RegKey::predef(HKEY_CURRENT_USER);
    if enabled {
        let Ok(exe) = std::env::current_exe() else { return };
        let exe = exe.display().to_string();
        if let Ok((k, _)) = hkcu.create_subkey(format!(r"Software\Classes\{ASSOC_PROGID}")) {
            let _ = k.set_value("", &"Clipo Image");
        }
        if let Ok((k, _)) = hkcu.create_subkey(format!(r"Software\Classes\{ASSOC_PROGID}\DefaultIcon")) {
            // Document-style image icon embedded as resource id 2 (see app.rc).
            let _ = k.set_value("", &format!("\"{exe}\",-2"));
        }
        if let Ok((k, _)) = hkcu.create_subkey(format!(r"Software\Classes\{ASSOC_PROGID}\shell\open\command")) {
            let _ = k.set_value("", &format!("\"{exe}\" \"%1\""));
        }
        for ext in ASSOC_EXTS {
            if let Ok((k, _)) = hkcu.create_subkey(format!(r"Software\Classes\.{ext}\OpenWithProgids")) {
                let _ = k.set_value(ASSOC_PROGID, &"");
            }
        }
    } else {
        for ext in ASSOC_EXTS {
            if let Ok(k) = hkcu.open_subkey_with_flags(format!(r"Software\Classes\.{ext}\OpenWithProgids"), KEY_SET_VALUE) {
                let _ = k.delete_value(ASSOC_PROGID);
            }
        }
        let _ = hkcu.delete_subkey_all(format!(r"Software\Classes\{ASSOC_PROGID}"));
    }
    // Tell Explorer the associations changed so the "Open with" entry appears /
    // disappears immediately instead of after the shell next re-reads.
    // SAFETY: SHCNE_ASSOCCHANGED carries no item pointers (both are None).
    unsafe {
        windows::Win32::UI::Shell::SHChangeNotify(
            windows::Win32::UI::Shell::SHCNE_ASSOCCHANGED,
            windows::Win32::UI::Shell::SHCNF_IDLIST,
            None,
            None,
        );
    }
}

pub fn load_settings() -> Settings {
    std::fs::read_to_string(settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        // No file (fresh install) → deserialize an empty object so the per-field
        // serde defaults (e.g. the opt-OUT toggles) apply instead of `false`.
        .unwrap_or_else(|| serde_json::from_str("{}").expect("default settings"))
}

pub fn save_settings(s: &Settings) {
    if let Ok(json) = serde_json::to_string_pretty(s) {
        if let Err(e) = std::fs::write(settings_path(), json) {
            tracing::warn!(error = %e, "write settings");
        }
    }
}
