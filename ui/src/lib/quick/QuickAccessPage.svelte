<script lang="ts">
  /**
   * Quick Access panel.
   *
   * Pre-declared `quick` window: right-edge, vertically centered.
   * Opens via tray or Ctrl+Shift+A; lists the 8 most recent captures
   * and offers Open (click thumb) / Copy / Reveal per row.
   *
   * Dismiss = ESC or focus loss. Refreshes live on `capture:saved`
   * and `capture:deleted` so the panel stays in sync if the user
   * keeps capturing while it's open.
   */
  import { convertFileSrc, invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { Copy, FolderOpen, Play, Video, X } from "@lucide/svelte";
  import { onDestroy, onMount } from "svelte";
  import { SvelteMap } from "svelte/reactivity";
  import Button from "../components/Button.svelte";
  import StatusMessage from "../components/StatusMessage.svelte";
  import { dismissOffscreen } from "../dismissOffscreen";
  import { getLocale, initLocaleSync, t } from "../i18n/index.svelte";

  type CaptureKind = "image" | "video" | "gif";

  type CaptureEntry = {
    path: string;
    filename: string;
    sizeBytes: number;
    modifiedMs: number;
    kind: CaptureKind;
    thumbnailPath?: string;
  };

  const MAX_ITEMS = 8;

  let entries = $state<CaptureEntry[]>([]);
  let error = $state<string | null>(null);
  let unlistenSaved: UnlistenFn | undefined;
  let unlistenDeleted: UnlistenFn | undefined;
  let unlistenFocus: (() => void) | undefined;
  let unlistenLocale: UnlistenFn | undefined;

  /** Sidecar paths backfilled lazily for legacy captures without a
   * `.thumb.jpg` next to them. Quick Access never holds more than 8
   * entries, so this map stays trivially small — but we still share
   * the same pattern as History so the daemon sees a single command
   * stream and Quick Access never re-asks for a sidecar it already
   * has. */
  const thumbCache = new SvelteMap<string, string>();
  const thumbInflight = new Set<string>();

  const win = getCurrentWindow();

  /** Localized age label: "agora" / "há 5 min" / "ontem" up to a week,
   * then an absolute day+month ("27 de mai."). The thumbnail already
   * answers *what* the capture is, so the text answers *when* — the one
   * axis that tells eight near-identical recents apart.
   *
   * Both Intl formatters are derived on the locale so a language flip
   * re-labels every card in place; building them is the costly part, so
   * we build once per locale, not once per card. */
  const relTime = $derived(
    new Intl.RelativeTimeFormat(getLocale(), { numeric: "auto", style: "short" }),
  );
  const absDate = $derived(
    new Intl.DateTimeFormat(getLocale(), { day: "numeric", month: "short" }),
  );

  function when(ms: number): string {
    const sec = Math.round((ms - Date.now()) / 1000);
    if (Math.abs(sec) < 60) return relTime.format(0, "second");
    const min = Math.round(sec / 60);
    if (Math.abs(min) < 60) return relTime.format(min, "minute");
    const hr = Math.round(sec / 3600);
    if (Math.abs(hr) < 24) return relTime.format(hr, "hour");
    const day = Math.round(sec / 86_400);
    if (Math.abs(day) < 7) return relTime.format(day, "day");
    return absDate.format(new Date(ms));
  }

  const close = () => dismissOffscreen(win);

  async function refresh() {
    try {
      const all = await invoke<CaptureEntry[]>("list_captures");
      entries = all.slice(0, MAX_ITEMS);
      error = null;
      // Quick Access is always 8 items, all visible — IntersectionObserver
      // would be overkill. Just fire-and-forget the backfill for any
      // entry without a sidecar so the cache fills up while the user
      // is still reading the list.
      for (const entry of entries) {
        if (entry.thumbnailPath || thumbCache.has(entry.path) || thumbInflight.has(entry.path)) {
          continue;
        }
        const path = entry.path;
        // GIFs render themselves in <img> — skip the sidecar pass that
        // would only fail (image crate built without the GIF feature).
        if (entry.kind === "gif") continue;
        const cmd = entry.kind === "video" ? "ensure_video_thumbnail" : "ensure_image_thumbnail";
        thumbInflight.add(path);
        void invoke<string>(cmd, { path })
          .then((thumb) => thumbCache.set(path, thumb))
          .catch((err) => console.warn(cmd, path, err))
          .finally(() => thumbInflight.delete(path));
      }
    } catch (e) {
      error = String(e);
    }
  }

  function posterFor(entry: CaptureEntry): string | undefined {
    return entry.thumbnailPath ?? thumbCache.get(entry.path);
  }

  async function openIt(path: string) {
    try {
      await invoke("open_file", { path });
      await close();
    } catch (e) {
      console.error("open_file", e);
    }
  }

  async function reveal(path: string, ev: Event) {
    ev.stopPropagation();
    try {
      await invoke("reveal_in_folder", { path });
      await close();
    } catch (e) {
      console.error("reveal_in_folder", e);
    }
  }

  async function copyImage(path: string, ev: Event) {
    ev.stopPropagation();
    try {
      await invoke("copy_capture_image", { path });
    } catch (e) {
      console.error("copy_capture_image", e);
    }
  }

  function onKey(event: KeyboardEvent) {
    if (event.key === "Escape") void close();
  }

  onMount(async () => {
    unlistenLocale = await initLocaleSync();
    await refresh();
    unlistenSaved = await listen("capture:saved", () => void refresh());
    unlistenDeleted = await listen("capture:deleted", () => void refresh());
    unlistenFocus = await win.onFocusChanged(({ payload: focused }) => {
      if (!focused) void close();
    });
  });

  onDestroy(() => {
    unlistenSaved?.();
    unlistenDeleted?.();
    unlistenFocus?.();
    unlistenLocale?.();
  });
</script>

<svelte:window onkeydown={onKey} />

<main class="root">
  <header class="head">
    <h1>{t().quickTitle}</h1>
    <Button iconOnly ghost size="sm" onclick={close} ariaLabel={t().commonClose} title={t().commonCloseEsc}>
      <X size={13} />
    </Button>
  </header>

  {#if error}
    <StatusMessage variant="error" title={t().quickFailedToLoad} reason={error} />
  {:else if entries.length === 0}
    <StatusMessage title={t().quickEmptyTitle} reason={t().quickEmptyReason} />
  {:else}
    <div class="list">
      {#each entries as entry (entry.path)}
        {@const poster = posterFor(entry)}
        <button class="item" onclick={() => openIt(entry.path)} title={entry.filename}>
          <span class="thumb">
            {#if entry.kind === "gif"}
              <!-- GIF plays in `<img>` natively. No sidecar branch
                   needed; file IS the thumbnail. -->
              <img src={convertFileSrc(entry.path)} alt="" loading="lazy" draggable="false" />
            {:else if poster}
              <img src={convertFileSrc(poster)} alt="" loading="lazy" draggable="false" />
            {:else if entry.kind === "image"}
              <!-- Fallback to the full PNG while the sidecar backfills.
                   Quick Access maxes out at 8 entries so the worst-case
                   VRAM cost is bounded; the cache swap lands fast. -->
              <img src={convertFileSrc(entry.path)} alt="" loading="lazy" draggable="false" />
            {:else}
              <Video size={28} strokeWidth={1.5} />
            {/if}
            {#if entry.kind === "video"}
              <span class="play-badge" aria-hidden="true">
                <Play size={12} fill="currentColor" />
              </span>
            {/if}
          </span>
          <span class="row">
            <span class="when">{when(entry.modifiedMs)}</span>
            <span class="actions">
              {#if entry.kind === "image"}
                <span
                  class="action"
                  role="button"
                  tabindex="0"
                  onclick={(ev) => copyImage(entry.path, ev)}
                  onkeydown={(ev) => ev.key === "Enter" && copyImage(entry.path, ev)}
                  aria-label={t().actionCopyImage}
                  title={t().actionCopyImage}
                >
                  <Copy size={12} />
                </span>
              {/if}
              <span
                class="action"
                role="button"
                tabindex="0"
                onclick={(ev) => reveal(entry.path, ev)}
                onkeydown={(ev) => ev.key === "Enter" && reveal(entry.path, ev)}
                aria-label={t().actionReveal}
                title={t().actionReveal}
              >
                <FolderOpen size={12} />
              </span>
            </span>
          </span>
        </button>
      {/each}
    </div>
  {/if}
</main>

<style>
  .root {
    height: 100vh;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }

  .head {
    display: flex;
    align-items: center;
    height: var(--titlebar-h);
    padding: 0 var(--space-3);
    border-bottom: 1px solid var(--color-border-subtle);
    flex-shrink: 0;
  }
  .head h1 {
    margin: 0;
    font-size: var(--text-md);
    font-weight: 600;
    color: var(--color-fg);
    flex: 1;
  }
  .list {
    flex: 1;
    overflow-y: auto;
    padding: 8px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }

  .item {
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 6px;
    border: 1px solid var(--color-border-subtle);
    background: var(--color-surface-1);
    border-radius: var(--radius-md);
    cursor: pointer;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      border-color var(--duration-quick) var(--ease-in-out-soft);
    text-align: left;
  }
  .item:hover {
    background: var(--color-surface-2);
    border-color: var(--color-border-accent);
  }
  .item:active {
    background: var(--color-surface-3);
  }

  .thumb {
    position: relative;
    display: flex;
    align-items: center;
    justify-content: center;
    width: 100%;
    aspect-ratio: 16 / 10;
    background: var(--color-surface-media);
    border-radius: var(--radius-sm);
    overflow: hidden;
    color: var(--color-fg-muted);
  }
  .thumb img {
    width: 100%;
    height: 100%;
    object-fit: contain;
    display: block;
  }
  /* Compact play overlay on video thumbs (matches History badge,
   * sized down for the narrower Quick Access cards). */
  .play-badge {
    position: absolute;
    top: 6px;
    right: 6px;
    width: 22px;
    height: 22px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: #fff;
    background: rgb(0 0 0 / 0.6);
    backdrop-filter: blur(4px);
    border-radius: var(--radius-full);
    pointer-events: none;
  }
  .play-badge :global(svg) {
    transform: translateX(1px);
  }

  .row {
    display: flex;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }
  .when {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-size: var(--text-xs);
    color: var(--color-fg-muted);
  }
  .actions {
    display: inline-flex;
    gap: 2px;
    flex-shrink: 0;
  }
  .action {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 22px;
    height: 22px;
    border-radius: var(--radius-xs);
    color: var(--color-fg-muted);
    cursor: pointer;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
  }
  .action:hover {
    background: var(--color-surface-2);
    color: var(--color-fg);
  }
</style>
