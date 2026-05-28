<script lang="ts">
  /**
   * Custom tray menu — replaces the OS native popup with a webview
   * surface so we can use the project's design tokens, lucide icons,
   * keyboard shortcuts displayed inline, and section dividers.
   *
   * Lifecycle: shown by the daemon on tray click (position computed
   * Rust-side, biased above cursor on standard taskbar layouts).
   * Dismisses on `Esc`, on click outside (window blur), and after any
   * item dispatches its action (the daemon hides us as part of the
   * pick handler so the action's own UI surfaces aren't competing).
   *
   * Pre-painted at app startup so opens are <30 ms — the WebView2's
   * page is already mounted + last paint cached, `show()` just reveals.
   */
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import {
    AppWindow,
    Crop,
    FolderOpen,
    Image as ImageIcon,
    LayoutGrid,
    LogOut,
    Mic,
    MicOff,
    Monitor,
    MonitorPlay,
    Pause,
    Play,
    RotateCcw,
    ScanText,
    Settings,
    Square,
    Timer as TimerIcon,
    Trash2,
    Video,
    Volume2,
    VolumeX,
    Zap,
  } from "@lucide/svelte";
  import { onDestroy, onMount } from "svelte";
  import type { Component } from "svelte";
  import { SvelteMap } from "svelte/reactivity";
  import { formatCombo } from "../components/keyCombo";
  import { initLocaleSync, t } from "../i18n/index.svelte";

  type Item =
    | {
        kind: "item";
        id: string;
        icon: Component;
        label: string;
        /** `HOTKEY_DEFS::id` from the backend if this item has a
         * rebindable hotkey. The shortcut label is resolved from the
         * `activeShortcuts` map so it stays in sync with whatever the
         * user set in Settings. Items without a hotkey (Capture (3s),
         * Open folder, Quit, …) leave this undefined. */
        hotkeyId?: string;
        danger?: boolean;
        /** Recording-stop variant: red icon + label at rest. The
         * `danger` flag only tints on hover, which isn't enough for
         * a state-indicator. */
        recording?: boolean;
      }
    | { kind: "divider" };

  /** Default menu — every `id` matches `tray_menu_pick` in
   *  `clipo/src/lib.rs`, and every `hotkeyId` matches a `HOTKEY_DEFS`
   *  entry so the shortcut label can be resolved dynamically. Labels
   *  flow through `t()` so a language flip re-renders the rows in
   *  place without any teardown. */
  const defaultItems = $derived.by<Item[]>(() => [
    { kind: "item", id: "capture-region", icon: Crop, label: t().trayCaptureRegion, hotkeyId: "overlay" },
    { kind: "item", id: "capture-fullscreen", icon: Monitor, label: t().trayCaptureFullscreen, hotkeyId: "capture" },
    { kind: "item", id: "capture-window", icon: AppWindow, label: t().trayCaptureWindow },
    { kind: "item", id: "capture-timer", icon: TimerIcon, label: t().trayCaptureTimer },
    { kind: "item", id: "capture-ocr", icon: ScanText, label: t().trayCaptureOcr, hotkeyId: "ocr" },
    { kind: "divider" },
    { kind: "item", id: "capture-record-fullscreen", icon: MonitorPlay, label: t().trayRecordFullscreen, hotkeyId: "record-fullscreen" },
    { kind: "item", id: "capture-record", icon: Video, label: t().trayRecordRegion },
    { kind: "divider" },
    { kind: "item", id: "menu", icon: LayoutGrid, label: t().trayAllInOneMenu, hotkeyId: "menu" },
    { kind: "item", id: "quick", icon: Zap, label: t().trayQuickAccess, hotkeyId: "quick" },
    { kind: "divider" },
    { kind: "item", id: "history", icon: ImageIcon, label: t().trayHistory },
    { kind: "item", id: "open-folder", icon: FolderOpen, label: t().trayOpenFolder },
    { kind: "item", id: "settings", icon: Settings, label: t().traySettings },
    { kind: "divider" },
    { kind: "item", id: "quit", icon: LogOut, label: t().trayQuit, danger: true },
  ]);

  /** Live recording state — populated by `get_recording_state` on
   * each open and used to render the recording-mode items below.
   * The tray dismisses on every action, so we don't need event-based
   * sync; each open is a fresh snapshot. */
  type RecordingSnapshot = {
    active: boolean;
    paused: boolean;
    audio_muted: boolean;
    mic_muted: boolean;
    audio_enabled: boolean;
    mic_enabled: boolean;
  };

  let recState = $state<RecordingSnapshot>({
    active: false,
    paused: false,
    audio_muted: false,
    mic_muted: false,
    audio_enabled: false,
    mic_enabled: false,
  });

  /** Recording-only menu. Shown while a `VideoRecorder` is active so
   * the user doesn't keep clicking capture entries that the daemon
   * would reject (DXGI duplication is exclusive per output). Matches
   * the CleanShot X / Loom convention: stop as the primary action,
   * pause/restart/discard as secondary recording controls, mute
   * toggles only when their capture branch is enabled, then the
   * non-capture utilities. */
  const recordingItems = $derived.by<Item[]>(() => {
    const labels = t();
    // Each recording-state row also carries its `hotkeyId` so the kbd
    // glyph for the user's bound combo (Ctrl+Alt+S default, etc.)
    // shows next to the label — same convention the default menu
    // uses for the capture entries. Pause + Resume share the
    // `recording-pause` accelerator: it's a single toggle on the
    // backend (`dispatch_shortcut` flips based on snapshot state).
    // `recording-discard` has no hotkey on purpose — it's
    // destructive and the bar already requires a confirm; a global
    // combo for it would be too easy to mis-press.
    const out: Item[] = [
      { kind: "item", id: "recording-stop", icon: Square, label: labels.trayStopRecording, recording: true, hotkeyId: "recording-stop" },
      recState.paused
        ? { kind: "item", id: "recording-resume", icon: Play, label: labels.trayResumeRecording, hotkeyId: "recording-pause" }
        : { kind: "item", id: "recording-pause", icon: Pause, label: labels.trayPauseRecording, hotkeyId: "recording-pause" },
      { kind: "item", id: "recording-restart", icon: RotateCcw, label: labels.trayRestartRecording, hotkeyId: "recording-restart" },
      { kind: "item", id: "recording-discard", icon: Trash2, label: labels.trayDiscardRecording, danger: true },
    ];
    if (recState.audio_enabled || recState.mic_enabled) {
      out.push({ kind: "divider" });
      if (recState.audio_enabled) {
        out.push(recState.audio_muted
          ? { kind: "item", id: "recording-toggle-audio-mute", icon: Volume2, label: labels.trayUnmuteAudio, hotkeyId: "recording-mute-audio" }
          : { kind: "item", id: "recording-toggle-audio-mute", icon: VolumeX, label: labels.trayMuteAudio, hotkeyId: "recording-mute-audio" });
      }
      if (recState.mic_enabled) {
        out.push(recState.mic_muted
          ? { kind: "item", id: "recording-toggle-mic-mute", icon: Mic, label: labels.trayUnmuteMic, hotkeyId: "recording-mute-mic" }
          : { kind: "item", id: "recording-toggle-mic-mute", icon: MicOff, label: labels.trayMuteMic, hotkeyId: "recording-mute-mic" });
      }
    }
    out.push(
      { kind: "divider" },
      { kind: "item", id: "history", icon: ImageIcon, label: labels.trayHistory },
      { kind: "item", id: "open-folder", icon: FolderOpen, label: labels.trayOpenFolder },
      { kind: "item", id: "settings", icon: Settings, label: labels.traySettings },
      { kind: "divider" },
      { kind: "item", id: "quit", icon: LogOut, label: labels.trayQuit, danger: true },
    );
    return out;
  });

  const items = $derived(recState.active ? recordingItems : defaultItems);

  /** `HOTKEY_DEFS::id → tauri-accelerator string`. Fed by
   * `get_active_shortcuts` so the labels follow whatever the user set
   * in Settings → Atalhos. SvelteMap keeps the reactivity targeted —
   * only the rows whose hotkey actually changed re-render. */
  const activeShortcuts = new SvelteMap<string, string>();

  function shortcutLabel(item: Item): string | undefined {
    if (item.kind !== "item" || !item.hotkeyId) return undefined;
    const combo = activeShortcuts.get(item.hotkeyId);
    return combo ? formatCombo(combo) : undefined;
  }

  async function refreshRecordingState() {
    try {
      recState = await invoke<RecordingSnapshot>("get_recording_state");
    } catch (e) {
      console.warn("get_recording_state", e);
    }
  }

  async function refreshShortcuts() {
    try {
      const map = await invoke<Record<string, string>>("get_active_shortcuts");
      // Targeted diff so a single rebind only re-renders the row that
      // changed, not all 13 items.
      for (const id of [...activeShortcuts.keys()]) {
        if (!(id in map)) activeShortcuts.delete(id);
      }
      for (const [id, combo] of Object.entries(map)) {
        if (activeShortcuts.get(id) !== combo) activeShortcuts.set(id, combo);
      }
    } catch (e) {
      console.warn("get_active_shortcuts", e);
    }
  }

  let listEl = $state<HTMLUListElement | null>(null);

  function pick(id: string) {
    void invoke("tray_menu_pick", { action: id });
  }

  function dismiss() {
    void invoke("dismiss_tray_menu");
  }

  function onKey(e: KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      dismiss();
    } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      navigate(e.key === "ArrowDown" ? 1 : -1);
    }
  }

  function navigate(direction: 1 | -1) {
    if (!listEl) return;
    const buttons = Array.from(listEl.querySelectorAll<HTMLButtonElement>("button.item"));
    if (buttons.length === 0) return;
    const active = document.activeElement as HTMLElement | null;
    const currentIdx = buttons.findIndex((b) => b === active);
    const nextIdx =
      currentIdx === -1
        ? direction === 1 ? 0 : buttons.length - 1
        : (currentIdx + direction + buttons.length) % buttons.length;
    buttons[nextIdx]?.focus();
  }

  let unlistenBlur: UnlistenFn | undefined;
  let unlistenStarted: UnlistenFn | undefined;
  let unlistenStopped: UnlistenFn | undefined;
  let unlistenShortcuts: UnlistenFn | undefined;
  let unlistenLocale: UnlistenFn | undefined;

  onMount(async () => {
    // Don't auto-focus the first item — OS-native tray menus open with
    // nothing pre-selected; the first ↓ keypress focuses item 1 (handled
    // by `navigate` below). Auto-focusing made the first item read as
    // pre-selected, visually noisy.
    //
    // Window focus = tray was just shown; refresh both shortcut labels
    // and the recording snapshot so the menu reflects whatever the
    // user did since the last open (e.g. toggled mute from the bar).
    // Blur = clicked outside → dismiss; standard popup pattern.
    unlistenBlur = await getCurrentWindow().onFocusChanged(({ payload }) => {
      if (payload) {
        void refreshRecordingState();
        void refreshShortcuts();
      } else {
        dismiss();
      }
    });

    // Snapshot first, listeners second. If we registered listeners
    // before the invoke resolved, a `recording:stopped` event that
    // landed while the invoke was in flight would set the snapshot
    // inactive — then the stale active value from the resolved invoke
    // would overwrite it, leaving the UI out of sync. The tray window
    // is pre-painted at boot and never remounts, so the listeners
    // catch every later cycle.
    unlistenLocale = await initLocaleSync();
    await refreshRecordingState();
    await refreshShortcuts();
    unlistenStarted = await listen("recording:started", () => {
      void refreshRecordingState();
    });
    unlistenStopped = await listen("recording:stopped", () => {
      void refreshRecordingState();
    });
    // Settings → Atalhos commits trigger this; the labels follow without
    // the user having to close + reopen the tray.
    unlistenShortcuts = await listen("shortcuts:updated", () => {
      void refreshShortcuts();
    });
  });

  onDestroy(() => {
    unlistenBlur?.();
    unlistenStarted?.();
    unlistenStopped?.();
    unlistenShortcuts?.();
    unlistenLocale?.();
  });
