<script lang="ts">
  /**
   * Post-capture actions panel.
   *
   * Lives in the pre-declared `actions` window (label fixed in
   * `tauri.conf.json`). The daemon shows the window after every
   * capture and emits `actions:show` with the payload — we listen
   * once on mount and keep the listener alive for the whole session.
   *
   * Dismiss = `setPosition(off-screen) + hide()`. Never `close()`:
   * destroying the WebView2 surface leaves a DWM-composited ghost
   * rectangle on screen until the app exits.
   */
  import { convertFileSrc, invoke } from "@tauri-apps/api/core";
  import { LogicalPosition } from "@tauri-apps/api/dpi";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { Video } from "@lucide/svelte";
  import { onDestroy, onMount } from "svelte";
  import CaptureActionsRow from "./CaptureActionsRow.svelte";
  import type { ActionId, UploadResult } from "./captureRegistry";
  import { initLocaleSync, t } from "../i18n/index.svelte";

  type SavedEvent = {
    path: string;
    filename: string;
    width: number;
    height: number;
    /** Sidecar JPEG path. Empty when the encode failed (rare) — the
     * panel falls back to the video glyph or no preview. Daemon
     * writes the sidecar with the same call History/Quick Access
     * read from, so the three surfaces share one disk artifact. */
    thumbnailPath: string;
    /** `"image"` for screenshots (default for legacy events without
     * the field) or `"video"` for recordings. Drives which action
     * set the panel renders and whether to show the thumbnail or
     * the video glyph in the preview tile. */
    kind?: "image" | "video" | "gif";
  };

  // Park the window far off any conceivable virtual desktop while
  // hidden so DWM never composites a stale frame in a visible area.
  const OFFSCREEN = new LogicalPosition(-30000, -30000);

  let current = $state<SavedEvent | null>(null);
  let hovering = $state(false);
  /** Any slow action (upload + GIF export) in flight. Pauses the
   * 5 s auto-dismiss so the panel stays visible while ffmpeg /
   * the upload host work. Resets via the `onBusyChange` callback
   * — covers both upload and GIF without separate plumbing. */
  let busy = $state(false);
  /** Auto-dismiss delay (ms), from Settings. Re-read each time the panel
   * appears so a change in Settings takes effect without an app restart. */
  let dismissMs = $state(5000);
  let dismissTimer: number | undefined;
  let unlistenShow: UnlistenFn | undefined;
  let unlistenLocale: UnlistenFn | undefined;

  // Action sets per capture kind. Image gets the full annotation /
  // OCR / clipboard pipeline; video gets the bare essentials — play
  // it and find the file. Upload is intentionally absent for video:
  // Catbox caps at 200 MB (~2 min of 2K@30 recording), 0x0.st caps
  // at 512 MB with retention scaling inversely to size — neither is
  // a sane default for arbitrary recordings, and silently failing
  // mid-upload is the worst UX for "share this". Re-add once we
  // have a video-aware host (Litterbox / chunked S3) and a pre-
  // upload size check. `delete` is intentionally absent everywhere:
  // it's a destructive action the user wants from a quiet review
  // surface (History grid / Lightbox), not from a 5 s auto-dismiss
  // toast where a slip is irrecoverable.
  const IMAGE_ACTIONS = ["edit", "ocr", "copy", "upload", "reveal"] as const satisfies readonly ActionId[];
  const VIDEO_ACTIONS = ["open", "gif", "reveal"] as const satisfies readonly ActionId[];
  /** GIF panel actions — same shape as the History/Lightbox sets but
   * without delete (the post-capture panel never offers delete on
   * any kind; it's a 5 s review surface). */
  const GIF_ACTIONS = ["open", "upload", "reveal"] as const satisfies readonly ActionId[];
  const panelActions = $derived<readonly ActionId[]>(
    current?.kind === "video"
      ? VIDEO_ACTIONS
      : current?.kind === "gif"
        ? GIF_ACTIONS
        : IMAGE_ACTIONS,
  );

  const win = getCurrentWindow();

  function scheduleDismiss() {
    clearTimeout(dismissTimer);
    // Pause auto-dismiss while the user is hovering OR a slow action
    // (upload, GIF export) is in flight — closing the panel
    // mid-request would leave the user wondering whether the action
    // succeeded.
    if (current && !hovering && !busy) {
      dismissTimer = window.setTimeout(dismiss, dismissMs);
    }
  }

  async function dismiss() {
    clearTimeout(dismissTimer);
    current = null;
    try {
      // Move off-screen BEFORE hide so we never flash an empty panel
      // in the bottom-right while WebView2 finishes painting.
      await win.setPosition(OFFSCREEN);
      await win.hide();
    } catch (e) {
      console.error("actions dismiss", e);
    }
  }

  function handleUploadResult(result: UploadResult) {
    if (result.kind === "done") void dismiss();
    // On error keep the panel open so the user can retry / close.
  }

  /** Show the capture immediately, then re-read the auto-dismiss delay
   * (user-configurable in Settings). Setting `current` first keeps the
   * panel's render off the IPC round-trip — the settings read lands a
   * few ms later and the `$effect` below reschedules the timer with the
   * fresh value, so the delay stays current without ever blocking the
   * panel from appearing. */
  async function showCapture(payload: SavedEvent) {
    current = payload;
    try {
      const s = await invoke<{ actionsDismissMs?: number }>("get_settings");
      if (typeof s.actionsDismissMs === "number") dismissMs = s.actionsDismissMs;
    } catch (e) {
      console.error("get_settings", e);
    }
  }

  function onKey(event: KeyboardEvent) {
    if (event.key === "Escape") void dismiss();
  }

  onMount(async () => {
    unlistenLocale = await initLocaleSync();
    // Primary path: the daemon emits this every capture. The window
    // was created at boot, so the listener has been attached long
    // before any emit lands — no race with WebView2 boot.
    unlistenShow = await listen<SavedEvent>("actions:show", (event) => {
      void showCapture(event.payload);
    });

    // Cold-start fallback: if the very first capture fires before the
    // mount completes, the daemon also stashes the payload in the
    // state map. Pulling here covers that window.
    try {
      const pending = await invoke<SavedEvent | null>("take_pending_capture", {
        label: win.label,
      });
      if (pending) void showCapture(pending);
    } catch (e) {
      console.error("take_pending_capture", e);
    }
  });

  // Auto-dismiss after 5 s, paused while the user hovers.
  $effect(() => {
    scheduleDismiss();
  });

  onDestroy(() => {
    clearTimeout(dismissTimer);
    unlistenShow?.();
    unlistenLocale?.();
  });
