<script lang="ts">
  /**
   * Window picker.
   *
   * Pre-declared `window-picker` window: centered on the primary work
   * area, opaque, alwaysOnTop. Receives the list of capturable
   * top-level windows from the daemon as the `picker:show` payload,
   * lets the user pick one with mouse or keyboard, and asks the
   * daemon to focus + capture it.
   *
   * Two power-user affordances on top of the basic list:
   * - **Type to filter**: a search input owns keyboard focus on
   *   open; matching is case-insensitive substring against the
   *   window title. ↑/↓/Enter/Esc are intercepted at the input so
   *   navigation still works while typing.
   * - **Live count**: header shows `12` or `3 / 12` while filtering;
   *   empty state distinguishes "no capturable windows" from "no
   *   match" with an Esc-to-clear hint.
   *
   * Dismiss = ESC (with non-empty filter, clears the query instead)
   * or focus loss. The window stays alive between runs; state is
   * reset on every `picker:show` so a stale highlight or query from
   * a prior open can't leak into the next one.
   */
  import { invoke } from "@tauri-apps/api/core";
  import { LogicalPosition } from "@tauri-apps/api/dpi";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { AppWindow } from "@lucide/svelte";
  import { onDestroy, onMount, tick } from "svelte";
  import { SvelteMap } from "svelte/reactivity";
  import { fmt, initLocaleSync, t } from "../i18n/index.svelte";

  type WindowInfo = { id: number; title: string; width: number; height: number };
  type IconPayload = { id: number; dataUrl: string };

  /** Format a window's bounds as the row's subtitle. Mono spacing
   * keeps digits aligned across rows; thin spaces around `×` read as
   * the dimensions symbol rather than punctuation. Zero/missing
   * bounds fall through to an empty string — the subtitle slot just
   * collapses for that row (rare). */
  function formatResolution(w: WindowInfo): string {
    if (w.width === 0 || w.height === 0) return "";
    return `${w.width} × ${w.height}`;
  }

  const OFFSCREEN = new LogicalPosition(-30000, -30000);

  let windows = $state<WindowInfo[]>([]);
  let query = $state("");
  let selected = $state(0);
  let listEl = $state<HTMLUListElement | undefined>(undefined);
  let inputEl = $state<HTMLInputElement | undefined>(undefined);
  /** Per-window icon data: URLs. Daemon streams them in via
   * `picker:icon` events after the list is already on screen, so each
   * row swaps from the lucide placeholder to the real app icon as the
   * payload arrives. `SvelteMap` so a single set targets only that
   * row's render (not the whole list). */
  const icons = new SvelteMap<number, string>();
  let unlistenShow: UnlistenFn | undefined;
  let unlistenIcon: UnlistenFn | undefined;
  let unlistenFocus: (() => void) | undefined;
  let unlistenLocale: UnlistenFn | undefined;

  const win = getCurrentWindow();

  /** Substring filter on the title. Case-insensitive; empty query
   * passes everything (no allocation overhead — JS's filter on an
   * always-true predicate is a cheap clone). Kept reactive so a
   * keystroke in the input re-renders the rows in one frame. */
  const filtered = $derived.by(() => {
    const q = query.trim().toLowerCase();
    if (q === "") return windows;
    return windows.filter((w) => w.title.toLowerCase().includes(q));
  });

  /** Display string for the header counter. Two forms — total when no
   * filter, "matched / total" when filtering — so the user sees at a
   * glance how the filter narrowed the list. */
  const countLabel = $derived.by(() => {
    if (query.trim() === "") return String(windows.length);
    return `${filtered.length} / ${windows.length}`;
  });

  async function close() {
    try {
      // Off-screen + hide (the daemon also calls `hide` on its side
      // when an action dispatches; this path covers Esc + blur).
      await win.setPosition(OFFSCREEN);
      await invoke("close_window_picker");
    } catch (e) {
      console.error("close_window_picker", e);
    }
  }

  async function pick(id: number) {
    try {
      await invoke("capture_window", { id });
    } catch (e) {
      console.error("capture_window", e);
      await close();
    }
  }

  function onKey(event: KeyboardEvent) {
    if (event.key === "Escape") {
      event.preventDefault();
      // Two-stage Esc: first clears the query (if any), second closes.
      // Matches Raycast / VS Code Cmd+P / Spotlight behaviour.
      if (query !== "") {
        query = "";
        selected = 0;
      } else {
        void close();
      }
      return;
    }
    if (filtered.length === 0) return;
    if (event.key === "ArrowDown") {
      event.preventDefault();
      selected = (selected + 1) % filtered.length;
      scrollSelectedIntoView();
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      selected = (selected - 1 + filtered.length) % filtered.length;
      scrollSelectedIntoView();
    } else if (event.key === "Home") {
      event.preventDefault();
      selected = 0;
      scrollSelectedIntoView();
    } else if (event.key === "End") {
      event.preventDefault();
      selected = filtered.length - 1;
      scrollSelectedIntoView();
    } else if (event.key === "Enter") {
      event.preventDefault();
      const target = filtered[selected];
      if (target) void pick(target.id);
    }
  }

  /** Reset the selection when the filter narrows. Without this, a
   * `selected` of 5 with a freshly-typed query that yields 2 matches
   * would point at nothing — the row indicator just disappears. The
   * `await tick()` lets the derived `filtered` recompute and the DOM
   * re-render before we ask `listEl.children[0]` to scroll into view:
   * without the await we'd be reading the *old* row at index 0. */
  async function onQueryInput() {
    selected = 0;
    await tick();
    scrollSelectedIntoView();
  }

  /** Keep the highlighted row in view during keyboard navigation —
   * `block: "nearest"` only scrolls when the row is actually clipped,
   * so a click that lands on a visible row doesn't yank the scroll. */
  function scrollSelectedIntoView() {
    const node = listEl?.children[selected] as HTMLElement | undefined;
    node?.scrollIntoView({ block: "nearest" });
  }

  /** Move keyboard focus back into the search input after every
   * `picker:show` so the user can start typing immediately. The
   * Tauri window is already focused (`focus: true` in tauri.conf),
   * but webview-internal focus has to be re-asserted because the
   * surface is reused across opens. */
  function focusInput() {
    // `setTimeout(0)` queues after the current micro-task so the
    // freshly-shown window has finished its focus handshake first
    // (calling `focus()` mid-handshake is a no-op).
    setTimeout(() => inputEl?.focus(), 0);
  }

  onMount(async () => {
    unlistenLocale = await initLocaleSync();
    unlistenShow = await listen<WindowInfo[]>("picker:show", async (event) => {
      windows = event.payload ?? [];
      query = "";
      selected = 0;
      // Drop stale icons from the previous invocation so a row doesn't
      // momentarily render with another app's icon before the new
      // `picker:icon` events land. `clear()` is one targeted op vs
      // re-creating the map.
      icons.clear();
      // Wait one frame so the freshly-rendered list is in the DOM
      // before we try to scroll the (re-set) selection into view.
      await tick();
      scrollSelectedIntoView();
      focusInput();
    });
    unlistenIcon = await listen<IconPayload>("picker:icon", (event) => {
      icons.set(event.payload.id, event.payload.dataUrl);
    });
    unlistenFocus = await win.onFocusChanged(({ payload: focused }) => {
      if (!focused) void close();
    });
  });

  onDestroy(() => {
    unlistenShow?.();
    unlistenIcon?.();
    unlistenFocus?.();
    unlistenLocale?.();
  });
