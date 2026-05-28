<script lang="ts">
  /**
   * Lightbox — full-screen image viewer for the history grid.
   *
   * Replaces "click thumb → open in system viewer" with an in-app
   * preview that owns the screen: image hero centered, ghost
   * icon-only toolbar at the top with the same actions as the hover
   * overlay, keyboard navigation (← → / Esc), backdrop click closes.
   *
   * Parent owns:
   *  - the `entries` list currently shown (so navigation respects the
   *    active date filter)
   *  - the index of the active entry (so it stays in sync if entries
   *    mutate — e.g. after deleting from inside the lightbox)
   *  - the action callbacks (already implemented in HistoryPage)
   *
   * Lightbox owns:
   *  - the keyboard listener
   *  - the open animation
   *  - backdrop click → close
   */
  import { convertFileSrc } from "@tauri-apps/api/core";
  import { ChevronLeft, ChevronRight, X } from "@lucide/svelte";
  import CaptureActionsRow from "../actions/CaptureActionsRow.svelte";
  import UploadToast from "../actions/UploadToast.svelte";
  import type { ActionId, UploadResult } from "../actions/captureRegistry";
  import { t } from "../i18n/index.svelte";

  type CaptureEntry = {
    path: string;
    filename: string;
    modifiedMs: number;
    kind: "image" | "video" | "gif";
    thumbnailPath?: string;
  };

  type Props = {
    entries: CaptureEntry[];
    index: number;
    /** Toast state owned by the parent (HistoryPage) so the grid +
     * lightbox share a single feedback slot. Lightbox renders its
     * own copy of <UploadToast> inside its overlay so the message
     * survives behind the dim backdrop. */
    uploadResult: UploadResult;
    /** path → URL map of already-uploaded captures. Lightbox just
     * looks up the current entry and hands the result to the row. */
    uploadedUrls: ReadonlyMap<string, string>;
    onClose: () => void;
    onIndexChange: (i: number) => void;
    /** Bubbled from the shared CaptureActionsRow up to the parent so
     * the toast state + URL cache stay single-sourced. */
    onUploadResult: (result: UploadResult, path: string) => void;
  };

  let {
    entries,
    index,
    uploadResult,
    uploadedUrls,
    onClose,
    onIndexChange,
    onUploadResult,
  }: Props = $props();

  const LIGHTBOX_ACTIONS_IMAGE = [
    "edit", "ocr", "copy", "upload", "reveal", "delete",
  ] as const satisfies readonly ActionId[];
  /** Video action set: drop edit/ocr/copy/upload (none apply to MP4
   * — see PostCaptureActions for the upload size-cap reasoning).
   * `open` lets the user fall back to the system video player. */
  const LIGHTBOX_ACTIONS_VIDEO = [
    "open", "gif", "reveal", "delete",
  ] as const satisfies readonly ActionId[];
  /** GIF action set: same upload/reveal/delete trio as video plus
   * `open` (system viewer animates it correctly). edit/ocr/copy drop
   * for the same reason as the History grid — they'd flatten the
   * animation to a single frame. */
  const LIGHTBOX_ACTIONS_GIF = [
    "open", "upload", "reveal", "delete",
  ] as const satisfies readonly ActionId[];

  function actionsFor(kind: "image" | "video" | "gif"): readonly ActionId[] {
    if (kind === "video") return LIGHTBOX_ACTIONS_VIDEO;
    if (kind === "gif") return LIGHTBOX_ACTIONS_GIF;
    return LIGHTBOX_ACTIONS_IMAGE;
  }

  const entry = $derived(entries[index]);
  const canPrev = $derived(index > 0);
  const canNext = $derived(index < entries.length - 1);

  function prev() {
    if (canPrev) onIndexChange(index - 1);
  }
  function next() {
    if (canNext) onIndexChange(index + 1);
  }

  function onKey(ev: KeyboardEvent) {
    if (ev.key === "Escape") {
      ev.preventDefault();
      onClose();
    } else if (ev.key === "ArrowLeft") {
      ev.preventDefault();
      prev();
    } else if (ev.key === "ArrowRight") {
      ev.preventDefault();
      next();
    }
  }

  $effect(() => {
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });
</script>

