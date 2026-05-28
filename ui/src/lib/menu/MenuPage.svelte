<script lang="ts">
  /**
   * All-in-one menu.
   *
   * Pre-declared `menu` window centered on the primary monitor.
   * Opens via tray or Ctrl+Shift+K. Picks one of six cards and asks
   * the daemon to dispatch it via `menu_pick` — the daemon hides this
   * window and runs the corresponding action so behaviour stays
   * identical across surfaces.
   *
   * Dismiss = ESC or focus loss.
   */
  import { invoke } from "@tauri-apps/api/core";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import {
    AppWindow,
    Crop,
    Monitor,
    ScanText,
    Settings as SettingsIcon,
    Timer,
    Video,
  } from "@lucide/svelte";
  import { onDestroy, onMount } from "svelte";
  import type { Component } from "svelte";
  import type { UnlistenFn } from "@tauri-apps/api/event";
  import { dismissOffscreen } from "../dismissOffscreen";
  import { initLocaleSync, t } from "../i18n/index.svelte";

  type Action =
    | "region"
    | "fullscreen"
    | "window"
    | "record-fullscreen"
    | "timer"
    | "ocr"
    | "settings";
  type Card = {
    action: Action;
    label: string;
    icon: Component;
  };

  /** Six action cards laid out 3×2 — top row is single-frame image
   * captures (region/fullscreen/window), bottom row is recording +
   * tools (record-screen/timer/ocr). Settings lives as a ghost gear
   * button in the top-right corner where settings affordances live
   * in every other surface (CleanShot X, Loom, Linear, Raycast). The
   * history grid moved off this menu — it's still accessible via the
   * tray and surfaces its recents through Quick Access, so the menu
   * stays focused on capture intents the user is actively triggering.
   * Labels flow through `t()` so a language flip re-renders the cards
   * in place. */
  const cards = $derived.by<Card[]>(() => [
    { action: "region", label: t().menuCardCaptureArea, icon: Crop },
    { action: "fullscreen", label: t().menuCardFullscreen, icon: Monitor },
    { action: "window", label: t().menuCardCaptureWindow, icon: AppWindow },
    { action: "record-fullscreen", label: t().menuCardRecordScreen, icon: Video },
    { action: "timer", label: t().menuCardTimer, icon: Timer },
    { action: "ocr", label: t().menuCardCaptureText, icon: ScanText },
  ]);

  const win = getCurrentWindow();
  let unlistenFocus: (() => void) | undefined;
  let unlistenLocale: UnlistenFn | undefined;

  const close = () => dismissOffscreen(win);

  async function pick(action: Action) {
    try {
      await invoke("menu_pick", { action });
    } catch (e) {
      console.error("menu_pick", e);
      await close();
    }
  }

  function onKey(event: KeyboardEvent) {
    if (event.key === "Escape") void close();
  }

  onMount(async () => {
    unlistenLocale = await initLocaleSync();
    unlistenFocus = await win.onFocusChanged(({ payload: focused }) => {
      if (!focused) void close();
    });
  });

  onDestroy(() => {
    unlistenFocus?.();
    unlistenLocale?.();
  });
</script>

<svelte:window onkeydown={onKey} />

