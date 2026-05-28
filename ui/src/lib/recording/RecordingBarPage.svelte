<script lang="ts">
  /**
   * Recording bar — slim opaque pill that surfaces during a region
   * recording. Layout (left → right):
   *
   *   [■ Stop  00:42]   ·   [⏸]  [🔊]
   *
   * - Stop + timer are paired on the left as "primary action + status"
   *   (mirrors macOS native recording menu bar / Loom's stop-first
   *   pattern). Stop is the only filled control; timer reads as the
   *   live status string next to it.
   * - Pause / audio mute are ghost icon buttons on the right
   *   (subtle, hover surfaces).
   * - The empty middle is the drag region.
   *
   * # Why opaque (not translucent)
   *
   * `transparent + backdrop-filter` forces a DWM redirection surface
   * + per-frame alpha-blend. Slower than opaque with no visual win at
   * this size. CleanShot / Loom / Icecream all ship opaque bars.
   *
   * # Drag-region trap
   *
   * `-webkit-app-region: drag` body-wide makes WebView2 swallow the
   * first click on any child as a drag intent. Drag is scoped to the
   * left strip; every button is explicitly `no-drag`.
   *
   * # Webview reuse
   *
   * Tauri keeps this webview mounted across show/hide cycles. State
   * (`elapsedMs`, `stopping`, `paused`, `muted`) is reset on every
   * `recording:started` so the second recording doesn't inherit the
   * first's stale flags.
   *
   * # Audio mute
   *
   * Two buttons — system-audio loopback and microphone — each mutes its
   * own track for the session via `set_audio_muted` / `set_mic_muted`.
   * A button shows only when its track was enabled in Settings at record
   * time (`audio_enabled` / `mic_enabled` from the snapshot); muting a
   * track that was never captured is meaningless. Backend is the source
   * of truth: local flags flip only after the snapshot / event lands so
   * a failed IPC can't desync the UI.
   */
  import { invoke } from "@tauri-apps/api/core";
  import type { UnlistenFn } from "@tauri-apps/api/event";
  import { listen } from "@tauri-apps/api/event";
  import { onDestroy, onMount } from "svelte";
  import { Mic, MicOff, Pause, Play, RotateCcw, Square, Trash2, Volume2, VolumeX } from "@lucide/svelte";
  import { formatCombo } from "../components/keyCombo";
  import { initLocaleSync, t } from "../i18n/index.svelte";

  let elapsedMs = $state(0);
  let stopping = $state(false);
  let restarting = $state(false);
  let paused = $state(false);
  let muted = $state(false);
  let micMuted = $state(false);
  // Per-track enable flags from the snapshot — a mute button shows only
  // when its track was enabled in Settings at record time. Hidden until
  // the snapshot lands.
  let audioEnabled = $state(false);
  let micEnabled = $state(false);
  let started = $state(false);
  let intervalId: ReturnType<typeof setInterval> | null = null;
  // Walltime baseline; advances real-time but freezes during pause so
  // the displayed timer matches the MP4's effective recording length
  // (which also skips the pause via VideoRecorder::pause).
  let t0 = $state(performance.now());
  let pausedAt = $state<number | null>(null);
  /** `HOTKEY_DEFS::id → tauri-accelerator string` for the global
   * recording shortcuts (Ctrl+Alt+S/P/E/M/I by default). Used to
   * append the canonical combo into each button's tooltip alongside
   * the bar-local key (Esc/Space/M/I/R) — same key resolution path
   * the tray uses, so tooltip and tray never disagree. */
  let activeShortcuts = $state<Record<string, string>>({});
  let unlistenStart: UnlistenFn | undefined;
  let unlistenStateChanged: UnlistenFn | undefined;
  let unlistenLocale: UnlistenFn | undefined;
  let unlistenShortcuts: UnlistenFn | undefined;

  type RecordingState = {
    active: boolean;
    paused: boolean;
    audio_muted: boolean;
    mic_muted: boolean;
    /** Whether each track was enabled in Settings at record time. */
    audio_enabled: boolean;
    mic_enabled: boolean;
  };

  /** Apply a backend snapshot to the bar's local state. Owning the
   * pausedAt / t0 reconciliation here (instead of inside togglePause)
   * means flips from the tray or any other surface land correctly. */
  function applyStateChange(s: RecordingState) {
    if (s.paused !== paused) {
      if (s.paused) {
        pausedAt = performance.now();
      } else if (pausedAt !== null) {
        t0 += performance.now() - pausedAt;
        pausedAt = null;
      }
      paused = s.paused;
    }
    muted = s.audio_muted;
    micMuted = s.mic_muted;
    audioEnabled = s.audio_enabled;
    micEnabled = s.mic_enabled;
  }

  /** Pull the full snapshot. Needed on `recording:started`: the
   * `recording:state-changed` event only fires on a flip, so the
   * enabled flags wouldn't arrive until the user toggled something. */
  async function refreshState() {
    try {
      const s = await invoke<RecordingState>("get_recording_state");
      if (s.active) applyStateChange(s);
    } catch (e) {
      console.error("get_recording_state", e);
    }
  }

  const elapsedLabel = $derived(formatElapsed(elapsedMs));

  function formatElapsed(ms: number): string {
    const totalSec = Math.floor(ms / 1000);
    const m = Math.floor(totalSec / 60);
    const s = totalSec % 60;
    return `${m.toString().padStart(2, "0")}:${s.toString().padStart(2, "0")}`;
  }

  async function stop() {
    if (stopping) return;
    stopping = true;
    try {
      await invoke("stop_recording");
    } catch (e) {
      console.error("stop_recording", e);
      stopping = false;
    }
  }

  async function discard() {
    if (stopping) return;
    // Destructive — the file goes to /dev/null without surfacing in
    // History or actions panel. Native confirm is the lightest UX
    // guard against accidental clicks (the trash icon is small + sits
    // next to Pause / Mute, easy to mis-tap).
    if (!window.confirm(t().barDiscardConfirm)) return;
    stopping = true;
    try {
      await invoke("discard_recording");
    } catch (e) {
      console.error("discard_recording", e);
      stopping = false;
    }
  }

  async function restart() {
    if (stopping || restarting) return;
    // Single round-trip: backend discards the current MP4 and arms a
    // new VideoRecorder over the same rect, firing `recording:started`
    // when the new pipeline is up. `resetAndStart` (driven by that
    // event) clears the timer + pause/mute flags + `restarting` flag.
    restarting = true;
    try {
      await invoke("restart_recording");
    } catch (e) {
      console.error("restart_recording", e);
      restarting = false;
    }
  }

  // Mute / pause toggles fire-and-forget the invoke; the local
  // `paused` / `muted` / `micMuted` flags update via the backend's
  // `recording:state-changed` event so the bar stays in sync when the
  // tray menu (or any other surface) flips the same flag.
  async function togglePause() {
    if (stopping) return;
    try {
      await invoke(paused ? "resume_recording" : "pause_recording");
    } catch (e) {
      console.error("pause/resume", e);
    }
  }

  async function toggleMute() {
    if (stopping) return;
    try {
      await invoke("set_audio_muted", { muted: !muted });
    } catch (e) {
      console.error("set_audio_muted", e);
    }
  }

  async function toggleMicMute() {
    if (stopping) return;
    try {
      await invoke("set_mic_muted", { muted: !micMuted });
    } catch (e) {
      console.error("set_mic_muted", e);
    }
  }

  function startTimer() {
    t0 = performance.now();
    pausedAt = null;
    intervalId = setInterval(() => {
      if (paused) return;
      elapsedMs = performance.now() - t0;
    }, 200);
    started = true;
  }

  function resetAndStart() {
    if (intervalId !== null) {
      clearInterval(intervalId);
      intervalId = null;
    }
    elapsedMs = 0;
    stopping = false;
    restarting = false;
    paused = false;
    muted = false;
    micMuted = false;
    started = false;
    pausedAt = null;
    startTimer();
  }

  /** Pull the latest global hotkey bindings — same `get_active_shortcuts`
   * the tray menu uses, so the bar tooltips and the tray rows never
   * disagree on which combo runs an action. Re-fetched on every
   * `shortcuts:updated` (which fires on rebind AND on the recording
   * lifecycle that toggles the scoped hotkeys between active/inactive). */
  async function refreshShortcuts() {
    try {
      activeShortcuts = await invoke<Record<string, string>>("get_active_shortcuts");
    } catch (e) {
      console.warn("get_active_shortcuts", e);
    }
  }

  /** Append the user's bound global hotkey to a base tooltip — e.g.
   * "Pause" + `recording-pause` (default F9) → "Pause (F9)". When the
   * hotkey id is `null` (the discard button, deliberately keyless)
   * or no combo is bound, the label passes through unchanged. */
  function withCombo(label: string, hotkeyId: string | null): string {
    if (!hotkeyId) return label;
    const raw = activeShortcuts[hotkeyId];
    if (!raw) return label;
    return `${label} (${formatCombo(raw)})`;
  }

  onMount(async () => {
    unlistenLocale = await initLocaleSync();
    unlistenStart = await listen("recording:started", () => {
      resetAndStart();
      void refreshState();
    });
    unlistenStateChanged = await listen<RecordingState>(
      "recording:state-changed",
      (event) => applyStateChange(event.payload),
    );
    unlistenShortcuts = await listen("shortcuts:updated", () => {
      void refreshShortcuts();
    });
    void refreshShortcuts();
    setTimeout(() => {
      if (!started) startTimer();
    }, 500);
  });

  onDestroy(() => {
    if (intervalId !== null) clearInterval(intervalId);
    unlistenStart?.();
    unlistenStateChanged?.();
    unlistenLocale?.();
    unlistenShortcuts?.();
  });
