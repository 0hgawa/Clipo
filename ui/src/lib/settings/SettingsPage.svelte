<script lang="ts">
  /**
   * Settings window.
   *
   * Lives in the pre-declared `settings` window. Pulls state via
   * `get_settings`; every change is persisted server-side by
   * `update_settings`, which also reconciles the OS-level autostart
   * entry.
   */
  import { invoke } from "@tauri-apps/api/core";
  import { open as openDialog } from "@tauri-apps/plugin-dialog";
  import { check, type Update } from "@tauri-apps/plugin-updater";
  import { relaunch } from "@tauri-apps/plugin-process";
  import { Copy, FolderOpen, FolderTree, Info, Keyboard, RefreshCw, RotateCcw, Settings as SettingsIcon, Video } from "@lucide/svelte";
  import { getTauriVersion } from "@tauri-apps/api/app";
  import type { Component } from "svelte";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { onDestroy, onMount } from "svelte";
  import { SvelteMap } from "svelte/reactivity";
  import WindowChrome from "../chrome/WindowChrome.svelte";
  import Button from "../components/Button.svelte";
  import KeyCaptureInput from "../components/KeyCaptureInput.svelte";
  import { formatCombo } from "../components/keyCombo";
  import SegmentedControl from "../components/SegmentedControl.svelte";
  import Toggle from "../components/Toggle.svelte";
  import { LOCALE_LABELS, SUPPORTED_LOCALES, fmt, initLocaleSync, setLocale, t } from "../i18n/index.svelte";
  import { getThemePref, setThemePref, type ThemePref } from "../theme.svelte";

  type UploadService = "catbox" | "zerox0";

  type AppSettings = {
    autostart: boolean;
    captureFolder: string | null;
    /** Screenshot file format: "png" (lossless) or "jpg" (smaller). */
    imageFormat: string;
    uploadService: UploadService;
    recordingCountdown: boolean;
    captureAudio: boolean;
    captureMic: boolean;
    recordingFps: number;
    showCursor: boolean;
    showMouseClicks: boolean;
    showMagnifier: boolean;
    /** Post-capture actions panel auto-dismiss delay, in milliseconds. */
    actionsDismissMs: number;
    /** Countdown duration in seconds — drives the photo self-timer and
     * the pre-recording countdown when `recordingCountdown` is on. */
    timerSeconds: number;
    /** Tray icon left-click action: region | fullscreen | menu | timer | ocr. */
    trayLeftClick: string;
    /** ISO-639-1 code matching one of `SUPPORTED_LOCALES`. */
    language: string;
    /** `id → tauri-accelerator string`. Empty / missing keys mean
     * "use the factory default" — backend resolves via `resolved_combo`. */
    shortcuts: Record<string, string>;
  };

  const FPS_OPTIONS: readonly { value: number; label: string }[] = [
    { value: 30, label: "30 fps" },
    { value: 60, label: "60 fps" },
  ] as const;

  /** Post-capture panel auto-dismiss choices (ms). Labels are unit
   * suffixes, not translations — "5 s" reads the same in every locale. */
  const DISMISS_OPTIONS: readonly { value: number; label: string }[] = [
    { value: 3000, label: "3 s" },
    { value: 5000, label: "5 s" },
    { value: 10000, label: "10 s" },
  ] as const;

  /** Self-timer / pre-recording countdown choices (seconds). Matches
   * CleanShot / Snagit shipping presets; labels are unit-suffix only. */
  const TIMER_OPTIONS: readonly { value: number; label: string }[] = [
    { value: 1, label: "1 s" },
    { value: 3, label: "3 s" },
    { value: 5, label: "5 s" },
    { value: 10, label: "10 s" },
  ] as const;

  /** Catalog entry returned by `list_hotkey_defs`. The backend is the
   * source of truth for which hotkeys exist; UI just renders the list. */
  type HotkeyInfo = {
    id: string;
    label: string;
    defaultCombo: string;
  };

  type ShortcutStatus = "active" | "conflict" | "invalid" | "inactive";

  /** Labels match the official names of each host; descriptions sit in
   * the row's `.row-desc` so this list stays a flat (value, label). */
  /** Upload-service host labels are brand names, not translations —
   * "Catbox" stays "Catbox" in every locale. The array is constant. */
  const UPLOAD_SERVICES: readonly { value: UploadService; label: string }[] = [
    { value: "catbox", label: "Catbox" },
    { value: "zerox0", label: "0x0.st" },
  ] as const;

  /** Screenshot format choices. Labels are format names, not
   * translated; the "jpg" value matches the file extension. */
  const FORMAT_OPTIONS: readonly { value: string; label: string }[] = [
    { value: "png", label: "PNG" },
    { value: "jpg", label: "JPEG" },
  ] as const;

  /** Language picker rows: `{value, label}` pairs in the same shape as
   * the upload chips. Labels come from the static `LOCALE_LABELS`
   * (each language name written in its own script). */
  const LANGUAGE_OPTIONS: readonly { value: string; label: string }[] = SUPPORTED_LOCALES.map(
    (code) => ({ value: code, label: LOCALE_LABELS[code] ?? code }),
  );

  // Labels flow through t() so the picker relabels on a language flip.
  const themeOptions = $derived<readonly { value: ThemePref; label: string }[]>([
    { value: "system", label: t().settingsThemeSystem },
    { value: "light", label: t().settingsThemeLight },
    { value: "dark", label: t().settingsThemeDark },
  ]);

  // Tray left-click choices. Labels reuse the menu / tray strings so the
  // names match what those surfaces already call each action.
  const trayClickOptions = $derived<readonly { value: string; label: string }[]>([
    { value: "region", label: t().menuCardCaptureArea },
    { value: "fullscreen", label: t().menuCardFullscreen },
    { value: "menu", label: t().trayAllInOneMenu },
    { value: "timer", label: t().menuCardTimer },
    { value: "ocr", label: t().menuCardCaptureText },
  ]);

  let settings = $state<AppSettings>({
    autostart: true,
    captureFolder: null,
    imageFormat: "png",
    uploadService: "catbox",
    recordingCountdown: true,
    captureAudio: true,
    captureMic: false,
    recordingFps: 30,
    showCursor: true,
    showMouseClicks: false,
    showMagnifier: false,
    actionsDismissMs: 5000,
    timerSeconds: 3,
    trayLeftClick: "region",
    language: "en",
    shortcuts: {},
  });
  let unlistenLocale: UnlistenFn | undefined;
  // `shortcuts:updated` fires when a session start/stop flips the
  // recording-scoped hotkeys between `"inactive"` and `"active"`. Re-
  // fetching the status here keeps the chips honest in real time
  // (otherwise they'd show stale `"inactive"` while a recording runs).
  let unlistenShortcuts: UnlistenFn | undefined;
  let busy = $state(false);
  let error = $state<string | null>(null);

  let hotkeyDefs = $state<HotkeyInfo[]>([]);
  // Split the catalog into two groups so the Shortcuts tab can render
  // a dedicated "During recording" section with its own heading + hint.
  // The boundary mirrors the Rust `is_recording_scoped` helper — the
  // `recording-` id prefix is the contract between the two layers.
  const regularDefs = $derived(hotkeyDefs.filter((d) => !d.id.startsWith("recording-")));
  const recordingDefs = $derived(hotkeyDefs.filter((d) => d.id.startsWith("recording-")));
  /** id → status. SvelteMap so the chip re-renders the moment we
   * patch a single entry, without re-fetching the whole object. */
  const shortcutStatus = new SvelteMap<string, ShortcutStatus>();

  async function refreshStatus() {
    try {
      const map = await invoke<Record<string, ShortcutStatus>>("get_shortcut_status");
      // Apply diff so the chip transitions are minimal — drop missing
      // keys then write the latest. (Map equality would re-render the
      // whole row; targeted set/delete only touches what changed.)
      for (const id of [...shortcutStatus.keys()]) {
        if (!(id in map)) shortcutStatus.delete(id);
      }
      for (const [id, status] of Object.entries(map)) {
        if (shortcutStatus.get(id) !== status) shortcutStatus.set(id, status);
      }
    } catch (e) {
      console.warn("get_shortcut_status", e);
    }
  }

  onMount(async () => {
    unlistenLocale = await initLocaleSync();
    unlistenShortcuts = await listen("shortcuts:updated", () => {
      void refreshStatus();
    });
    try {
      // Settings + hotkey catalog + build info + Tauri version in
      // parallel — four independent reads, total wall-time is the
      // slowest one only. Build info is `&'static str` on the Rust
      // side (zero alloc); `getTauriVersion` is a cheap Tauri-API
      // call.
      const [s, defs, bi, tv] = await Promise.all([
        invoke<AppSettings>("get_settings"),
        invoke<HotkeyInfo[]>("list_hotkey_defs"),
        invoke<BuildInfo>("get_build_info"),
        getTauriVersion(),
      ]);
      settings = s;
      hotkeyDefs = defs;
      buildInfo = bi;
      tauriVersion = tv;
      await refreshStatus();
    } catch (e) {
      error = String(e);
    }
  });

  onDestroy(() => {
    unlistenLocale?.();
    unlistenShortcuts?.();
  });

  async function patch(next: AppSettings) {
    if (busy) return;
    busy = true;
    error = null;
    try {
      settings = await invoke<AppSettings>("update_settings", { settings: next });
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  async function setAutostart(next: boolean) {
    await patch({ ...settings, autostart: next });
  }

  async function pickCaptureFolder() {
    try {
      const picked = await openDialog({ directory: true, multiple: false });
      if (typeof picked === "string" && picked) {
        await patch({ ...settings, captureFolder: picked });
      }
    } catch (e) {
      error = String(e);
    }
  }

  async function resetCaptureFolder() {
    await patch({ ...settings, captureFolder: null });
  }

  async function setUploadService(next: UploadService) {
    if (next === settings.uploadService) return;
    await patch({ ...settings, uploadService: next });
  }

  async function setRecordingCountdown(next: boolean) {
    await patch({ ...settings, recordingCountdown: next });
  }

  async function setRecordingFps(next: number) {
    if (next === settings.recordingFps) return;
    await patch({ ...settings, recordingFps: next });
  }

  async function setActionsDismiss(next: number) {
    if (next === settings.actionsDismissMs) return;
    await patch({ ...settings, actionsDismissMs: next });
  }

  async function setTimerSeconds(next: number) {
    if (next === settings.timerSeconds) return;
    await patch({ ...settings, timerSeconds: next });
  }

  async function setTrayLeftClick(next: string) {
    if (next === settings.trayLeftClick) return;
    await patch({ ...settings, trayLeftClick: next });
  }

  async function setImageFormat(next: string) {
    if (next === settings.imageFormat) return;
    await patch({ ...settings, imageFormat: next });
  }

  async function setShowCursor(next: boolean) {
    await patch({ ...settings, showCursor: next });
  }

  async function setShowMouseClicks(next: boolean) {
    await patch({ ...settings, showMouseClicks: next });
  }

  async function setShowMagnifier(next: boolean) {
    await patch({ ...settings, showMagnifier: next });
  }

  /** Language change feels instant: flip the local `setLocale` first
   * so the picker re-labels itself + every surface in this window
   * before the IPC round-trip; the backend then emits
   * `settings:language-changed` which other windows pick up via
   * `initLocaleSync`. If the patch fails (rare — settings.json
   * unwritable, etc.) we'd briefly show the new language until the
   * next mount; acceptable trade-off for the snappier UX. */
  async function setLanguage(next: string) {
    if (next === settings.language) return;
    setLocale(next);
    await patch({ ...settings, language: next });
  }

  async function setCaptureAudio(next: boolean) {
    await patch({ ...settings, captureAudio: next });
  }

  async function setCaptureMic(next: boolean) {
    await patch({ ...settings, captureMic: next });
  }

  /** Bind / unbind a hotkey id. `combo === null` resets to the
   * factory default (delete from the overrides map). After the
   * patch lands, re-fetch status so the chip reflects whether
   * the new combo actually registered (or hit a conflict). */
  async function setShortcut(id: string, combo: string | null) {
    const shortcuts = { ...settings.shortcuts };
    if (combo === null || combo === "") {
      delete shortcuts[id];
    } else {
      shortcuts[id] = combo;
    }
    await patch({ ...settings, shortcuts });
    await refreshStatus();
  }

  function effectiveCombo(def: HotkeyInfo): string {
    return settings.shortcuts[def.id] ?? def.defaultCombo;
  }

  function statusLabel(s: ShortcutStatus | undefined): string {
    if (s === "conflict") return t().settingsShortcutsStatusConflict;
    if (s === "invalid") return t().settingsShortcutsStatusInvalid;
    if (s === "inactive") return t().settingsShortcutsStatusInactive;
    return t().settingsShortcutsStatusActive;
  }

  /** How many hotkeys the user has overridden. Drives the global
   * "Restaurar padrões" button — counter in the label gives a sense
   * of scope before clicking, and the button hides itself when zero
   * (no work to do). */
  const customShortcutCount = $derived(Object.keys(settings.shortcuts).length);

  /** Copy the diagnostic block to the clipboard. WebView2's
   * `navigator.clipboard.writeText` works inside Tauri without
   * needing the clipboard plugin (the API runs in the renderer's
   * secure context). */
  async function copyDiagnostic() {
    if (!diagnosticText) return;
    try {
      await navigator.clipboard.writeText(diagnosticText);
    } catch (e) {
      console.error("clipboard write", e);
    }
  }

  /** Wipe every preference back to the factory baseline. Default
   * lives in Rust (`SettingsData::default`); the backend command
   * reuses `update_settings` so autostart sync + hotkey re-register
   * + JSON persistence all run in the same atomic pass. */
  async function resetAllSettings() {
    const ok = window.confirm(t().settingsAboutResetConfirm);
    if (!ok) return;
    if (busy) return;
    busy = true;
    error = null;
    try {
      settings = await invoke<AppSettings>("reset_settings");
      await refreshStatus();
    } catch (e) {
      error = String(e);
    } finally {
      busy = false;
    }
  }

  /** One-shot reset of every hotkey override. Backend's diff inside
   * `apply_shortcut_changes` only re-registers the ones that actually
   * changed, so cost scales with `customShortcutCount`, not with
   * `HOTKEY_DEFS.len()`. Confirmation prompt because the action is
   * destructive (overrides are lost). */
  async function resetAllShortcuts() {
    if (customShortcutCount === 0) return;
    const ok = window.confirm(
      fmt(
        customShortcutCount === 1
          ? t().settingsShortcutsResetConfirm
          : t().settingsShortcutsResetConfirmPlural,
        { count: customShortcutCount },
      ),
    );
    if (!ok) return;
    await patch({ ...settings, shortcuts: {} });
    await refreshStatus();
  }

  /** Four sidebar sections, each with at least two rows so no
   * category feels sparse: `Geral` (app + recording behaviour),
   * `Áudio` (system + mic), `Armazenamento` (capture folder +
   * upload host), `Atalhos` (hotkey grid). Sidebar over top tabs
   * once we crossed 3 categories — matches Windows 11 Settings /
   * macOS System Settings / Linear, all of which switch to a
   * sidebar once the nav gets richer than a couple of pills. */
  type TabId = "general" | "recording" | "storage" | "shortcuts" | "about";
  const TABS = $derived.by<readonly { id: TabId; label: string; icon: Component }[]>(() => [
    { id: "general", label: t().settingsTabGeneral, icon: SettingsIcon },
    { id: "recording", label: t().settingsTabRecording, icon: Video },
    { id: "storage", label: t().settingsTabStorage, icon: FolderTree },
    { id: "shortcuts", label: t().settingsTabShortcuts, icon: Keyboard },
    { id: "about", label: t().settingsTabAbout, icon: Info },
  ]);
  let activeTab = $state<TabId>("general");

  type BuildInfo = {
    version: string;
    commit: string;
    commitDate: string;
  };
  let buildInfo = $state<BuildInfo | null>(null);
  let tauriVersion = $state<string>("");

  // Self-update (Settings → About). The updater verifies each package
  // against the bundled minisign pubkey; on install it runs the new
  // NSIS installer and `relaunch()` restarts into the new version.
  type UpdatePhase = "idle" | "checking" | "uptodate" | "available" | "downloading" | "error";
  let updatePhase = $state<UpdatePhase>("idle");
  let updateVersion = $state("");
  let updatePct = $state(0);
  let updateError = $state("");
  let pendingUpdate: Update | null = null;

  async function checkForUpdates() {
    updatePhase = "checking";
    updateError = "";
    try {
      const found = await check();
      if (found) {
        pendingUpdate = found;
        updateVersion = found.version;
        updatePhase = "available";
      } else {
        updatePhase = "uptodate";
      }
    } catch (e) {
      updatePhase = "error";
      updateError = String(e);
    }
  }

  async function installUpdate() {
    if (!pendingUpdate) return;
    updatePhase = "downloading";
    updatePct = 0;
    let total = 0;
    let got = 0;
    try {
      await pendingUpdate.downloadAndInstall((ev) => {
        if (ev.event === "Started") {
          total = ev.data.contentLength ?? 0;
        } else if (ev.event === "Progress") {
          got += ev.data.chunkLength;
          if (total > 0) updatePct = Math.round((got / total) * 100);
        } else if (ev.event === "Finished") {
          updatePct = 100;
        }
      });
      // Installed — restart into the freshly installed version.
      await relaunch();
    } catch (e) {
      updatePhase = "error";
      updateError = String(e);
    }
  }

  /** Single multi-line string used by both the "Copiar info" button
   * (clipboard payload for bug reports) and the visible block. Sole
   * source of truth for the formatting. Returns "" until the async
   * loads land — caller decides what fallback to render. */
  const diagnosticText = $derived.by(() => {
    if (!buildInfo) return "";
    return [
      `Clipo ${buildInfo.version}`,
      `Commit: ${buildInfo.commit}`,
      `${t().settingsAboutBuiltLabel}: ${buildInfo.commitDate}`,
      `Tauri: ${tauriVersion || "?"}`,
    ].join("\n");
  });

  const activeLabel = $derived(
    TABS.find((t) => t.id === activeTab)?.label ?? "",
  );
</script>

<main class="root">
  <header class="titlebar" data-tauri-drag-region>
    <h1 data-tauri-drag-region>{t().settingsTitle}</h1>
    <WindowChrome />
  </header>

  <div class="body">
    <div class="sidebar" role="tablist" aria-label={t().settingsSectionsAria}>
      {#each TABS as tab (tab.id)}
        {@const Icon = tab.icon}
        <button
          type="button"
          class="nav"
          class:active={activeTab === tab.id}
          role="tab"
          aria-selected={activeTab === tab.id}
          onclick={() => (activeTab = tab.id)}
        >
          <Icon size={16} strokeWidth={2} />
          <span>{tab.label}</span>
        </button>
      {/each}
    </div>

    <div class="content" role="tabpanel" aria-labelledby={`tab-${activeTab}`}>
      <header class="section-title">
        <h2>{activeLabel}</h2>
        {#if activeTab === "shortcuts" && customShortcutCount > 0}
          <Button
            size="sm"
            disabled={busy}
            onclick={resetAllShortcuts}
            title={fmt(
              customShortcutCount === 1
                ? t().settingsShortcutsResetTitle
                : t().settingsShortcutsResetTitlePlural,
              { count: customShortcutCount },
            )}
            ariaLabel={t().settingsShortcutsResetLabel}
          >
            <RotateCcw size={14} />
            <span>{fmt(t().settingsShortcutsResetWithCount, { count: customShortcutCount })}</span>
          </Button>
        {/if}
      </header>

      {#if error}
        <p class="error">{fmt(t().settingsErrorBackend, { error })}</p>
      {/if}

      {#if activeTab === "general"}
        <!-- Appearance prefs lead the tab, ordered by how often they're
             touched: theme (light/dark is a frequent toggle) before
             language (set once, already defaults from the OS). Both sit
             ahead of recording-specific prefs (in the Recording tab). -->
        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsThemeTitle}</span>
            <span class="row-desc">{t().settingsThemeDesc}</span>
          </div>
          <SegmentedControl
            value={getThemePref()}
            options={themeOptions}
            onchange={setThemePref}
            ariaLabel={t().settingsThemeTitle}
          />
        </div>

        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsLanguageTitle}</span>
            <span class="row-desc">{t().settingsLanguageDesc}</span>
          </div>
          <!-- Native <select> for the language picker — 12 entries is
               above the chip-segmented threshold (segmented controls
               read as a row of pills which would wrap awkwardly), and
               <select> automatically inherits the OS keyboard
               navigation (typeahead, ↑/↓, Enter) without us writing
               any handlers. -->
          <select
            class="language-select"
            value={settings.language}
            disabled={busy}
            aria-label={t().settingsLanguageAria}
            onchange={(e) => setLanguage((e.currentTarget as HTMLSelectElement).value)}
          >
            {#each LANGUAGE_OPTIONS as lang (lang.value)}
              <option value={lang.value}>{lang.label}</option>
            {/each}
          </select>
        </div>

        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsTrayClickTitle}</span>
            <span class="row-desc">{t().settingsTrayClickDesc}</span>
          </div>
          <select
            class="language-select"
            value={settings.trayLeftClick}
            disabled={busy}
            aria-label={t().settingsTrayClickTitle}
            onchange={(e) => setTrayLeftClick((e.currentTarget as HTMLSelectElement).value)}
          >
            {#each trayClickOptions as opt (opt.value)}
              <option value={opt.value}>{opt.label}</option>
            {/each}
          </select>
        </div>

        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsAutostartTitle}</span>
            <span class="row-desc">{t().settingsAutostartDesc}</span>
          </div>
          <Toggle
            checked={settings.autostart}
            disabled={busy}
            ariaLabel={t().settingsAutostartTitle}
            onchange={setAutostart}
          />
        </div>

        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsMagnifierTitle}</span>
            <span class="row-desc">{t().settingsMagnifierDesc}</span>
          </div>
          <Toggle
            checked={settings.showMagnifier}
            disabled={busy}
            ariaLabel={t().settingsMagnifierTitle}
            onchange={setShowMagnifier}
          />
        </div>

        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsDismissTitle}</span>
            <span class="row-desc">{t().settingsDismissDesc}</span>
          </div>
          <SegmentedControl
            value={settings.actionsDismissMs}
            options={DISMISS_OPTIONS}
            onchange={setActionsDismiss}
            ariaLabel={t().settingsDismissTitle}
            disabled={busy}
          />
        </div>

        <!-- Countdown duration drives the photo self-timer surface and
             the optional pre-recording countdown (Recording tab). One
             knob, two flows — keeps the mental model "this is how long
             the dial counts down" no matter which entry triggered it. -->
        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsTimerSecondsTitle}</span>
            <span class="row-desc">{t().settingsTimerSecondsDesc}</span>
          </div>
          <SegmentedControl
            value={settings.timerSeconds}
            options={TIMER_OPTIONS}
            onchange={setTimerSeconds}
            ariaLabel={t().settingsTimerSecondsTitle}
            disabled={busy}
          />
        </div>
      {:else if activeTab === "recording"}
        <!-- Recording tab consolidates every preference that affects
             what the resulting MP4 contains: pacing (countdown before
             arming), quality (fps), visuals (cursor), and audio tracks
             (system + mic). Previously the first three lived under
             "General" and the last two under "Audio" — splitting by
             *kind of input* didn't help discoverability when every row
             was about the same artefact. -->
        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsCountdownTitle}</span>
            <span class="row-desc">{t().settingsCountdownDesc}</span>
          </div>
          <Toggle
            checked={settings.recordingCountdown}
            disabled={busy}
            ariaLabel={t().settingsCountdownTitle}
            onchange={setRecordingCountdown}
          />
        </div>

        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsFpsTitle}</span>
            <span class="row-desc">{t().settingsFpsDesc}</span>
          </div>
          <SegmentedControl
            value={settings.recordingFps}
            options={FPS_OPTIONS}
            onchange={setRecordingFps}
            ariaLabel={t().settingsFpsAria}
            disabled={busy}
          />
        </div>

        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsCursorTitle}</span>
            <span class="row-desc">{t().settingsCursorDesc}</span>
          </div>
          <Toggle
            checked={settings.showCursor}
            disabled={busy}
            ariaLabel={t().settingsCursorTitle}
            onchange={setShowCursor}
          />
        </div>

        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsMouseClicksTitle}</span>
            <span class="row-desc">{t().settingsMouseClicksDesc}</span>
          </div>
          <Toggle
            checked={settings.showMouseClicks}
            disabled={busy}
            ariaLabel={t().settingsMouseClicksTitle}
            onchange={setShowMouseClicks}
          />
        </div>

        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsCaptureAudioTitle}</span>
            <span class="row-desc">{t().settingsCaptureAudioDesc}</span>
          </div>
          <Toggle
            checked={settings.captureAudio}
            disabled={busy}
            ariaLabel={t().settingsCaptureAudioTitle}
            onchange={setCaptureAudio}
          />
        </div>

        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsCaptureMicTitle}</span>
            <span class="row-desc">{t().settingsCaptureMicDesc}</span>
          </div>
          <Toggle
            checked={settings.captureMic}
            disabled={busy}
            ariaLabel={t().settingsCaptureMicTitle}
            onchange={setCaptureMic}
          />
        </div>
      {:else if activeTab === "storage"}
        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsCaptureFolderTitle}</span>
            <span class="row-desc">
              {settings.captureFolder ?? fmt(t().settingsCaptureFolderDefault, { path: "%USERPROFILE%\\Pictures\\Clipo" })}
            </span>
          </div>
          <div class="actions">
            <Button size="sm" disabled={busy} onclick={pickCaptureFolder}>
              <FolderOpen size={14} />
              <span>{t().settingsCaptureFolderChoose}</span>
            </Button>
            {#if settings.captureFolder}
              <Button size="sm" iconOnly disabled={busy} onclick={resetCaptureFolder} title={t().settingsCaptureFolderResetTitle} ariaLabel={t().settingsCaptureFolderResetTitle}>
                <RotateCcw size={14} />
              </Button>
            {/if}
          </div>
        </div>

        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsImageFormatTitle}</span>
            <span class="row-desc">{t().settingsImageFormatDesc}</span>
          </div>
          <SegmentedControl
            value={settings.imageFormat}
            options={FORMAT_OPTIONS}
            onchange={setImageFormat}
            ariaLabel={t().settingsImageFormatTitle}
            disabled={busy}
          />
        </div>

        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsUploadServiceTitle}</span>
            <span class="row-desc">{t().settingsUploadServiceDesc}</span>
          </div>
          <SegmentedControl
            value={settings.uploadService}
            options={UPLOAD_SERVICES}
            onchange={setUploadService}
            ariaLabel={t().settingsUploadServiceAria}
            disabled={busy}
          />
        </div>
      {:else if activeTab === "shortcuts"}
        <div class="shortcuts-list">
          {#each regularDefs as def (def.id)}
            {@const combo = effectiveCombo(def)}
            {@const status = shortcutStatus.get(def.id) ?? "active"}
            <div class="row">
              <div class="row-text">
                <span class="row-title">{def.label}</span>
                <span class="row-desc">
                  {@html fmt(t().settingsShortcutsDefault, { combo: `<code>${formatCombo(def.defaultCombo)}</code>` })}
                </span>
              </div>
              <div class="actions">
                <span class="status" data-status={status} title={statusLabel(status)}>
                  {statusLabel(status)}
                </span>
                <KeyCaptureInput
                  value={combo}
                  disabled={busy}
                  onChange={(next) => setShortcut(def.id, next)}
                />
              </div>
            </div>
          {/each}

          {#if recordingDefs.length > 0}
            <!-- Recording-scoped group: registered with the OS only
                 while a session is live, otherwise dormant so the
                 combos don't pollute the user's normal keyboard. The
                 hint explains the `"Inactive"` chip below. -->
            <div class="shortcuts-group-head">
              <h3>{t().settingsShortcutsRecordingGroupTitle}</h3>
              <p>{t().settingsShortcutsRecordingGroupHint}</p>
            </div>
            {#each recordingDefs as def (def.id)}
              {@const combo = effectiveCombo(def)}
              {@const status = shortcutStatus.get(def.id) ?? "inactive"}
              <div class="row">
                <div class="row-text">
                  <span class="row-title">{def.label}</span>
                  <span class="row-desc">
                    {@html fmt(t().settingsShortcutsDefault, { combo: `<code>${formatCombo(def.defaultCombo)}</code>` })}
                  </span>
                </div>
                <div class="actions">
                  <span class="status" data-status={status} title={statusLabel(status)}>
                    {statusLabel(status)}
                  </span>
                  <KeyCaptureInput
                    value={combo}
                    disabled={busy}
                    onChange={(next) => setShortcut(def.id, next)}
                  />
                </div>
              </div>
            {/each}
          {/if}
        </div>
      {:else if activeTab === "about"}
        <div class="about-hero">
          <h3>Clipo</h3>
          <p class="about-tagline">{t().settingsAboutTagline}</p>
        </div>
        {#if buildInfo}
          <div class="about-meta">
            <span class="badge">v{buildInfo.version}</span>
            <span class="badge"><code>{buildInfo.commit}</code></span>
            <span class="badge">{buildInfo.commitDate}</span>
            {#if tauriVersion}
              <span class="badge">Tauri {tauriVersion}</span>
            {/if}
          </div>
        {/if}
        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsAboutUpdateTitle}</span>
            <span class="row-desc">
              {#if updatePhase === "checking"}{t().commonLoading}
              {:else if updatePhase === "uptodate"}{t().settingsAboutUpdateUpToDate}
              {:else if updatePhase === "available"}{fmt(t().settingsAboutUpdateAvailable, { version: updateVersion })}
              {:else if updatePhase === "downloading"}{fmt(t().settingsAboutUpdateDownloading, { pct: updatePct })}
              {:else if updatePhase === "error"}{updateError}
              {:else}{t().settingsAboutUpdateDesc}{/if}
            </span>
          </div>
          <div class="actions">
            {#if updatePhase === "available"}
              <Button size="sm" disabled={busy} onclick={installUpdate} title={t().settingsAboutUpdateInstall}>
                <span>{t().settingsAboutUpdateInstall}</span>
              </Button>
            {:else}
              <Button
                size="sm"
                disabled={updatePhase === "checking" || updatePhase === "downloading"}
                onclick={checkForUpdates}
                title={t().settingsAboutUpdateCheck}
                ariaLabel={t().settingsAboutUpdateCheck}
              >
                <RefreshCw size={14} />
                <span>{t().settingsAboutUpdateCheck}</span>
              </Button>
            {/if}
          </div>
        </div>
        <div class="row">
          <div class="row-text">
            <span class="row-title">{t().settingsAboutDiagnosticTitle}</span>
            <span class="row-desc">{t().settingsAboutDiagnosticDesc}</span>
          </div>
          <div class="actions">
            <Button
              size="sm"
              disabled={!buildInfo}
              onclick={copyDiagnostic}
              title={t().settingsAboutCopyInfoTitle}
              ariaLabel={t().settingsAboutCopyInfoTitle}
            >
              <Copy size={14} />
              <span>{t().settingsAboutCopyInfoLabel}</span>
            </Button>
          </div>
        </div>
        <div class="row danger">
          <div class="row-text">
            <span class="row-title">{t().settingsAboutResetTitle}</span>
            <span class="row-desc">{t().settingsAboutResetDesc}</span>
          </div>
          <div class="actions">
            <Button
              size="sm"
              disabled={busy}
              onclick={resetAllSettings}
              title={t().settingsAboutResetButtonTitle}
              ariaLabel={t().settingsAboutResetButtonTitle}
            >
              <RotateCcw size={14} />
              <span>{t().settingsAboutResetButton}</span>
            </Button>
          </div>
        </div>
      {/if}
    </div>
  </div>
</main>

<style>
  .root {
    height: 100vh;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .titlebar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    height: var(--titlebar-h);
    padding-left: var(--space-5);
    border-bottom: 1px solid var(--color-border-subtle);
    flex-shrink: 0;
  }
  .titlebar h1 {
    margin: 0;
    font-size: var(--text-md);
    font-weight: 600;
    color: var(--color-fg);
  }

  /* Sidebar + content body. Sidebar mirrors Windows 11 Settings /
   * macOS System Settings: icon + label, accent-tinted active state,
   * subtle hover. Pinned width keeps the content column predictable
   * regardless of label length. */
  .body {
    flex: 1 1 auto;
    display: flex;
    min-height: 0;
  }
  .sidebar {
    flex: 0 0 176px;
    display: flex;
    flex-direction: column;
    gap: 1px;
    padding: 10px 8px;
    border-right: 1px solid var(--color-border-subtle);
    background: var(--color-surface-0);
    overflow-y: auto;
  }
  .nav {
    display: inline-flex;
    align-items: center;
    gap: 10px;
    /* 36 px + 14 px label matches Windows 11 Settings sidebar
     * (Fluent 2 nav body). Was 32 px / 12 px which read smaller
     * than the content body and made the sidebar feel like a
     * "tray of chips" instead of "navigation list". 5 items at
     * 36 px = 180 px of nav, well under the 500 px content area. */
    height: 36px;
    padding: 0 10px;
    background: transparent;
    border: none;
    border-radius: var(--radius-sm);
    color: var(--color-fg-muted);
    font-family: inherit;
    font-size: var(--text-md);
    text-align: left;
    cursor: pointer;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
  }
  .nav :global(svg) { flex-shrink: 0; }
  .nav:hover:not(.active) {
    background: var(--color-surface-2);
    color: var(--color-fg);
  }
  .nav:focus-visible {
    outline: none;
    box-shadow: 0 0 0 2px var(--color-border-accent-strong);
  }
  .nav.active {
    background: var(--color-accent-bg-subtle);
    color: var(--color-accent-fg);
  }

  .content {
    flex: 1 1 auto;
    overflow-y: auto;
    padding: 16px 24px 24px;
    min-width: 0;
  }
  /* Big section title at the top — gives the active sidebar entry
   * an echo in the content area so the user has a clear "you are
   * here" signal once they scroll past the nav. */
  .section-title {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    margin-bottom: 4px;
    min-height: 28px;
  }
  .section-title h2 {
    margin: 0;
    font-size: var(--text-lg);
    font-weight: 600;
    color: var(--color-fg);
  }
  .error {
    margin: 0 0 16px;
    padding: 8px 12px;
    background: var(--color-danger-bg-subtle);
    border: 1px solid var(--color-border-danger);
    color: var(--color-danger-fg-soft);
    border-radius: var(--radius-sm);
    font-size: var(--text-sm);
  }
  /* Hairline divider between every row except the first, with
   * comfortable vertical padding so the description has room to
   * wrap. Mirrors macOS System Settings / Windows 11 Settings
   * grouping language — items in the same section read as a list,
   * not as floating cards. */
  .row {
    display: flex;
    align-items: center;
    gap: var(--space-4);
    padding: 14px 0;
    border-top: 1px solid var(--color-border-subtle);
  }
  .row:first-of-type {
    border-top: none;
    padding-top: 8px;
  }
  .row-text {
    display: flex;
    flex-direction: column;
    gap: 3px;
    flex: 1;
    min-width: 0;
  }
  .row-title {
    font-size: var(--text-md);
    font-weight: 500;
  }
  /* Descriptions wrap to as many lines as needed — truncation was
   * dropping useful context on the narrower default window width.
   * Sized to Fluent 2 Caption1 (12 px) — the same size Windows 11
   * Settings uses for the explanation under each row title. The
   * previous `--text-xs` (11 px) was a badge / chip-label size and
   * read as cramped at body-text length. */
  .row-desc {
    font-size: var(--text-sm);
    line-height: var(--leading-normal);
    color: var(--color-fg-muted);
  }
  .actions {
    display: inline-flex;
    gap: 6px;
    flex-shrink: 0;
  }

  /* Native <select> styled to match the rest of the form chrome — 12
   * languages is past the chip-segmented threshold (chips would wrap
   * to multiple rows; popovers inherit OS keyboard navigation for
   * free). Sized for the longest known label ("Português" / "한국어"
   * with name written in its own script). */
  .language-select {
    height: 30px;
    min-width: 160px;
    padding: 0 28px 0 10px;
    background: var(--color-surface-input);
    color: var(--color-fg);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius-sm);
    font-family: inherit;
    font-size: var(--text-sm);
    cursor: pointer;
    appearance: none;
    -webkit-appearance: none;
    background-image:
      linear-gradient(45deg, transparent 50%, var(--color-fg-muted) 50%),
      linear-gradient(135deg, var(--color-fg-muted) 50%, transparent 50%);
    background-position:
      right 12px center,
      right 7px center;
    background-size:
      5px 5px,
      5px 5px;
    background-repeat: no-repeat;
    transition: border-color var(--duration-quick) var(--ease-in-out-soft);
  }
  .language-select:hover:not(:disabled) {
    border-color: var(--color-border-strong);
  }
  .language-select:focus-visible {
    border-color: var(--color-border-accent);
  }
  .language-select:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  /* Status pill next to each hotkey row — green (active) / red
   * (conflict) / amber (invalid) / neutral (inactive). Pure surface
   * tinting; no border so the row layout stays light against the
   * row-desc text. */
  .status {
    display: inline-flex;
    align-items: center;
    height: 22px;
    padding: 0 8px;
    border-radius: var(--radius-full);
    font-size: var(--text-xs);
    font-weight: 500;
    flex-shrink: 0;
  }
  .status[data-status="active"] {
    background: var(--color-success-bg-subtle, rgba(34, 197, 94, 0.14));
    color: var(--color-success, #4ade80);
  }
  .status[data-status="conflict"] {
    background: var(--color-danger-bg-subtle);
    color: var(--color-danger-fg-soft);
  }
  .status[data-status="invalid"] {
    background: var(--color-warning-bg-subtle, rgba(245, 158, 11, 0.14));
    color: var(--color-warning, #f59e0b);
  }
  /* Recording-scoped combos sit here while no session is live — the
   * binding is parsed and stored, the OS hotkey is just not claimed.
   * Neutral tint reads as "configured, dormant", not "broken". */
  .status[data-status="inactive"] {
    background: var(--color-surface-2);
    color: var(--color-fg-muted);
  }

  /* `<code>` inside `.row-desc` for the default-combo hint. */
  .row-desc :global(code) {
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    background: var(--color-surface-2);
    padding: 1px 5px;
    border-radius: var(--radius-xs);
  }

  /* About / Sobre — hero block with the app name + tagline above the
   * spec rows. Keeps the visual rhythm of other tabs (title at top,
   * info rows below) while signalling "this is the identity page". */
  .about-hero {
    padding: 4px 0 12px;
  }
  .about-hero h3 {
    /* Fluent 2 Title2 (28 px) — one-off because the canonical
     * `--text-display` (112 px) is a timer-dial-only size, not a
     * general display token. Inlined to avoid growing the chrome
     * scale just for one branding moment. */
    margin: 0 0 4px;
    font-family: var(--font-display);
    font-size: 1.75rem;
    font-weight: 600;
    color: var(--color-fg);
    letter-spacing: -0.01em;
  }
  .about-tagline {
    margin: 0;
    font-size: var(--text-sm);
    color: var(--color-fg-muted);
  }

  /* Pill bar of build metadata — collapses the four stacked rows
   * (Versão / Commit / Compilado em / Tauri) into a single 24 px
   * tall strip. Read at a glance, copy via the button below. */
  .about-meta {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin: 4px 0 12px;
  }
  .badge {
    display: inline-flex;
    align-items: center;
    height: 24px;
    padding: 0 10px;
    background: var(--color-surface-1);
    color: var(--color-fg-muted);
    font-size: var(--text-xs);
    font-variant-numeric: tabular-nums;
    border-radius: var(--radius-full);
  }
  .badge :global(code) {
    background: transparent;
    padding: 0;
    color: var(--color-fg);
  }

  /* Atalhos list — rows already carry visual weight (status chip +
   * key-capture input), so the inter-row hairline that helps the
   * other sections read as a list would read as noise here. Wrap +
   * override drops the dividers without touching other tabs. */
  .shortcuts-list .row {
    border-top: none;
    padding: 8px 0;
  }

  /* Section break between the always-on shortcuts and the recording-
   * scoped group. A 1 px hairline + breathing room above gives the
   * "this is a different category" cue Windows 11 Settings uses for
   * subsections within a tab. The hint clarifies why the rows below
   * read as `"Inactive"` outside a recording. */
  .shortcuts-group-head {
    margin-top: var(--space-5);
    padding-top: var(--space-5);
    border-top: 1px solid var(--color-border-subtle);
  }
  .shortcuts-group-head h3 {
    margin: 0;
    font-size: var(--text-md);
    font-weight: 600;
    color: var(--color-fg);
  }
  .shortcuts-group-head p {
    margin: 4px 0 var(--space-2);
    font-size: var(--text-sm);
    line-height: var(--leading-normal);
    color: var(--color-fg-muted);
  }

  /* "Danger zone" row — destructive action gets a subtle red tint so
   * the user reads it as "different category" before hovering the
   * button. Matches the danger-fg-soft cue the Stop button on the
   * recording bar uses. */
  .row.danger .row-title {
    color: var(--color-danger-fg-soft);
  }
</style>