</script>

<svelte:window onkeydown={onKey} />

<main class="root">
  <header class="head">
    <div class="search-row">
      <input
        bind:this={inputEl}
        bind:value={query}
        oninput={onQueryInput}
        type="text"
        class="search"
        placeholder={t().pickerSearchPlaceholder}
        spellcheck="false"
        autocomplete="off"
        autocapitalize="off"
        aria-label={t().pickerSearchPlaceholder}
      />
      <span class="count" aria-label={t().pickerCountLabel}>{countLabel}</span>
    </div>
  </header>

  {#if windows.length === 0}
    <!-- True empty: enumeration came back with no capturable windows.
         Rare in practice; usually means every window failed the
         Alt+Tab inclusion filter (e.g. headless session). -->
    <div class="empty">
      <AppWindow size={32} strokeWidth={1.4} />
      <p>{t().pickerEmpty}</p>
    </div>
  {:else if filtered.length === 0}
    <!-- Filter cleared the list. Distinct from "no windows" — the
         CTA is `Esc` to clear the query, not to close the panel. -->
    <div class="empty">
      <AppWindow size={32} strokeWidth={1.4} />
      <p>{fmt(t().pickerEmptyMatch, { query })}</p>
      <p class="empty-hint">
        {@html t().pickerEmptyMatchHint.replace(
          "{esc}",
          '<kbd class="kbd kbd-inline">Esc</kbd>',
        )}
      </p>
    </div>
  {:else}
    <ul class="list" bind:this={listEl} role="listbox" aria-label={t().pickerTitle}>
      {#each filtered as w, i (w.id)}
        {@const res = formatResolution(w)}
        <li role="option" aria-selected={i === selected}>
          <button
            type="button"
            class="row"
            class:selected={i === selected}
            tabindex={-1}
            onclick={() => pick(w.id)}
            onmouseenter={() => (selected = i)}
          >
            <span class="icon" aria-hidden="true">
              {#if icons.has(w.id)}
                <!-- Real per-app icon streamed in by the daemon's
                     background loader. Sized to 32×32 via CSS — we
                     prefer ICON_BIG first in the resolver so the
                     source is typically a native 32×32 PNG and we
                     paint 1:1 without scaling fuzz. -->
                <img class="icon-img" src={icons.get(w.id)} alt="" width="32" height="32" />
              {:else}
                <!-- Placeholder while the daemon's lazy loader works
                     through the list. Also the final state for apps
                     that don't expose an icon at all (rare). -->
                <AppWindow size={22} strokeWidth={1.6} />
              {/if}
            </span>
            <span class="text">
              <span class="title">{w.title}</span>
              {#if res}
                <!-- Resolution subtitle — answers "what size capture
                     will I get?" before the click. Mono digits, tabular
                     numerals so the column visually aligns across rows
                     even when widths differ by an order of magnitude. -->
                <span class="subtitle">{res}</span>
              {/if}
            </span>
          </button>
        </li>
      {/each}
    </ul>
  {/if}

  <footer class="foot">
    <!-- Kbd glyphs are inserted via `{@html …replace(…)}` so a single
         translated sentence can hold three substitutions without
         needing a templating helper. -->
    <p class="hint">
      {@html t()
        .pickerHint.replace("{up}", '<kbd class="kbd kbd-inline">↑</kbd>')
        .replace("{down}", '<kbd class="kbd kbd-inline">↓</kbd>')
        .replace("{enter}", '<kbd class="kbd kbd-inline">Enter</kbd>')
        .replace("{esc}", '<kbd class="kbd kbd-inline">Esc</kbd>')}
    </p>
  </footer>
</main>

<style>
  :global(html), :global(body) {
    margin: 0;
    padding: 0;
    background: var(--color-surface-0);
    color: var(--color-fg);
    overflow: hidden;
    user-select: none;
    -webkit-user-select: none;
  }
  :global(button) {
    appearance: none;
    -webkit-appearance: none;
    background: transparent;
    border: none;
    font: inherit;
    color: inherit;
    cursor: pointer;
  }

  .root {
    height: 100vh;
    display: flex;
    flex-direction: column;
    padding: 12px 12px 10px;
    box-sizing: border-box;
  }

  /* Header: search input + live counter on a single row. The input
   * is the prompt — placeholder text doubles as the panel's title.
   * Saves ~24 px of vertical space vs a separate <h1>, which we use
   * for an extra ~3 visible rows on the same 480 px window height. */
  .head {
    flex: 0 0 auto;
    padding: 2px 2px 8px;
  }
  .search-row {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .search {
    flex: 1 1 auto;
    height: 28px;
    padding: 0 10px;
    background: var(--color-surface-input);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius-sm);
    color: var(--color-fg);
    font-family: inherit;
    font-size: var(--text-sm);
    outline: none;
    transition:
      border-color var(--duration-quick) var(--ease-in-out-soft),
      background var(--duration-quick) var(--ease-in-out-soft);
  }
  .search:focus,
  .search:focus-visible {
    border-color: var(--color-border-accent);
  }
  .search::placeholder {
    color: var(--color-fg-placeholder);
  }
  /* Counter pill — tabular-nums so the digits never reflow when the
   * count ticks between e.g. "9 / 12" and "10 / 12". */
  .count {
    flex: 0 0 auto;
    font-size: var(--text-xs);
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    color: var(--color-fg-subtle);
    padding: 0 6px;
    min-width: 24px;
    text-align: right;
  }

  /* Scrollable list region. `min-height: 0` is the standard fix for
   * a flex child that should shrink under overflow — without it the
   * list pushes the footer off-window. */
  .list {
    list-style: none;
    margin: 0;
    padding: 2px;
    flex: 1 1 auto;
    min-height: 0;
    overflow-y: auto;
    overflow-x: hidden;
    border-radius: var(--radius-sm);
  }

  /* Each row is a `<button>` inside a `<li>` so keyboard focus +
   * Enter activation is free — the window-level listener handles
   * arrow navigation, and the button handles a click + Enter when
   * tabbed-to. Width 100% so the button fills its `<li>` slot.
   * Two-line row: ~48 px = 32 px icon + 8 px padding × 2. */
  .row {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 10px;
    min-height: 48px;
    padding: 6px 10px;
    border-radius: var(--radius-sm);
    cursor: pointer;
    color: var(--color-fg);
    text-align: left;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
  }
  /* Single visual state for hover *and* keyboard selection — the row
   * lights up via the `.selected` class which mouseenter also sets,
   * so the two input modes never fight each other. */
  .row.selected {
    background: var(--color-accent-bg-subtle);
    color: var(--color-accent-fg);
  }
  .row:active {
    background: var(--color-accent-bg-strong);
  }

  .icon {
    flex: 0 0 auto;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 32px;
    height: 32px;
    color: var(--color-fg-muted);
  }
  /* Real icons keep the source's color — don't recolor on selection
   * (an app's blue icon turning accent-fg would be jarring). The
   * lucide placeholder still recolors via `.row.selected .icon`. */
  .icon-img {
    width: 32px;
    height: 32px;
    object-fit: contain;
    image-rendering: -webkit-optimize-contrast;
  }
  .row.selected .icon {
    color: var(--color-accent-fg);
  }

  /* Two-line text block: title + optional resolution subtitle. The
   * outer flex column owns the min-width: 0 so ellipsis on `.title`
   * actually triggers (without it the column would size to content
   * and never overflow the row). */
  .text {
    flex: 1 1 auto;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
    justify-content: center;
  }
  .title {
    font-size: var(--text-sm);
    line-height: 1.2;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* Resolution subtitle. Mono + tabular-nums so digits in the
   * "1920 × 1080" column visually line up across rows. Quieter than
   * the title so the eye reads title → subtitle. Doesn't recolor on
   * selection — it's metadata, not the primary affordance. */
  .subtitle {
    font-size: var(--text-xs);
    line-height: 1.2;
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    color: var(--color-fg-subtle);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .row.selected .subtitle {
    color: var(--color-accent-fg);
    opacity: 0.75;
  }

  /* Empty state — used for both "no capturable windows" and "no
   * match for query". Centered icon + a single line + (optional)
   * subtler hint below. Reads as "looked, found nothing" rather
   * than "broken". */
  .empty {
    flex: 1 1 auto;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 8px;
    padding: 20px;
    color: var(--color-fg-subtle);
  }
  .empty p {
    margin: 0;
    font-size: var(--text-sm);
    text-align: center;
  }
  /* More specific than `.empty p` so the hint shrinks without
   * `!important`. Lives a tier below the primary "no match" message,
   * which carries the user-typed query and stays at body size. */
  .empty p.empty-hint {
    font-size: var(--text-xs);
    color: var(--color-fg-subtle);
  }

  /* Footer hint — same kbd style as the all-in-one menu so the two
   * surfaces visually rhyme (both are centered-panel pickers). */
  .foot {
    flex: 0 0 auto;
    padding: 8px 6px 2px;
    border-top: 1px solid var(--color-border-subtle);
    margin-top: 6px;
  }
  .hint {
    margin: 0;
    font-size: var(--text-xs);
    color: var(--color-fg-subtle);
    text-align: center;
  }
  :global(.kbd-inline) {
    display: inline-flex;
    align-items: center;
    height: 16px;
    padding: 0 4px;
    margin: 0 2px;
    background: var(--color-surface-2);
    border: 1px solid var(--color-border-subtle);
    border-bottom-width: 2px;
    border-radius: var(--radius-xs);
    font-family: var(--font-mono);
    font-size: 10px;
    line-height: 1;
    color: var(--color-fg-muted);
    vertical-align: middle;
  }
</style>