</script>

<div class="bar">
  <button
    type="button"
    class="stop"
    onclick={stop}
    disabled={stopping}
    aria-label={t().barStopAria}
    title={withCombo(t().barStopTitle, "recording-stop")}
  >
    <Square size={10} fill="currentColor" strokeWidth={0} />
    <span class="time" aria-live="polite">{elapsedLabel}</span>
  </button>

  <div class="drag" aria-hidden="true"></div>

  <div class="actions">
    <button
      type="button"
      class="ghost"
      onclick={togglePause}
      disabled={stopping}
      aria-label={paused ? t().barResumeAria : t().barPauseAria}
      title={withCombo(paused ? t().barResumeTitle : t().barPauseTitle, "recording-pause")}
    >
      {#if paused}
        <Play size={14} strokeWidth={2.2} />
      {:else}
        <Pause size={14} strokeWidth={2.2} />
      {/if}
    </button>

    {#if audioEnabled}
      <button
        type="button"
        class="ghost"
        class:muted
        onclick={toggleMute}
        disabled={stopping}
        aria-label={muted ? t().barUnmuteSystemAria : t().barMuteSystemAria}
        title={withCombo(muted ? t().barUnmuteSystemTitle : t().barMuteSystemTitle, "recording-mute-audio")}
      >
        {#if muted}
          <VolumeX size={14} strokeWidth={2.2} />
        {:else}
          <Volume2 size={14} strokeWidth={2.2} />
        {/if}
      </button>
    {/if}

    {#if micEnabled}
      <button
        type="button"
        class="ghost"
        class:muted={micMuted}
        onclick={toggleMicMute}
        disabled={stopping}
        aria-label={micMuted ? t().barUnmuteMicAria : t().barMuteMicAria}
        title={withCombo(micMuted ? t().barUnmuteMicTitle : t().barMuteMicTitle, "recording-mute-mic")}
      >
        {#if micMuted}
          <MicOff size={14} strokeWidth={2.2} />
        {:else}
          <Mic size={14} strokeWidth={2.2} />
        {/if}
      </button>
    {/if}

    <button
      type="button"
      class="ghost"
      onclick={restart}
      disabled={stopping || restarting}
      aria-label={t().barRestartAria}
      title={withCombo(t().barRestartTitle, "recording-restart")}
    >
      <RotateCcw size={14} strokeWidth={2.2} />
    </button>

    <button
      type="button"
      class="ghost discard"
      onclick={discard}
      disabled={stopping}
      aria-label={t().barDiscardAria}
      title={t().barDiscardTitle}
    >
      <Trash2 size={14} strokeWidth={2.2} />
    </button>
  </div>
</div>

<style>
  :global(html), :global(body) {
    margin: 0;
    padding: 0;
    /* Same chrome as the post-capture actions panel: `surface-0` bg,
     * no inset border, DWM shadow on (set in `tauri.conf.json`).
     * Trade-off vs. the previous border-only setup: the DWM shadow
     * bleeds ~15-30 px into the recorded MP4 when the bar sits
     * adjacent to the rect (see commit a105819). Toggle back to
     * `shadow: false` + inset border if the bleed shows up. */
    background: var(--color-surface-0);
    color: var(--color-fg);
    overflow: hidden;
    user-select: none;
    -webkit-user-select: none;
  }

  /* Bar buttons are visually minimal but they're still real buttons —
   * `:focus-visible` from `app.css` provides the keyboard ring. The
   * previous `outline: none` here applied to *every* state and broke
   * keyboard nav inside the bar (Tab through Pause / Mute / Stop
   * showed no indicator at all). */
  :global(button) {
    appearance: none;
    -webkit-appearance: none;
    background: transparent;
    border: none;
    font: inherit;
    color: inherit;
    cursor: pointer;
    -webkit-tap-highlight-color: transparent;
  }
  :global(button:focus) {
    outline: none;
  }

  .bar {
    height: 100vh;
    width: 100vw;
    display: flex;
    align-items: center;
    /* 12 px horizontal padding so the leftmost Stop pill and the
     * rightmost ghost don't kiss the window edge. Previous 6 px
     * read as "too close to the border". */
    padding: 0 12px;
    gap: 4px;
    box-sizing: border-box;
  }

  /* Empty middle strip takes the leftover space and is the drag
     surface (macOS / Windows titlebar pattern: action on one side,
     controls on the other, draggable gap between). */
  .drag {
    flex: 1 1 auto;
    align-self: stretch;
    -webkit-app-region: drag;
  }

  .time {
    font-family: var(--font-mono);
    font-size: var(--text-sm);
    font-weight: 600;
    letter-spacing: 0.01em;
    font-variant-numeric: tabular-nums;
    line-height: 1;
  }

  /* Right cluster: pause + mic. Sits flush right, balancing the
     Stop+timer pill on the left. */
  .actions {
    flex: 0 0 auto;
    display: flex;
    align-items: center;
    gap: 2px;
  }

  .ghost {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 26px;
    height: 26px;
    border-radius: var(--radius-sm);
    color: var(--color-fg-muted);
    -webkit-app-region: no-drag;
    transition:
      background-color var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
  }
  .ghost:hover:not(:disabled) {
    background: var(--color-surface-2);
    color: var(--color-fg);
  }
  .ghost:active:not(:disabled) { background: var(--color-surface-3); }
  .ghost:disabled { color: var(--color-fg-disabled); cursor: default; }
  /* Muted state on the mic — tint to danger so it reads as "off /
     attention" without becoming noisy. */
  .ghost.muted { color: var(--color-danger); }
  .ghost.muted:hover:not(:disabled) {
    background: var(--color-danger-bg-subtle);
    color: var(--color-danger-hover);
  }
  /* Discard (trash) — quiet at rest like the other ghosts, escalates
   * to red on hover so the destructive intent only surfaces under
   * cursor focus (the explicit confirm dialog is the real safety). */
  .ghost.discard:hover:not(:disabled) {
    background: var(--color-danger-bg-subtle);
    color: var(--color-danger);
  }

  /* Stop+timer pill: primary destructive action carries the live
     timer as its label. Pairing them = "what's happening + how to
     end it" in a single glance target (macOS recording menu bar +
     Loom both put stop adjacent to the duration). */
  .stop {
    display: inline-flex;
    align-items: center;
    gap: 7px;
    height: 28px;
    padding: 0 12px 0 10px;
    border-radius: var(--radius-sm);
    background: var(--color-danger-bg-strong);
    color: var(--color-danger-fg-soft);
    -webkit-app-region: no-drag;
    flex: 0 0 auto;
    transition:
      background-color var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
  }
  .stop:hover:not(:disabled) {
    background: var(--color-danger);
    color: #fff;
  }
  .stop:active:not(:disabled) {
    background: var(--color-danger-pressed);
    color: #fff;
  }
  .stop:disabled {
    background: var(--color-surface-3);
    color: var(--color-fg-disabled);
    cursor: default;
  }
  .stop .time { color: inherit; }
</style>
