<script lang="ts">
  /**
   * Fluent 2 / Win11 slider — 4 px track, 16 px filled-accent thumb
   * that shrinks to 12 px on press (the canonical Win11 cue) and gains
   * a tinted ring on hover/focus.
   *
   * Backed by a styled `<input type="range">` so we inherit the OS-
   * level slider contract for free: `role="slider"`, live `aria-
   * valuenow`, ←/→/Home/End/PgUp/PgDn stepping, pointer + touch
   * dragging, all without a single JS listener on the hot path.
   *
   * `oninput` fires continuously while dragging; `onchange` fires
   * once on release. Use `onchange` for anything that hits disk
   * (settings persist, IPC commit) so drags don't fan out.
   */
  type Props = {
    value?: number;
    min?: number;
    max?: number;
    step?: number;
    disabled?: boolean;
    ariaLabel?: string;
    /** Live updates during drag. */
    oninput?: (next: number) => void;
    /** Committed value on release / keyboard step. */
    onchange?: (next: number) => void;
  };

  let {
    value = $bindable(0),
    min = 0,
    max = 100,
    step = 1,
    disabled = false,
    ariaLabel,
    oninput,
    onchange,
  }: Props = $props();

  /* Drives the filled-track gradient stop via a CSS custom property.
   * Computed from the prop so external value writes (reset buttons,
   * preset chips, IPC echoes) repaint without re-mounting. */
  let percent = $derived(
    max === min ? 0 : ((value - min) / (max - min)) * 100,
  );

  function handleInput(ev: Event & { currentTarget: HTMLInputElement }) {
    const next = Number(ev.currentTarget.value);
    value = next;
    oninput?.(next);
  }
  function handleChange(ev: Event & { currentTarget: HTMLInputElement }) {
    onchange?.(Number(ev.currentTarget.value));
  }
</script>

<input
  type="range"
  class="slider"
  class:disabled
  style:--pct="{percent}%"
  {min}
  {max}
  {step}
  {value}
  {disabled}
  aria-label={ariaLabel}
  oninput={handleInput}
  onchange={handleChange}
/>

<style>
  /* Container — 20 px hit area matches `Toggle` so a stacked
   * "Toggle row / Slider row" layout in Settings keeps a clean
   * baseline. The track is centered inside this height. */
  .slider {
    -webkit-appearance: none;
    appearance: none;
    width: 100%;
    height: 20px;
    background: transparent;
    cursor: pointer;
    margin: 0;
    padding: 0;
    /* Block the browser's default focus outline — the thumb's own
     * ring + the global `:focus-visible` rule handle keyboard cue. */
    outline: none;
  }
  .slider.disabled {
    cursor: not-allowed;
    opacity: 0.45;
  }

  /* ── TRACK ──────────────────────────────────────────────────
   * Single gradient does both halves: accent up to `--pct`,
   * surface-3 after. Cheaper than two stacked elements and means
   * the filled portion updates by repainting the gradient stop
   * (no layout). */
  .slider::-webkit-slider-runnable-track {
    height: 4px;
    border-radius: var(--radius-full);
    background: linear-gradient(
      to right,
      var(--color-accent) 0,
      var(--color-accent) var(--pct),
      var(--color-surface-3) var(--pct),
      var(--color-surface-3) 100%
    );
    transition: background var(--duration-quick) var(--ease-in-out-soft);
  }
  .slider.disabled::-webkit-slider-runnable-track {
    background: var(--color-surface-3);
  }

  /* ── THUMB ──────────────────────────────────────────────────
   * 16 px resting, 12 px on :active — the Win11 Settings slider
   * does this exact shrink to signal "I'm being dragged". Margin-
   * top centers the thumb on the 4 px track ((4 − 16) / 2 = −6). */
  .slider::-webkit-slider-thumb {
    -webkit-appearance: none;
    appearance: none;
    width: 16px;
    height: 16px;
    margin-top: -6px;
    background: var(--color-accent);
    border: 3px solid var(--color-surface-0);
    border-radius: 50%;
    box-shadow: 0 0 0 1px var(--color-accent);
    cursor: pointer;
    transition:
      width var(--duration-quick) var(--ease-in-out-soft),
      height var(--duration-quick) var(--ease-in-out-soft),
      margin-top var(--duration-quick) var(--ease-in-out-soft),
      box-shadow var(--duration-quick) var(--ease-in-out-soft),
      background var(--duration-quick) var(--ease-in-out-soft);
  }
  .slider:hover:not(.disabled)::-webkit-slider-thumb,
  .slider:focus-visible::-webkit-slider-thumb {
    box-shadow:
      0 0 0 1px var(--color-accent),
      0 0 0 5px var(--color-accent-bg-subtle);
  }
  .slider:active:not(.disabled)::-webkit-slider-thumb {
    width: 12px;
    height: 12px;
    margin-top: -4px;
    background: var(--color-accent-pressed);
    box-shadow:
      0 0 0 1px var(--color-accent-pressed),
      0 0 0 6px var(--color-accent-bg-subtle);
  }
  .slider.disabled::-webkit-slider-thumb {
    background: var(--color-fg-muted);
    box-shadow: 0 0 0 1px var(--color-border-strong);
  }
</style>
