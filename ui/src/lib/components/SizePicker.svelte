<script lang="ts">
  /**
   * Size picker — pill trigger showing the current value, click opens
   * a popover with a grid of presets and a custom-number input.
   *
   * Same UX shape as `ColorPicker`: single trigger on the toolbar,
   * popover anchored below, click-outside / Esc closes.
   *
   * Reusable for any "pick a numeric preset or type a custom one"
   * scenario — currently used for the text tool's font size; future
   * uses (stroke width, radius, etc.) can drop it in.
   */
  import { Check } from "@lucide/svelte";
  import { outsideDismiss } from "./outsideDismiss";

  type Props = {
    value?: number;
    options?: readonly number[];
    /** Unit label rendered next to the trigger value (e.g. "px"). */
    unit?: string;
    onchange?: (size: number) => void;
    ariaLabel?: string;
  };

  let {
    value = $bindable(16),
    options = [12, 16, 20, 24, 32, 48, 64, 96, 128],
    unit = "px",
    onchange,
    ariaLabel = "Size",
  }: Props = $props();

  let open = $state(false);
  let trigger = $state<HTMLButtonElement | undefined>(undefined);

  function pick(n: number) {
    value = n;
    onchange?.(n);
    open = false;
  }

  function isCurrent(n: number): boolean {
    return n === value;
  }
</script>

<div class="sp">
  <button
    bind:this={trigger}
    type="button"
    class="sp-trigger"
    onclick={() => (open = !open)}
    title={ariaLabel}
    aria-label={ariaLabel}
    aria-expanded={open}
  >
    <span class="sp-value">{value}</span>
    <span class="sp-unit">{unit}</span>
  </button>

  {#if open}
    <div
      class="sp-panel"
      role="dialog"
      aria-label={ariaLabel}
      use:outsideDismiss={{ trigger, onDismiss: () => (open = false) }}
    >
      <div class="sp-list">
        {#each options as n (n)}
          <button
            type="button"
            class="sp-option"
            class:current={isCurrent(n)}
            onclick={() => pick(n)}
            title="{n}{unit}"
          >
            <span class="sp-check">
              {#if isCurrent(n)}<Check size={10} strokeWidth={3} />{/if}
            </span>
            <span class="sp-num">{n}</span>
            <span class="sp-unit-inline">{unit}</span>
          </button>
        {/each}
      </div>

    </div>
  {/if}
</div>

<style>
  .sp {
    position: relative;
    display: inline-flex;
  }

  /* Trigger — matches StrokePicker's pill geometry exactly so the
   * two appear visually paired in the toolbar. */
  .sp-trigger {
    height: 28px;
    min-width: 60px;
    padding: 0 10px;
    border: none;
    background: var(--color-surface-1);
    color: var(--color-fg);
    border-radius: var(--radius-sm);
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 4px;
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    transition: background var(--duration-quick) var(--ease-in-out-soft);
  }
  .sp-trigger:hover {
    background: var(--color-surface-2);
  }
  .sp-trigger[aria-expanded="true"] {
    background: var(--color-accent-bg-subtle);
    color: var(--color-accent-fg);
  }
  /* Same font-size for both value and unit prevents the optical
   * misalignment that comes from mixing two text sizes on one line
   * (baselines diverge, the smaller one looks floated). Distinction
   * is by colour only. */
  .sp-value {
    font-size: var(--text-sm);
    color: var(--color-fg);
  }
  .sp-unit {
    font-size: var(--text-sm);
    color: var(--color-fg-muted);
  }

  /* Panel — narrow vertical list (one size per row). Centered below
   * the trigger so it always reads, regardless of where the trigger
   * lives on the toolbar. */
  .sp-panel {
    position: absolute;
    top: calc(100% + 8px);
    left: 50%;
    transform: translateX(-50%);
    z-index: 30;
    width: 110px;
    background: var(--color-surface-1);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-lg);
    padding: 6px;
    animation: sp-pop var(--duration-quick) var(--ease-out-snappy);
  }

  /* Identical row geometry to StrokePicker so the two dropdowns
   * feel like a single design language. */
  .sp-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .sp-option {
    height: 30px;
    padding: 0 8px;
    border: none;
    background: transparent;
    color: var(--color-fg);
    border-radius: var(--radius-xs);
    cursor: pointer;
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    display: inline-flex;
    align-items: center;
    gap: 10px;
    transition: background var(--duration-quick) var(--ease-in-out-soft);
  }
  .sp-option:hover {
    background: var(--color-surface-2);
  }
  .sp-option.current {
    background: var(--color-surface-2);
  }
  .sp-check {
    width: 12px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: var(--color-accent);
    flex-shrink: 0;
  }
  .sp-num {
    flex: 1;
    font-size: var(--text-sm);
    text-align: left;
  }
  .sp-unit-inline {
    color: var(--color-fg-muted);
    font-size: var(--text-xs);
    min-width: 22px;
    text-align: right;
  }

  @keyframes sp-pop {
    from {
      opacity: 0;
      transform: translate(-50%, -3px);
    }
    to {
      opacity: 1;
      transform: translate(-50%, 0);
    }
  }
</style>
