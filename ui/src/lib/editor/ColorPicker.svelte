<script lang="ts">
  /**
   * Color picker — single circular trigger on the toolbar opens a
   * drop-down panel anchored to the top of the editor window.
   *
   * Designed for screenshot annotation, not graphic design:
   *  - 12 curated swatches (warm row → cool row) — fast pick.
   *  - Native OS picker for "any other colour".
   *  - Hex input for paste-from-Figma cases.
   *
   * Closes on ESC, click outside, or picking a preset.
   */
  import { Check } from "@lucide/svelte";
  import { outsideDismiss } from "../components/outsideDismiss";

  type Props = {
    value?: string;
    onchange?: (color: string) => void;
    ariaLabel?: string;
  };

  let {
    value = $bindable("#ff1744"),
    onchange,
    ariaLabel = "Pick a colour",
  }: Props = $props();

  /** 12 curated swatches — full chroma colours that read on any
   * background, ordered by hue. Two rows of six fits in the panel
   * width without crowding. */
  const SWATCHES = [
    // Warm row
    "#ff1744", // red
    "#ff6e40", // orange
    "#ffbf3c", // amber
    "#ffeb3b", // yellow
    "#9ccc65", // lime
    "#3ed184", // green
    // Cool row
    "#1de9b6", // teal
    "#5b9eff", // blue
    "#7c4dff", // indigo
    "#b95bff", // purple
    "#000000", // black
    "#ffffff", // white
  ] as const;

  let open = $state(false);
  let hex = $state("");
  let trigger = $state<HTMLButtonElement | undefined>(undefined);

  function pick(c: string) {
    const normalized = c.toLowerCase();
    value = normalized;
    onchange?.(normalized);
    open = false;
  }

  function commitHex() {
    const cleaned = hex.trim();
    // Accept both 6-char (#ff8800) and 3-char (#f80) shorthand —
    // matches what users copy/paste out of Figma, Tailwind classes,
    // etc. 3-char expands by duplicating each nibble (f→ff).
    const six = cleaned.match(/^#?([a-fA-F0-9]{6})$/);
    const three = cleaned.match(/^#?([a-fA-F0-9]{3})$/);
    let raw: string | null = null;
    if (six && six[1]) raw = six[1];
    else if (three && three[1])
      raw = three[1]
        .split("")
        .map((c) => c + c)
        .join("");
    if (raw) {
      const normalized = `#${raw.toLowerCase()}`;
      value = normalized;
      hex = raw.toLowerCase();
      onchange?.(normalized);
    } else {
      hex = stripHash(value);
    }
  }

  function onHexKeyDown(ev: KeyboardEvent) {
    if (ev.key === "Enter") {
      ev.preventDefault();
      commitHex();
    } else if (ev.key === "Escape") {
      ev.preventDefault();
      hex = stripHash(value);
      open = false;
    }
  }

  /** Native picker `oninput` fires live as the user drags through
   * the OS dialog — instant preview. */
  function onNativeInput(c: string) {
    const normalized = c.toLowerCase();
    value = normalized;
    hex = stripHash(normalized);
    onchange?.(normalized);
  }

  function stripHash(c: string): string {
    return c.startsWith("#") ? c.slice(1) : c;
  }

  function isCurrent(c: string): boolean {
    return c.toLowerCase() === value.toLowerCase();
  }

  // Refresh the hex input every time the popover opens so external
  // changes to `value` (e.g. picking a new color from the swatches
  // grid) propagate into the text field on the next open.
  $effect(() => {
    if (open) hex = stripHash(value);
  });
</script>

<div class="cp">
  <button
    bind:this={trigger}
    type="button"
    class="cp-trigger"
    style:--swatch={value}
    onclick={() => (open = !open)}
    title={ariaLabel}
    aria-label={ariaLabel}
    aria-expanded={open}
  ></button>

  {#if open}
    <div
      class="cp-panel"
      role="dialog"
      aria-label="Colour options"
      use:outsideDismiss={{ trigger, onDismiss: () => (open = false) }}
    >
      <div class="cp-grid">
        {#each SWATCHES as c (c)}
          <button
            type="button"
            class="cp-swatch"
            class:current={isCurrent(c)}
            style:--swatch={c}
            onclick={() => pick(c)}
            title={c}
            aria-label={c}
          >
            {#if isCurrent(c)}
              <Check size={10} strokeWidth={3} />
            {/if}
          </button>
        {/each}
      </div>

      <div class="cp-divider"></div>

      <div class="cp-custom">
        <label
          class="cp-native"
          style:--swatch={value}
          title="More colours"
          aria-label="Open OS colour picker"
        >
          <input
            type="color"
            value={value}
            oninput={(ev) => onNativeInput((ev.target as HTMLInputElement).value)}
          />
        </label>
        <div class="cp-hex">
          <span class="cp-hash">#</span>
          <input
            type="text"
            bind:value={hex}
            onkeydown={onHexKeyDown}
            onblur={commitHex}
            maxlength="7"
            spellcheck="false"
            aria-label="Hex colour"
            placeholder="ffffff"
          />
        </div>
      </div>
    </div>
  {/if}
</div>

<style>
  .cp {
    position: relative;
    display: inline-flex;
  }

  /* ─── Trigger: single circular swatch ──────────────────────────
   * The only thing visible in the toolbar — current colour at a
   * glance, click opens the panel. */
  .cp-trigger {
    width: 26px;
    height: 26px;
    border-radius: var(--radius-full);
    background: var(--swatch);
    border: 2px solid rgba(255, 255, 255, 0.18);
    cursor: pointer;
    padding: 0;
    transition:
      transform var(--duration-quick) var(--ease-in-out-soft),
      box-shadow var(--duration-quick) var(--ease-in-out-soft);
  }
  .cp-trigger:hover {
    transform: scale(1.08);
  }
  .cp-trigger[aria-expanded="true"] {
    box-shadow: 0 0 0 2px var(--color-accent);
  }

  /* ─── Panel ────────────────────────────────────────────────────
   * Anchored top-left of the trigger; floats above whatever's in
   * the canvas with a heavy shadow + dark surface. */
  .cp-panel {
    position: absolute;
    top: calc(100% + 8px);
    left: 50%;
    transform: translateX(-50%);
    z-index: 30;
    width: 232px;
    background: var(--color-surface-1);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-lg);
    padding: 12px;
    animation: cp-pop var(--duration-quick) var(--ease-out-snappy);
  }

  /* ─── Grid of presets ──────────────────────────────────────────
   * 12 colours in 2 rows of 6: warm row, cool row. Each swatch is
   * a touch larger (28px) and round so the panel feels like a real
   * picker, not a tiny popover. */
  .cp-grid {
    display: grid;
    grid-template-columns: repeat(6, 1fr);
    gap: 8px;
  }
  .cp-swatch {
    aspect-ratio: 1;
    border-radius: var(--radius-full);
    background: var(--swatch);
    border: 1.5px solid rgba(255, 255, 255, 0.14);
    cursor: pointer;
    padding: 0;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: #000;
    transition:
      transform var(--duration-quick) var(--ease-in-out-soft),
      box-shadow var(--duration-quick) var(--ease-in-out-soft);
  }
  .cp-swatch:hover {
    transform: scale(1.12);
  }
  .cp-swatch.current {
    box-shadow:
      0 0 0 2px var(--color-surface-1),
      0 0 0 4px var(--color-accent);
  }

  /* ─── Divider ──────────────────────────────────────────────── */
  .cp-divider {
    height: 1px;
    background: var(--color-border-subtle);
    margin: 12px -12px;
  }

  /* ─── Custom row: OS picker + hex input ────────────────────────
   * Native picker on the left (invisible <input type="color">
   * behind a themed swatch), hex input on the right. */
  .cp-custom {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .cp-native {
    position: relative;
    width: 28px;
    height: 28px;
    border-radius: var(--radius-sm);
    background: var(--swatch);
    border: 1.5px solid rgba(255, 255, 255, 0.18);
    cursor: pointer;
    flex-shrink: 0;
    overflow: hidden;
    transition: transform var(--duration-quick) var(--ease-in-out-soft);
  }
  .cp-native:hover {
    transform: scale(1.08);
  }
  .cp-native input[type="color"] {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    opacity: 0;
    cursor: pointer;
    padding: 0;
    border: none;
  }

  .cp-hex {
    flex: 1;
    display: flex;
    align-items: center;
    height: 28px;
    padding: 0 8px;
    background: var(--color-surface-0);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius-sm);
    gap: 4px;
  }
  .cp-hex:focus-within {
    border-color: var(--color-border-accent);
  }
  .cp-hash {
    color: var(--color-fg-muted);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
  }
  .cp-hex input {
    flex: 1;
    background: transparent;
    border: none;
    color: var(--color-fg);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    outline: none;
    padding: 0;
    min-width: 0;
    text-transform: lowercase;
  }
  .cp-hex input::placeholder {
    color: var(--color-fg-placeholder);
  }

  @keyframes cp-pop {
    from {
      opacity: 0;
      transform: translate(-50%, -4px);
    }
    to {
      opacity: 1;
      transform: translate(-50%, 0);
    }
  }
</style>