</script>

<svelte:window onkeydown={onKey} />

{#if current}
  <div
    class="panel"
    role="dialog"
    aria-label={t().postCaptureSavedAria}
    tabindex="-1"
    onmouseenter={() => (hovering = true)}
    onmouseleave={() => (hovering = false)}
  >
    <div class="thumb" class:video={current.kind === "video"}>
      {#if current.thumbnailPath}
        <img src={convertFileSrc(current.thumbnailPath)} alt={t().postCapturePreviewAlt} draggable="false" />
      {:else if current.kind === "video"}
        <!-- No sidecar produced (DXGI grab failed or encode error).
             A centred Video glyph matches the design language and
             reads as "this is a recording, not a screenshot". -->
        <Video size={42} strokeWidth={1.4} />
      {/if}
    </div>
    <div class="actions">
      <CaptureActionsRow
        entry={current}
        actions={panelActions}
        ghost
        iconSize={15}
        onActionComplete={() => void dismiss()}
        onBusyChange={(b) => (busy = b)}
        onUploadResult={handleUploadResult}
      />
    </div>
  </div>
{/if}

<style>
  .panel {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 8px;
    box-sizing: border-box;
    width: 100%;
    height: 100%;
    animation: fade-in var(--duration-slow) var(--ease-out-snappy);
  }

  .thumb {
    display: flex;
    align-items: center;
    justify-content: center;
    background: var(--color-surface-media);
    border-radius: var(--radius-md);
    width: 100%;
    height: 150px;
    overflow: hidden;
  }
  /* Video tile: centered glyph in muted fg, no image inside. */
  .thumb.video {
    color: var(--color-fg-muted);
  }

  .thumb img {
    max-width: 100%;
    max-height: 100%;
    display: block;
    object-fit: contain;
  }

  .actions {
    display: flex;
    gap: 6px;
    justify-content: center;
  }

  @keyframes fade-in {
    from {
      opacity: 0;
    }
    to {
      opacity: 1;
    }
  }
</style>