{#if entry}
  <div class="lb" role="dialog" aria-modal="true" aria-label={t().lightboxAria}>
    <!-- Backdrop click closes. Visual layer only — the visible Close
         button (X) is the proper close affordance for assistive tech;
         this stays out of the tab order and aria-hidden. -->
    <button
      type="button"
      class="lb-backdrop"
      onclick={onClose}
      tabindex="-1"
      aria-hidden="true"
    ></button>

    <header class="lb-bar">
      <div class="lb-meta">
        <span class="lb-count">{index + 1} / {entries.length}</span>
        <span class="lb-name" title={entry.path}>{entry.filename}</span>
      </div>
      <div class="lb-actions">
        <CaptureActionsRow
          {entry}
          actions={actionsFor(entry.kind)}
          ghost
          cachedUrl={uploadedUrls.get(entry.path)}
          {onUploadResult}
        />
      </div>
      <button type="button" class="lb-close" onclick={onClose} aria-label={t().commonClose} title={t().commonCloseEsc}>
        <X size={18} />
      </button>
    </header>

    <div class="lb-stage">
      {#if canPrev}
        <button
          type="button"
          class="lb-nav prev"
          onclick={prev}
          aria-label={t().lightboxPrevAria}
          title={t().lightboxPrevTitle}
        ><ChevronLeft size={28} /></button>
      {/if}
      <!-- key=path so Svelte tears down + remounts on navigation,
           re-running the zoom-in animation per slide. Critical for
           video: without the remount, the same `<video>` element
           keeps the previous source loaded, doesn't auto-play the
           new one, and (worse) holds an MF source reader pipeline
           alive across navigations. -->
      {#key entry.path}
        {#if entry.kind === "video"}
          <!-- svelte-ignore a11y_media_has_caption -->
          <video
            class="lb-img"
            src={convertFileSrc(entry.path)}
            controls
            autoplay
            preload="auto"
          ></video>
        {:else}
          <img
            class="lb-img"
            src={convertFileSrc(entry.path)}
            alt={entry.filename}
            draggable="false"
          />
        {/if}
      {/key}
      {#if canNext}
        <button
          type="button"
          class="lb-nav next"
          onclick={next}
          aria-label={t().lightboxNextAria}
          title={t().lightboxNextTitle}
        ><ChevronRight size={28} /></button>
      {/if}

      <!-- Same UploadToast as HistoryPage — the parent owns the state
           and this instance just re-renders it above the lightbox
           backdrop so the user sees it without having to close. -->
      <UploadToast state={uploadResult} position="absolute" />
    </div>
  </div>
{/if}

<style>
  /* ─── Container ────────────────────────────────────────────────
   * Fixed full-viewport overlay. Above sticky group headers (z-5)
   * with plenty of headroom. */
  .lb {
    position: fixed;
    inset: 0;
    z-index: 100;
    display: flex;
    flex-direction: column;
    animation: lb-in 180ms ease-out;
  }

  /* Backdrop is a button so a click anywhere outside the image closes.
   * Visually it's just a dim layer — no button affordance. */
  .lb-backdrop {
    position: absolute;
    inset: 0;
    background: rgb(0 0 0 / 0.92);
    backdrop-filter: blur(8px);
    border: none;
    padding: 0;
    cursor: default;
  }

  /* ─── Top bar (filename + actions + close) ─────────────────── */
  .lb-bar {
    position: relative;
    z-index: 1;
    display: flex;
    align-items: center;
    gap: 16px;
    height: 48px;
    padding: 0 12px 0 20px;
    color: #fff;
    flex-shrink: 0;
  }
  .lb-meta {
    display: flex;
    align-items: center;
    gap: 12px;
    min-width: 0;
  }
  .lb-count {
    font-family: var(--font-mono);
    font-size: var(--text-sm);
    color: rgb(255 255 255 / 0.55);
    font-variant-numeric: tabular-nums;
    flex-shrink: 0;
  }
  .lb-name {
    font-size: var(--text-sm);
    color: rgb(255 255 255 / 0.85);
    font-family: var(--font-mono);
    max-width: 380px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .lb-actions {
    margin-left: auto;
    display: inline-flex;
    gap: 6px;
  }
  .lb-close {
    width: 32px;
    height: 32px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: transparent;
    border: none;
    border-radius: var(--radius-sm);
    cursor: pointer;
    margin-left: 4px;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
  }

  /* Lightbox controls — the close X and the shared action buttons — sit
   * on the always-dark backdrop, so they read light in either theme. The
   * Button's default ink is the theme-aware muted token, which vanishes
   * here on light theme; pin them all to one white. (Delete is gated by a
   * confirm dialog, so it needs no danger hover tint of its own.) */
  .lb-close,
  .lb-actions :global(.btn) {
    color: rgb(255 255 255 / 0.7);
  }
  .lb-close:hover,
  .lb-actions :global(.btn:hover:not(:disabled)) {
    background: rgb(255 255 255 / 0.1);
    color: #fff;
  }

  /* ─── Image stage ──────────────────────────────────────────────
   * Image grows to fill available space, but is contained so wide
   * screenshots don't overflow into the toolbar. */
  .lb-stage {
    position: relative;
    z-index: 1;
    flex: 1;
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 0 80px 24px;
    overflow: hidden;
    min-height: 0;
  }
  .lb-img {
    max-width: 100%;
    max-height: 100%;
    object-fit: contain;
    display: block;
    user-select: none;
    border-radius: var(--radius-sm);
    box-shadow: 0 16px 64px rgb(0 0 0 / 0.6);
    animation: lb-zoom 220ms cubic-bezier(0.16, 1, 0.3, 1);
  }
  /* When a video enters HTML5 fullscreen (double-click), strip the
   * cosmetic chrome — `border-radius` would clip the corners and
   * the gap would fall through to the browser `::backdrop`, which
   * Chromium/WebView2 renders as black or white depending on the
   * system theme. Either way the user sees a thin strip of "wrong"
   * colour around the rounded corners. Reset to a flat rectangle
   * in fullscreen, and pin the backdrop to black for the very
   * narrow window between layout and paint. */
  .lb-img:fullscreen {
    border-radius: 0;
    box-shadow: none;
    max-width: 100vw;
    max-height: 100vh;
  }
  .lb-img::backdrop {
    background: #000;
  }

  /* ─── Prev / Next nav ──────────────────────────────────────────
   * Floating chevrons inside the stage padding. Translucent dark
   * so they read on any image content. */
  .lb-nav {
    position: absolute;
    top: 50%;
    transform: translateY(-50%);
    width: 48px;
    height: 48px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: transparent;
    color: rgb(255 255 255 / 0.7);
    border: none;
    border-radius: var(--radius-full);
    cursor: pointer;
    padding: 0;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
  }
  .lb-nav:hover {
    background: rgb(255 255 255 / 0.1);
    color: #fff;
  }
  .lb-nav.prev {
    left: 16px;
  }
  .lb-nav.next {
    right: 16px;
  }
  /* Optical centering for chevron (Lucide V reads "heavy" toward opening). */
  .lb-nav.prev :global(svg) { transform: translateX(-1px); }
  .lb-nav.next :global(svg) { transform: translateX(1px); }

  /* ─── Animations ──────────────────────────────────────────── */
  @keyframes lb-in {
    from {
      opacity: 0;
    }
    to {
      opacity: 1;
    }
  }
  @keyframes lb-zoom {
    from {
      opacity: 0;
      transform: scale(0.97);
    }
    to {
      opacity: 1;
      transform: scale(1);
    }
  }
</style>