</script>

<svelte:window onkeydown={onKey} />

<ul class="list" bind:this={listEl} role="menu">
  {#each items as item, i (i)}
    {#if item.kind === "divider"}
      <li class="divider" role="separator"></li>
    {:else}
      {@const Icon = item.icon}
      {@const shortcut = shortcutLabel(item)}
      <li role="none">
        <button
          type="button"
          class="item"
          class:danger={item.danger}
          class:recording={item.recording}
          role="menuitem"
          onclick={() => pick(item.id)}
        >
          <span class="icon" aria-hidden="true">
            {#if item.recording}
              <!-- Filled square reads as "stop" universally —
                   strokeWidth=0 forces the solid fill. -->
              <Icon size={11} fill="currentColor" strokeWidth={0} />
            {:else}
              <Icon size={15} strokeWidth={1.8} />
            {/if}
          </span>
          <span class="label">{item.label}</span>
          {#if shortcut}
            <span class="shortcut">{shortcut}</span>
          {/if}
        </button>
      </li>
    {/if}
  {/each}
</ul>

<style>
  :global(html), :global(body) {
    margin: 0;
    padding: 0;
    background: var(--color-surface-1);
    color: var(--color-fg);
    overflow: hidden;
    user-select: none;
    -webkit-user-select: none;
    box-shadow: inset 0 0 0 1px var(--color-border-subtle);
  }

  :global(button) {
    appearance: none;
    -webkit-appearance: none;
    background: transparent;
    border: none;
    outline: none;
    -webkit-tap-highlight-color: transparent;
    font: inherit;
    color: inherit;
    cursor: pointer;
  }
  /* `:focus` (mouse) is suppressed so click doesn't leave a stuck
   * ring; `:focus-visible` (keyboard) gets the app-level ring from
   * `app.css` — critical for ↑/↓ navigation to be visible. The
   * previous version killed both and the keyboard user got no
   * indicator of which item was focused. */
  :global(button:focus) {
    outline: none;
    box-shadow: none;
  }

  .list {
    list-style: none;
    margin: 0;
    padding: 6px;
    display: flex;
    flex-direction: column;
    gap: 1px;
  }

  .item {
    display: flex;
    align-items: center;
    width: 100%;
    height: 32px;
    padding: 0 10px;
    gap: 10px;
    border-radius: var(--radius-sm);
    text-align: left;
    color: var(--color-fg);
    font-size: var(--text-md);
    transition: background-color var(--duration-quick) var(--ease-in-out-soft);
  }
  .item:hover, .item:focus-visible { background: var(--color-surface-2); }
  .item:active { background: var(--color-surface-3); }

  .icon {
    display: inline-flex;
    color: var(--color-fg-muted);
    flex: 0 0 auto;
    width: 16px;
    height: 16px;
    align-items: center;
    justify-content: center;
  }
  .item:hover .icon, .item:focus-visible .icon { color: var(--color-fg); }

  .label {
    flex: 1 1 auto;
    line-height: var(--leading-normal);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .shortcut {
    flex: 0 0 auto;
    color: var(--color-fg-subtle);
    font-size: var(--text-xs);
    font-family: var(--font-mono);
    letter-spacing: 0.01em;
  }

  .divider {
    height: 1px;
    margin: 5px 4px;
    background: var(--color-border-subtle);
    list-style: none;
  }

  /* Quit = destructive intent; tint the label red on hover. */
  .item.danger .label { color: var(--color-fg); }
  .item.danger:hover .label, .item.danger:focus-visible .label {
    color: var(--color-danger-hover);
  }
  .item.danger:hover .icon, .item.danger:focus-visible .icon {
    color: var(--color-danger-hover);
  }

  /* Recording-stop = active state indicator. Always-on red icon +
   * label so the user reads it as the obvious primary action at
   * rest, not just on hover. Matches the recording-border colour
   * for a coherent "you're in recording mode" cue. */
  .item.recording .icon { color: var(--color-danger); }
  .item.recording .label { color: var(--color-danger); }
  .item.recording:hover .icon, .item.recording:focus-visible .icon {
    color: var(--color-danger-hover);
  }
  .item.recording:hover .label, .item.recording:focus-visible .label {
    color: var(--color-danger-hover);
  }
</style>