<main class="root">
  <button
    type="button"
    class="settings-btn"
    onclick={() => pick("settings")}
    aria-label={t().menuOpenSettings}
    title={t().menuSettings}
  >
    <SettingsIcon size={16} strokeWidth={2} />
  </button>
  <header class="head">
    <h1>{t().menuHeader}</h1>
    <p class="hint">
      <!-- `Esc` is rendered as the kbd glyph so the surrounding copy
           can be a single translated string with one substitution. -->
      {@html t().menuHint.replace('{esc}', '<kbd class="kbd kbd-inline">Esc</kbd>')}
    </p>
  </header>
  <div class="grid">
    {#each cards as card, i (card.action)}
      <button
        class="card"
        onclick={() => pick(card.action)}
        type="button"
        style:--card-index={i}
      >
        <span class="icon">
          <card.icon size={20} />
        </span>
        <span class="label">{card.label}</span>
      </button>
    {/each}
  </div>
</main>

<style>
  .root {
    height: 100vh;
    display: flex;
    flex-direction: column;
    padding: 14px 14px 20px;
    box-sizing: border-box;
    position: relative;
  }

  /* Ghost gear in the top-right corner. Standard placement for
   * Settings affordances across desktop UIs — Cleanshot X, Loom,
   * Raycast, Linear all put it here. Absolute positioning keeps the
   * centered header text unaffected (the header stays a clean
   * symmetric title block). 28 px hit target with a 16 px icon —
   * just under the recording-bar ghost size (26) so it visually
   * recedes vs the action cards which are the primary affordance. */
  .settings-btn {
    position: absolute;
    top: 10px;
    right: 10px;
    width: 28px;
    height: 28px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    background: transparent;
    border: none;
    border-radius: var(--radius-sm);
    color: var(--color-fg-subtle);
    cursor: pointer;
    appearance: none;
    -webkit-appearance: none;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
    z-index: 1;
  }
  .settings-btn:hover {
    background: var(--color-surface-2);
    color: var(--color-fg);
  }
  .settings-btn:active {
    background: var(--color-surface-3);
  }
  .settings-btn:focus-visible {
    outline: 2px solid var(--color-border-accent);
    outline-offset: 1px;
  }

  .head {
    margin-bottom: 14px;
    text-align: center;
  }
  .head h1 {
    margin: 0;
    font-size: var(--text-lg);
    font-weight: 600;
    color: var(--color-fg);
    letter-spacing: -0.01em;
  }
  .head .hint {
    margin: 4px 0 0;
    font-size: var(--text-xs);
    color: var(--color-fg-subtle);
  }
  /* Lives inside `{@html t().menuHint.replace(...)}` so the class
   * gets injected at runtime — global selector lets Svelte's scoped
   * styles still target it.
   *
   * Sized one px below `--text-xs` (the host `.hint` size, 11 px) so
   * the keycap reads as inset inside the sentence: same-size glyphs
   * + a border would inflate the line-height and stand off the
   * surrounding text. The 10 px is intentional and lives only in
   * this one place. */
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

  /* 3 columns × 2 rows of cards, all the same size. With Settings
   * pulled out into the corner gear, the six capture intents fit a
   * clean rectangle — no orphan card, no special-case styling. */
  .grid {
    flex: 1;
    display: grid;
    grid-template-columns: repeat(3, 1fr);
    grid-auto-rows: 1fr;
    gap: 8px;
    min-height: 0;
  }

  .card {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 6px;
    padding: 8px;
    border: 1px solid var(--color-border-subtle);
    background: var(--color-surface-1);
    color: var(--color-fg);
    border-radius: var(--radius-md);
    cursor: pointer;
    text-align: center;
    min-width: 0;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      border-color var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
    /* Stagger fade-in driven by `--card-index` so the cards cascade
     * in (~35 ms apart). `backwards` keeps the initial opacity:0 in
     * effect while the delay is pending, so nothing flashes. */
    animation: card-in 180ms var(--ease-out-snappy) backwards;
    animation-delay: calc(var(--card-index, 0) * 35ms);
  }
  .card:hover {
    background: var(--color-surface-2);
    border-color: var(--color-border-accent);
  }
  .card:active {
    background: var(--color-surface-3);
  }
  /* Honour `prefers-reduced-motion`: drop the cascade so users who
   * disabled animations get the cards immediately. */
  @media (prefers-reduced-motion: reduce) {
    .card {
      animation: none;
    }
  }

  .icon {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 38px;
    height: 38px;
    border-radius: var(--radius-full);
    background: var(--color-surface-2);
    color: var(--color-fg-muted);
    flex-shrink: 0;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
  }
  /* Hover lift: the icon fills with accent + colour pulse. Strong
   * enough to read "this is the focused card" without the glow/
   * blur tricks CleanShot relies on. */
  .card:hover .icon {
    background: var(--color-accent-bg-strong);
    color: var(--color-accent-fg);
  }

  .label {
    font-size: var(--text-md);
    font-weight: 500;
    line-height: var(--leading-normal);
    max-width: 100%;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  @keyframes card-in {
    from {
      opacity: 0;
      transform: translateY(4px);
    }
    to {
      opacity: 1;
      transform: translateY(0);
    }
  }
</style>
