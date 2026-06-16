//! Global hotkeys: parse combos, (de)register with the OS, and the
//! Shortcuts-tab conflict model. Self-contained (fully-qualified paths).

pub const HK_IDS: [&str; 6] = ["region", "fullscreen", "window", "record", "menu", "ocr"];
pub const HK_DEFAULTS: [&str; 6] = [
    "PrintScreen",
    "Shift+PrintScreen",
    "Ctrl+Shift+W",
    "Ctrl+Shift+R",
    "Ctrl+Shift+K",
    "Ctrl+Shift+T",
];

/// Map a single combo token (a letter, digit, F-key or PrintScreen) to a `Code`.
pub fn key_to_code(k: &str) -> Option<global_hotkey::hotkey::Code> {
    use global_hotkey::hotkey::Code;
    Some(match k {
        "A" => Code::KeyA, "B" => Code::KeyB, "C" => Code::KeyC, "D" => Code::KeyD,
        "E" => Code::KeyE, "F" => Code::KeyF, "G" => Code::KeyG, "H" => Code::KeyH,
        "I" => Code::KeyI, "J" => Code::KeyJ, "K" => Code::KeyK, "L" => Code::KeyL,
        "M" => Code::KeyM, "N" => Code::KeyN, "O" => Code::KeyO, "P" => Code::KeyP,
        "Q" => Code::KeyQ, "R" => Code::KeyR, "S" => Code::KeyS, "T" => Code::KeyT,
        "U" => Code::KeyU, "V" => Code::KeyV, "W" => Code::KeyW, "X" => Code::KeyX,
        "Y" => Code::KeyY, "Z" => Code::KeyZ,
        "0" => Code::Digit0, "1" => Code::Digit1, "2" => Code::Digit2, "3" => Code::Digit3,
        "4" => Code::Digit4, "5" => Code::Digit5, "6" => Code::Digit6, "7" => Code::Digit7,
        "8" => Code::Digit8, "9" => Code::Digit9,
        "F1" => Code::F1, "F2" => Code::F2, "F3" => Code::F3, "F4" => Code::F4,
        "F5" => Code::F5, "F6" => Code::F6, "F7" => Code::F7, "F8" => Code::F8,
        "F9" => Code::F9, "F10" => Code::F10, "F11" => Code::F11, "F12" => Code::F12,
        "PrintScreen" | "PrtSc" => Code::PrintScreen,
        _ => return None,
    })
}

/// Parse a combo like `"Ctrl+Shift+W"` into a registrable `HotKey`.
pub fn parse_combo(s: &str) -> Option<global_hotkey::hotkey::HotKey> {
    use global_hotkey::hotkey::{HotKey, Modifiers};
    let mut mods = Modifiers::empty();
    let mut code = None;
    for part in s.split('+').map(str::trim) {
        match part {
            "Ctrl" | "Control" => mods |= Modifiers::CONTROL,
            "Shift" => mods |= Modifiers::SHIFT,
            "Alt" => mods |= Modifiers::ALT,
            "Cmd" | "Super" | "Win" => mods |= Modifiers::SUPER,
            other => code = Some(key_to_code(other)?),
        }
    }
    code.map(|c| HotKey::new((!mods.is_empty()).then_some(mods), c))
}

/// The Shortcuts tab's per-row conflict flags, as a Slint model.
pub fn shortcut_conflicts(hk: &HkState) -> slint::ModelRc<bool> {
    slint::ModelRc::new(slint::VecModel::from(hk.conflicts().to_vec()))
}

/// Registered global hotkeys, shared between the dispatch timer and the
/// Settings rebind handler so a new combo can replace the old registration.
pub struct HkState {
    pub(crate) mgr: Option<global_hotkey::GlobalHotKeyManager>,
    pub(crate) by_id: std::collections::HashMap<u32, usize>,
    pub(crate) current: [Option<global_hotkey::hotkey::HotKey>; 6],
    // The combo each row *wants* (even if it failed to register) — lets us flag
    // both rows when two share a combo, since the second one never registers.
    pub(crate) intended: [String; 6],
    // Recording-scoped F-keys — only registered during an active session so the
    // user's keyboard isn't claimed otherwise. Value = action code (see timer).
    pub(crate) rec_by_id: std::collections::HashMap<u32, u8>,
    pub(crate) rec_keys: Vec<global_hotkey::hotkey::HotKey>,
}

/// Recording F-keys (combo, action code). Codes: 0 stop · 1 pause · 2 restart
/// · 3 mute audio. F8/F9 match Loom/OBS; the Native records no mic so there is
/// no mute-mic key.
pub const REC_HOTKEYS: [(&str, u8); 5] = [("F8", 0), ("F9", 1), ("F10", 2), ("F7", 3), ("F6", 4)];

impl HkState {
    /// (Re)register row `idx` to `combo`, dropping its previous binding.
    /// Returns false if the combo is invalid or the OS refused it.
    pub(crate) fn bind(&mut self, idx: usize, combo: &str) -> bool {
        self.intended[idx] = combo.to_string();
        let Some(hk) = parse_combo(combo) else { return false };
        if let Some(old) = self.current[idx] {
            if let Some(mgr) = &self.mgr {
                let _ = mgr.unregister(old);
            }
            self.by_id.remove(&old.id());
        }
        if let Some(mgr) = &self.mgr {
            if mgr.register(hk).is_err() {
                self.current[idx] = None;
                return false;
            }
        }
        self.by_id.insert(hk.id(), idx);
        self.current[idx] = Some(hk);
        true
    }

    /// Per editable row: true when its combo won't fire — either the OS refused
    /// it (taken by another app, or it lost the race to an earlier identical
    /// row) or another row wants the very same combo.
    pub(crate) fn conflicts(&self) -> [bool; 6] {
        let ids: [Option<u32>; 6] = std::array::from_fn(|i| parse_combo(&self.intended[i]).map(|h| h.id()));
        std::array::from_fn(|i| {
            self.current[i].is_none() || ids[i].is_some() && (0..6).any(|j| j != i && ids[j] == ids[i])
        })
    }

    /// Retry any row the OS previously refused, in case its key was freed since
    /// (e.g. another app that held it closed). Only touches unregistered rows,
    /// so keys we already own are left untouched. Called when Settings opens to
    /// keep the conflict pills truthful without polling.
    pub(crate) fn refresh(&mut self) {
        for i in 0..6 {
            if self.current[i].is_none() && !self.intended[i].is_empty() {
                let combo = self.intended[i].clone();
                self.bind(i, &combo);
            }
        }
    }

    /// Register the recording F-keys for an active session.
    pub(crate) fn register_recording(&mut self) {
        for (combo, code) in REC_HOTKEYS {
            let Some(hk) = parse_combo(combo) else { continue };
            if let Some(mgr) = &self.mgr {
                if mgr.register(hk).is_err() {
                    continue;
                }
            }
            self.rec_by_id.insert(hk.id(), code);
            self.rec_keys.push(hk);
        }
    }

    /// Drop the recording F-keys when the session ends.
    pub(crate) fn unregister_recording(&mut self) {
        if let Some(mgr) = &self.mgr {
            for hk in self.rec_keys.drain(..) {
                let _ = mgr.unregister(hk);
            }
        }
        self.rec_keys.clear();
        self.rec_by_id.clear();
    }
}

/// Human-readable combo for the Shortcuts chips ("Ctrl+Shift+W" → "Ctrl + Shift + W").
pub fn combo_display(combo: &str) -> slint::SharedString {
    combo.replace("PrintScreen", "PrtSc").replace('+', " + ").into()
}
