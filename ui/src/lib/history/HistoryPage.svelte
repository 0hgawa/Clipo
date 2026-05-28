<script lang="ts">
  /**
   * History window.
   *
   * Pre-declared `history` Tauri window. Pulls captures from the
   * filesystem via `list_captures` and re-renders on every
   * `capture:saved` / `capture:deleted` event so the grid stays live
   * while the user keeps capturing.
   *
   * Layout follows the Google Photos / CleanShot pattern: sticky date
   * group headers + thumbnail grid with hover-revealed actions, and
   * clicking a thumb opens an in-app lightbox instead of handing off
   * to the system viewer.
   */
  import { convertFileSrc, invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { Store } from "@tauri-apps/plugin-store";
  import { Play, RotateCw, Video } from "@lucide/svelte";
  import WindowChrome from "../chrome/WindowChrome.svelte";
  import Button from "../components/Button.svelte";
  import SegmentedControl from "../components/SegmentedControl.svelte";
  import StatusMessage from "../components/StatusMessage.svelte";
  import CaptureActionsRow from "../actions/CaptureActionsRow.svelte";
  import UploadToast from "../actions/UploadToast.svelte";
  import type { ActionId, UploadResult } from "../actions/captureRegistry";
  import Lightbox from "./Lightbox.svelte";
  import { onDestroy, onMount } from "svelte";
  import { SvelteMap } from "svelte/reactivity";
  import { getLocale, initLocaleSync, fmt, t } from "../i18n/index.svelte";

  type CaptureKind = "image" | "video" | "gif";

  type CaptureEntry = {
    path: string;
    filename: string;
    modifiedMs: number;
    kind: CaptureKind;
    /** Sidecar JPEG poster. Backend fills this on save (both kinds);
     * legacy captures without a sidecar trigger a kind-aware lazy
     * backfill (`ensure_image_thumbnail` / `ensure_video_thumbnail`)
     * the first time their card enters the viewport. */
    thumbnailPath?: string;
  };

  let entries = $state<CaptureEntry[]>([]);
  let loading = $state(true);
  let error = $state<string | null>(null);
  let unlistenSaved: UnlistenFn | undefined;
  let unlistenDeleted: UnlistenFn | undefined;
  let unlistenUploads: UnlistenFn | undefined;
  let unlistenLocale: UnlistenFn | undefined;
  let uploadsStore: Store | undefined;

  /** Segmented date-range filter. Replaces filename search — nobody
   * memorises `Screenshot_2026-05-21_14-32.png`, but everyone says
   * "the one from this week". */
  type FilterMode = "all" | "today" | "week" | "month";
  let filterMode = $state<FilterMode>("all");
  /** Date-filter chip labels — reactive so a language flip updates
   * the segmented control in place. Status keys stay stable; only
   * the visible label flows through `t()`. */
  const FILTERS = $derived.by<readonly { value: FilterMode; label: string }[]>(() => [
    { value: "all", label: t().historyDateAll },
    { value: "today", label: t().historyDateToday },
    { value: "week", label: t().historyDateWeek },
    { value: "month", label: t().historyDateMonth },
  ]);

  /** Media-kind filter — second axis next to the date chips. The two
   * axes compose: "Videos" + "Week" = only recordings from this week.
   * Each filter is a single string compare against `entry.kind` in
   * the derived `filtered` pass — no extra allocations. */
  type KindMode = "all" | "image" | "video";
  let kindMode = $state<KindMode>("all");
  const KIND_FILTERS = $derived.by<readonly { value: KindMode; label: string }[]>(() => [
    { value: "all", label: t().historyKindAll },
    { value: "image", label: t().historyKindImage },
    { value: "video", label: t().historyKindVideo },
  ]);

  /** Date helpers shared by filtering and bucketing. Both derivations
   * compute "days ago" against today's midnight, so factor the maths
   * out of the per-entry loop. */
  const DAY_MS = 86_400_000;

  function startOfDay(d: Date): number {
    return new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  }

  const filtered = $derived.by(() => {
    // Both filters in a single pass so a "Tudo / Tudo" selection
    // short-circuits to a reference equality check (no new array)
    // and any combo only walks `entries` once.
    if (filterMode === "all" && kindMode === "all") return entries;
    const now = new Date();
    const todayStart = startOfDay(now);
    return entries.filter((entry) => {
      // GIFs fold into the Photos filter (Google Photos convention — a
      // .gif is an animated image, not its own filter axis).
      const kindOk =
        kindMode === "all" ||
        entry.kind === kindMode ||
        (kindMode === "image" && entry.kind === "gif");
      if (!kindOk) return false;
      if (filterMode === "all") return true;
      const d = new Date(entry.modifiedMs);
      const daysAgo = Math.round((todayStart - startOfDay(d)) / DAY_MS);
      if (filterMode === "today") return daysAgo === 0;
      if (filterMode === "week") return daysAgo < 7;
      // month — same calendar month + year
      return d.getMonth() === now.getMonth() && d.getFullYear() === now.getFullYear();
    });
  });

  // The empty-filtered state can come from either axis (kind, date) or
  // both, so name whichever is actually narrowing — otherwise "Nothing
  // in {label}" shows the date even when the kind filter is the cause.
  function currentFilterLabel(): string {
    const parts: string[] = [];
    if (kindMode !== "all") parts.push(KIND_FILTERS.find((f) => f.value === kindMode)?.label ?? "");
    if (filterMode !== "all") parts.push(FILTERS.find((f) => f.value === filterMode)?.label ?? "");
    return parts.join(" · ");
  }

  /** Date bucketing à la Google Photos: groups replace the per-card
   * timestamp so the grid stays purely visual and a single date label
   * covers all captures from that period. */
  type Bucket = { key: string; label: string; order: number; entries: CaptureEntry[] };

  /** Resolve a localized month name via `Intl.DateTimeFormat`. The
   * formatter is reactive on `getLocale()` so a language flip
   * re-renders sticky headers in place — no hand-maintained month
   * arrays per language. */
  const monthFormatter = $derived(
    new Intl.DateTimeFormat(getLocale(), { month: "long" }),
  );
  const monthYearFormatter = $derived(
    new Intl.DateTimeFormat(getLocale(), { month: "long", year: "numeric" }),
  );

  function bucketOf(ms: number, now: Date, todayStart: number): { key: string; label: string; order: number } {
    const date = new Date(ms);
    const daysAgo = Math.round((todayStart - startOfDay(date)) / DAY_MS);
    const labels = t();
    if (daysAgo <= 0) return { key: "today", label: labels.historyBucketToday, order: 0 };
    if (daysAgo === 1) return { key: "yesterday", label: labels.historyBucketYesterday, order: 1 };
    if (daysAgo < 7) return { key: "thisweek", label: labels.historyBucketThisWeek, order: 2 };
    if (date.getMonth() === now.getMonth() && date.getFullYear() === now.getFullYear()) {
      return { key: "thismonth", label: labels.historyBucketThisMonth, order: 3 };
    }
    const monthsAgo =
      (now.getFullYear() - date.getFullYear()) * 12 + (now.getMonth() - date.getMonth());
    const key = `${date.getFullYear()}-${String(date.getMonth()).padStart(2, "0")}`;
    const label =
      date.getFullYear() === now.getFullYear()
        ? monthFormatter.format(date)
        : monthYearFormatter.format(date);
    return { key, label, order: 3 + monthsAgo };
  }

  const grouped = $derived.by<Bucket[]>(() => {
    const sorted = [...filtered].sort((a, b) => b.modifiedMs - a.modifiedMs);
    const now = new Date();
    const todayStart = startOfDay(now);
    const map = new Map<string, Bucket>();
    for (const entry of sorted) {
      const b = bucketOf(entry.modifiedMs, now, todayStart);
      let bucket = map.get(b.key);
      if (!bucket) {
        bucket = { ...b, entries: [] };
        map.set(b.key, bucket);
      }
      bucket.entries.push(entry);
    }
    return [...map.values()].sort((a, b) => a.order - b.order);
  });

  /** Refresh entries. `showLoading` is only true on the very first
   * mount — toggling `loading` on background refreshes (save/delete
   * events) would unmount the scroll container and reset the user's
   * scroll position to the top, which is jarring mid-browse. */
  async function refresh(showLoading = false) {
    if (showLoading) loading = true;
    error = null;
    try {
      entries = await invoke<CaptureEntry[]>("list_captures");
    } catch (e) {
      error = String(e);
    } finally {
      if (showLoading) loading = false;
    }
  }

  onMount(async () => {
    // One observer for the whole grid: cheaper than per-card and
    // batches notifications. `rootMargin` pre-fetches thumbs ~one
    // viewport ahead of the user's scroll so they're already swapped
    // in by the time the card lands on-screen.
    thumbObserver = new IntersectionObserver(
      (observed) => {
        for (const obs of observed) {
          if (!obs.isIntersecting) continue;
          const el = obs.target as HTMLElement;
          const path = el.dataset.thumbPath;
          const kind = el.dataset.thumbKind as CaptureKind | undefined;
          if (!path || !kind) continue;
          thumbObserver?.unobserve(obs.target);
          if (thumbCache.has(path) || thumbInflight.has(path)) continue;
          // GIFs render themselves in `<img>` already (no sidecar
          // branch). Skip thumbnail generation — the `image` crate
          // ships without the GIF feature, so the call would fail
          // anyway and pollute the console with warnings.
          if (kind === "gif") continue;
          thumbInflight.add(path);
          const cmd = kind === "video" ? "ensure_video_thumbnail" : "ensure_image_thumbnail";
          void invoke<string>(cmd, { path })
            .then((thumb) => thumbCache.set(path, thumb))
            .catch((e) => console.warn(cmd, path, e))
            .finally(() => thumbInflight.delete(path));
        }
      },
      { rootMargin: "400px 0px" },
    );

    unlistenLocale = await initLocaleSync();
    await refresh(true);
    unlistenSaved = await listen("capture:saved", () => void refresh());
    unlistenDeleted = await listen("capture:deleted", () => void refresh());
    // `tauri-plugin-store` handles persistence, debounced writes and
    // cross-window change notifications. We hydrate once from the
    // file and let `onChange` keep the local map in sync with edits
    // coming from any other window (notably the post-capture panel).
    try {
      uploadsStore = await Store.load("uploads.json");
      for (const [path, value] of await uploadsStore.entries<string>()) {
        if (typeof value === "string") uploadedUrls.set(path, value);
      }
      unlistenUploads = await uploadsStore.onChange<string>((key, value) => {
        if (typeof value === "string") {
          uploadedUrls.set(key, value);
        } else if (value === undefined) {
          uploadedUrls.delete(key);
        }
      });
    } catch (e) {
      console.error("uploads store", e);
    }
  });

  onDestroy(() => {
    clearTimeout(uploadTimer);
    unlistenSaved?.();
    unlistenDeleted?.();
    unlistenUploads?.();
    unlistenLocale?.();
    thumbObserver?.disconnect();
  });

  /** Lightbox state. Index points into `filtered` (the flat list
   * driving the grid), not into the bucketed groups. Null = closed. */
  let lightboxIndex = $state<number | null>(null);

  function openLightbox(entry: CaptureEntry) {
    const idx = filtered.findIndex((e) => e.path === entry.path);
    if (idx >= 0) lightboxIndex = idx;
  }

  /** When entries shrink (delete-from-inside, filter change), keep
   * the index inside bounds — clamp to the new last item, or close
   * the lightbox if nothing's left. */
  $effect(() => {
    if (lightboxIndex === null) return;
    if (filtered.length === 0) {
      lightboxIndex = null;
      return;
    }
    if (lightboxIndex >= filtered.length) {
      lightboxIndex = filtered.length - 1;
    }
  });

  /** Single upload-feedback slot shared by the grid hover overlay
   * AND the Lightbox toast (via prop). Whichever surface is visible
   * shows the latest result — auto-resets to idle after a few
   * seconds so the chrome doesn't accumulate stale URLs. */
  let uploadResult = $state<UploadResult>({ kind: "idle" });
  let uploadTimer: ReturnType<typeof setTimeout> | undefined;

  /** path → URL of every capture this user has already uploaded.
   * Hydrated once from the daemon's `uploads.json`; mutated as new
   * uploads land. The grid and lightbox read this so the Upload
   * button flips to a `Copy link` chrome for captures that already
   * live online — saves a network round-trip and avoids burning
   * through host rate limits on accidental double-clicks. */
  const uploadedUrls = new SvelteMap<string, string>();

  function handleUploadResult(result: UploadResult, path: string) {
    clearTimeout(uploadTimer);
    uploadResult = result;
    uploadTimer = setTimeout(
      () => {
        uploadResult = { kind: "idle" };
      },
      result.kind === "error" ? 5000 : 4000,
    );
    if (result.kind === "done") {
      uploadedUrls.set(path, result.url);
    }
  }

  const GRID_ACTIONS_IMAGE = ["edit", "ocr", "copy", "upload", "reveal", "delete"] as const satisfies readonly ActionId[];
  /** Video action set: edit / ocr / copy / upload don't apply to MP4
   * (Catbox/0x0.st both cap well under typical recording sizes — see
   * the note in PostCaptureActions for the full reasoning). `open`
   * takes the play-it slot — opens in the system video player. */
  const GRID_ACTIONS_VIDEO = ["open", "gif", "reveal", "delete"] as const satisfies readonly ActionId[];
  /** GIF action set: edit / ocr / copy don't preserve animation
   * (editor canvas is static; OCR runs on one frame; clipboard image
   * formats drop frames). Upload / open / reveal / delete are the
   * only ones whose semantics survive an animated source. */
  const GRID_ACTIONS_GIF = ["open", "upload", "reveal", "delete"] as const satisfies readonly ActionId[];

  function actionsFor(kind: CaptureKind): readonly ActionId[] {
    if (kind === "video") return GRID_ACTIONS_VIDEO;
    if (kind === "gif") return GRID_ACTIONS_GIF;
    return GRID_ACTIONS_IMAGE;
  }

  /** Backfilled sidecar paths for legacy captures (image or video)
   * that didn't have a `.thumb.jpg` at list-time. Reactive map so
   * the card swaps to the lightweight sidecar the moment the daemon
   * resolves — replacing the full PNG (image legacy) or the video
   * glyph placeholder (video legacy) in place. */
  const thumbCache = new SvelteMap<string, string>();
  /** Paths currently being processed — dedupes simultaneous
   * IntersectionObserver hits (rapid scroll past the same card
   * before the first call returns). */
  const thumbInflight = new Set<string>();

  let thumbObserver: IntersectionObserver | undefined;

  /** Svelte action: attach a card to the shared IntersectionObserver
   * for lazy sidecar backfill. The observer callback dispatches to
   * the right Tauri command based on `kind`. One observer for all
   * cards keeps overhead O(1) regardless of how many captures the
   * user has. */
  function lazyThumb(node: HTMLElement, param: { kind: CaptureKind; path: string }) {
    node.dataset.thumbPath = param.path;
    node.dataset.thumbKind = param.kind;
    thumbObserver?.observe(node);
    return {
      update(next: { kind: CaptureKind; path: string }) {
        node.dataset.thumbPath = next.path;
        node.dataset.thumbKind = next.kind;
      },
      destroy() {
        thumbObserver?.unobserve(node);
      },
    };
  }

  /** Resolve the sidecar JPEG for a card. `thumbnailPath` (from the
   * daemon, set if the file existed at list time) wins; otherwise
   * whatever the backfill cached. `undefined` → caller decides the
   * fallback (image: full PNG; video: glyph placeholder). */
  function posterFor(entry: CaptureEntry): string | undefined {
    return entry.thumbnailPath ?? thumbCache.get(entry.path);
  }

</script>

<main class="root">
  <header class="head" data-tauri-drag-region>
    <h1 data-tauri-drag-region>{t().historyTitle}</h1>
    <span class="count" data-tauri-drag-region>
      {fmt(t().historyCount, { filtered: filtered.length, total: entries.length })}
    </span>
    <SegmentedControl
      value={kindMode}
      options={KIND_FILTERS}
      onchange={(v) => (kindMode = v)}
      ariaLabel={t().historyFilterByKind}
    />
    <SegmentedControl
      value={filterMode}
      options={FILTERS}
      onchange={(v) => (filterMode = v)}
      ariaLabel={t().historyFilterByDate}
    />
    <Button size="sm" onclick={() => refresh()} ariaLabel={t().commonRefresh} title={t().commonRefresh}>
      <RotateCw size={14} />
      <span>{t().commonRefresh}</span>
    </Button>
    <WindowChrome />
  </header>

  {#if loading}
    <StatusMessage loading title={t().commonLoading} />
  {:else if error}
    <StatusMessage variant="error" title={t().historyFailedToLoad} reason={error} />
  {:else if entries.length === 0}
    <div class="empty">
      <p class="empty-title">{t().historyEmptyTitle}</p>
      <p class="empty-hint">
        <!-- Substitute the two key combos as kbd glyphs. The catalog
             string carries the surrounding sentence in the locale's
             natural word order. -->
        {@html fmt(t().historyEmptyHint, {
          shortcut: '<kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>S</kbd>',
          fullscreen: '<kbd>PrintScreen</kbd>',
        })}
      </p>
    </div>
  {:else if filtered.length === 0}
    <div class="empty">
      <p class="empty-title">{fmt(t().historyEmptyFiltered, { label: currentFilterLabel() })}</p>
      <!-- Click delegation: the catalog string carries the link as
           inline HTML so the surrounding sentence reads naturally per
           language; we listen on the parent and clear BOTH filters
           (kind + date) so "show all" genuinely returns every capture,
           whichever axis emptied the grid. -->
      <p
        class="empty-hint"
        onclick={(e) => {
          if ((e.target as HTMLElement).closest("[data-reset-filter]")) {
            kindMode = "all";
            filterMode = "all";
          }
        }}
        role="presentation"
      >
        {@html fmt(t().historyEmptyFilteredHint, {
          all: `<button type="button" class="link" data-reset-filter>${t().historyDateAll}</button>`,
          count: entries.length,
        })}
      </p>
    </div>
  {:else}
    <div class="scroll">
      {#each grouped as group (group.key)}
        <section class="group">
          <h2 class="group-label">{group.label}</h2>
          <div class="grid">
            {#each group.entries as entry (entry.path)}
              {@const poster = posterFor(entry)}
              <article class="card">
                <div class="thumb-wrap">
                  <button class="thumb" onclick={() => openLightbox(entry)} title={entry.filename}>
                    {#if entry.kind === "gif"}
                      <!-- GIFs play themselves in `<img>` — no sidecar,
                           no placeholder, the file IS the thumbnail.
                           `loading=lazy` keeps off-screen ones out of
                           VRAM until the user scrolls them in. -->
                      <img src={convertFileSrc(entry.path)} alt={entry.filename} loading="lazy" draggable="false" />
                    {:else if poster}
                      <!-- Sidecar JPEG: ~15 KB, decodes to ~500 KB
                           in VRAM. The shared path for image + video. -->
                      <img src={convertFileSrc(poster)} alt={entry.filename} loading="lazy" draggable="false" />
                    {:else if entry.kind === "image"}
                      <!-- Legacy image without a sidecar yet. Show
                           the full PNG inline while the observer
                           backfills, so the user never sees a blank
                           card. `loading=lazy` keeps off-screen ones
                           out of VRAM until they scroll into view. -->
                      <img
                        src={convertFileSrc(entry.path)}
                        alt={entry.filename}
                        loading="lazy"
                        draggable="false"
                        use:lazyThumb={{ kind: entry.kind, path: entry.path }}
                      />
                    {:else}
                      <!-- Legacy video: can't render `.mp4` in <img>,
                           so a glyph placeholder fills the same aspect
                           box until the backfill lands. -->
                      <div class="video-placeholder" use:lazyThumb={{ kind: entry.kind, path: entry.path }}>
                        <Video size={32} />
                      </div>
                    {/if}
                  </button>
                  {#if entry.kind === "video"}
                    <span class="play-badge" aria-hidden="true">
                      <Play size={16} fill="currentColor" />
                    </span>
                  {/if}
                  <!-- Sibling of the thumb (not a child) — nested
                       buttons are invalid HTML. Actions come from the
                       shared registry so the lightbox and post-capture
                       panel stay in lock-step with whatever lands here. -->
                  <div class="actions-overlay" role="toolbar" aria-label={t().historyActionsAria}>
                    <CaptureActionsRow
                      {entry}
                      actions={actionsFor(entry.kind)}
                      cachedUrl={uploadedUrls.get(entry.path)}
                      onUploadResult={handleUploadResult}
                    />
                  </div>
                </div>
              </article>
            {/each}
          </div>
        </section>
      {/each}
    </div>
  {/if}
</main>

<!-- Shared toast — renders above the grid when no lightbox is open.
     The Lightbox renders its own copy of <UploadToast> inside its
     overlay (above the backdrop) so the same `uploadResult` state
     surfaces wherever the user is looking. -->
<UploadToast state={uploadResult} />

{#if lightboxIndex !== null}
  <Lightbox
    entries={filtered}
    index={lightboxIndex}
    {uploadResult}
    {uploadedUrls}
    onClose={() => (lightboxIndex = null)}
    onIndexChange={(i) => (lightboxIndex = i)}
    onUploadResult={handleUploadResult}
  />
{/if}

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
    gap: var(--space-3);
    height: var(--titlebar-h);
    padding: 0 0 0 var(--space-5);
    border-bottom: 1px solid var(--color-border-subtle);
    flex-shrink: 0;
  }
  .head h1 {
    margin: 0;
    font-size: var(--text-md);
    font-weight: 600;
  }
  .count {
    font-size: var(--text-sm);
    color: var(--color-fg-muted);
    /* Push everything after the count (segmented controls + Refresh
     * + chrome) to the right edge of the titlebar. Used to live on
     * the first `.chips` itself; moved here so the chips can be a
     * shared component without leaking layout concerns. */
    margin-right: auto;
  }
  /* Inline button styled as a link, used in the empty state. Lives
   * inside `{@html ...}` so we tag the selector global — Svelte's
   * scoped class hash doesn't reach runtime-injected nodes. */
  .empty :global(.link) {
    background: transparent;
    border: none;
    padding: 0;
    color: var(--color-accent);
    cursor: pointer;
    font: inherit;
    text-decoration: underline;
    text-underline-offset: 2px;
  }
  .empty :global(.link:hover) {
    color: var(--color-accent-fg);
  }
  .empty {
    flex: 1;
    display: flex;
    flex-direction: column;
    justify-content: center;
    align-items: center;
    text-align: center;
    padding: 32px;
    gap: 8px;
  }
  .empty-title {
    margin: 0;
    font-size: var(--text-lg);
    color: var(--color-fg);
  }
  .empty-hint {
    margin: 0;
    font-size: var(--text-md);
    color: var(--color-fg-muted);
  }
  .empty :global(kbd) {
    display: inline-block;
    padding: 1px 5px;
    margin: 0 1px;
    border: 1px solid var(--color-border-subtle);
    background: var(--color-surface-2);
    border-radius: var(--radius-xs);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
  }

  /* ─── Scroll area + date groups ───────────────────────────────
   * The scroll container moved up from `.grid` to a wrapper so
   * the per-group sticky labels can pin against it. Each group
   * stacks: a sticky `.group-label` (date header) followed by a
   * `.grid` of thumbnail cards. */
  .scroll {
    flex: 1;
    overflow-y: auto;
    padding: 0 16px 16px;
  }
  .group + .group {
    margin-top: 4px;
  }
  /* Sticky date header — matches page background so cards scroll
   * underneath without bleeding through. Uppercase + muted colour
   * keeps it as a label, not competing with the thumbs visually. */
  .group-label {
    position: sticky;
    top: 0;
    margin: 0;
    padding: 14px 0 10px;
    font-size: var(--text-xs);
    font-weight: 600;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--color-fg-muted);
    background: var(--color-surface-0);
    z-index: 5;
  }
  /* Tight grid gap (2px) because each card adds its own 6px padding
   * — total visible spacing between thumbs stays at ~14px, but the
   * padding becomes a forgiving hit area that the hover background
   * fills, matching the Windows Explorer / Files pattern.
   *
   * 280px minmax targets ~4-5 columns on common fullscreen widths
   * and adapts down on narrower windows. */
  .grid {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(280px, 1fr));
    gap: 2px;
    align-content: start;
  }

  /* Card pads the thumb so the hit area + hover background extends
   * into the gap. At rest fully transparent (image is the card);
   * on hover the padded region picks up surface-2 with rounded
   * corners — same forgiving target as Explorer's grid. */
  .card {
    display: block;
    padding: 6px;
    border-radius: var(--radius-md);
    transition: background var(--duration-quick) var(--ease-in-out-soft);
  }
  .card:hover {
    background: var(--color-surface-2);
  }

  .thumb-wrap {
    position: relative;
  }
  .thumb {
    display: block;
    width: 100%;
    aspect-ratio: 16 / 10;
    background: var(--color-surface-media);
    border: none;
    border-radius: var(--radius-sm);
    padding: 0;
    overflow: hidden;
    cursor: pointer;
  }
  .thumb img {
    width: 100%;
    height: 100%;
    object-fit: contain;
    display: block;
  }

  /* Placeholder shown for video cards while the sidecar JPEG is
   * being generated lazily. Same aspect-ratio container as the real
   * <img> so the grid doesn't reflow when the thumb lands. */
  .video-placeholder {
    width: 100%;
    height: 100%;
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--color-fg-muted);
    background:
      linear-gradient(135deg, var(--color-surface-2), var(--color-surface-1));
  }

  /* Small "play" affordance overlaid on every video thumbnail's
   * top-right corner — same idiom YouTube/CleanShot use to signal
   * "this card is a clip, not a still". Pointer-events disabled so
   * clicks still hit the thumb button below. */
  .play-badge {
    position: absolute;
    top: 8px;
    right: 8px;
    width: 26px;
    height: 26px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: #fff;
    background: rgb(0 0 0 / 0.6);
    backdrop-filter: blur(6px);
    border-radius: var(--radius-full);
    pointer-events: none;
  }
  /* Optical centre — Lucide's Play glyph reads heavy to the left. */
  .play-badge :global(svg) {
    transform: translateX(1px);
  }

  /* ─── Hover-reveal actions overlay ─────────────────────────────
   * Uniform dark scrim over the whole thumb (Pinterest / Unsplash
   * pattern) — the image dims, light-on-dark buttons pop forward.
   * Overlay itself is permanently click-through; only the buttons
   * catch clicks so the thumb's open-lightbox still fires on the
   * gaps between them. */
  .actions-overlay {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: flex-end;
    justify-content: center;
    gap: 6px;
    padding: 10px;
    background: rgb(0 0 0 / 0.4);
    opacity: 0;
    pointer-events: none;
    transition: opacity var(--duration-quick) var(--ease-in-out-soft);
  }
  .card:hover .actions-overlay,
  .card:focus-within .actions-overlay {
    opacity: 1;
  }

  /* Translucent-dark Buttons over the thumb. Box-shadow gives the
   * 1px hairline border without affecting Button's box dimensions.
   * Overlay parent is pointer-events:none so its gaps don't swallow
   * the thumb's click — Buttons opt back in here. */
  .actions-overlay :global(.btn.icon-only) {
    pointer-events: auto;
    background: rgb(0 0 0 / 0.55);
    color: #fff;
    box-shadow: inset 0 0 0 1px rgb(255 255 255 / 0.12);
    backdrop-filter: blur(6px);
  }
  .actions-overlay :global(.btn.icon-only:hover:not(:disabled)) {
    background: rgb(0 0 0 / 0.85);
    color: #fff;
    box-shadow: inset 0 0 0 1px rgb(255 255 255 / 0.25);
  }
  .actions-overlay :global(.btn.danger:hover:not(:disabled)) {
    background: var(--color-danger);
    box-shadow: inset 0 0 0 1px var(--color-danger);
  }

</style>
