<script lang="ts">
  /**
   * Self-timer countdown.
   *
   * Pre-declared `timer` window: opaque dial, centered on the primary
   * monitor, always-on-top. Listens for `timer:start` (the daemon
   * shows the window then emits start with the user-configured
   * duration as payload), counts down to zero, and invokes
   * `timer_complete` — the daemon hides the window and dispatches the
   * pending pipeline (fullscreen capture or pre-recording arm).
   *
   * Dismiss = ESC. The window stays alive for the next run; we never
   * destroy it (destroy-and-recreate is what leaves DWM ghost frames).
   */
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { onDestroy, onMount } from "svelte";
  import { dismissOffscreen } from "../dismissOffscreen";
  import { initLocaleSync, t } from "../i18n/index.svelte";

  /** Fallback for legacy null payload. Matches Rust default_timer_seconds(). */
  const FALLBACK_SECONDS = 3;

  let count = $state<number | null>(null);
  let tickHandle: number | undefined;
  let unlisten: UnlistenFn | undefined;
  let unlistenLocale: UnlistenFn | undefined;

  const win = getCurrentWindow();

  function tick() {
    if (count === null) return;
    count -= 1;
    if (count <= 0) {
      clearTimeout(tickHandle);
      void complete();
    } else {
      tickHandle = window.setTimeout(tick, 1000);
    }
  }

  function start(seconds: number) {
    clearTimeout(tickHandle);
    // Guard against zero/negative or absurd input — the daemon
    // shouldn't emit those, but the UI shouldn't soft-lock on a bad
    // value either. Clamp into a sane (positive, ≤ 60 s) range.
    const safe =
      Number.isFinite(seconds) && seconds > 0
        ? Math.min(Math.floor(seconds), 60)
        : FALLBACK_SECONDS;
    count = safe;
    tickHandle = window.setTimeout(tick, 1000);
  }

  async function complete() {
    count = null;
    try {
      await invoke("timer_complete");
    } catch (e) {
      console.error("timer_complete", e);
      await dismissOffscreen(win);
    }
  }

  function onKey(event: KeyboardEvent) {
    if (event.key === "Escape") {
      clearTimeout(tickHandle);
      count = null;
      void dismissOffscreen(win);
    }
  }

  onMount(async () => {
    unlistenLocale = await initLocaleSync();
    unlisten = await listen<number | null | undefined>("timer:start", (event) => {
      start(event.payload ?? FALLBACK_SECONDS);
    });
  });

  onDestroy(() => {
    clearTimeout(tickHandle);
    unlisten?.();
    unlistenLocale?.();
  });
</script>

<svelte:window onkeydown={onKey} />

{#if count !== null}
  <div class="dial" role="status" aria-live="polite">
    <span class="number">{count}</span>
    <span class="hint">{t().commonEscToCancel}</span>
  </div>
{/if}

<style>
  .dial {
    width: 100vw;
    height: 100vh;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 10px;
    color: var(--color-fg);
    font-family: var(--font-display);
  }

  .number {
    font-size: var(--text-display);
    font-weight: 600;
    line-height: 1;
    color: #fff;
    text-shadow: 0 6px 22px rgb(0 0 0 / 0.5);
    animation: pop 1s var(--ease-out-quint);
  }

  .hint {
    font-size: var(--text-xs);
    letter-spacing: 0.04em;
    color: var(--color-fg-muted);
    text-transform: uppercase;
  }

  @keyframes pop {
    0% {
      transform: scale(0.65);
      opacity: 0.2;
    }
    25% {
      transform: scale(1.08);
      opacity: 1;
    }
    100% {
      transform: scale(1);
      opacity: 1;
    }
  }
</style>
