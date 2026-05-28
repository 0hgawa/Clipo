<script lang="ts">
  /**
   * Stroke-width picker — pill trigger shows the current stroke as
   * a short line preview, click opens a popover with presets (each
   * rendered as a line of that thickness) + custom-px input.
   *
   * Line preview > dot preview: stroke width applies to lines
   * (arrow shaft, rect border, pen path), so showing a line is
   * literal. Matches Excalidraw / Photoshop / Figma convention.
   */
  import { Check } from "@lucide/svelte";
  import { outsideDismiss } from "../components/outsideDismiss";

  type Props = {
    value?: number;
    options?: readonly number[];
    onchange?: (width: number) => void;
    ariaLabel?: string;
  };

  let {
    value = $bindable(6),
    options = [2, 4, 6, 10, 16],
    onchange,
    ariaLabel = "Stroke width",
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

  /** Preview thickness clamped so it stays inside the trigger / row
   * even when the picker holds a thicker stroke. Below 1px the line
   * is invisible, so we floor it visually too. */
  function previewHeight(n: number): number {
    return Math.max(1, Math.min(12, n));
  }
</script>

<div class="wp">
  <button
    bind:this={trigger}
    type="button"
    class="wp-trigger"
    onclick={() => (open = !open)}
    title={ariaLabel}
    aria-label={ariaLabel}
    aria-expanded={open}
  >
    <span class="wp-line" style:height="{previewHeight(value)}px"></span>
    <span class="wp-value">{value}</span>
  </button>

  {#if open}
    <div
      class="wp-panel"
      role="dialog"
      aria-label={ariaLabel}
      use:outsideDismiss={{ trigger, onDismiss: () => (open = false) }}
    >
      <div class="wp-list">
        {#each options as n (n)}
          <button
            type="button"
            class="wp-option"
            class:current={isCurrent(n)}
            onclick={() => pick(n)}
            title="{n}px"
          >
            <span class="wp-check">
              {#if isCurrent(n)}<Check size={10} strokeWidth={3} />{/if}
            </span>
            <span class="wp-line" style:height="{previewHeight(n)}px"></span>
            <span class="wp-num">{n}</span>
          </button>
        {/each}
      </div>
    </div>
  {/if}
</div>

<style>
  .wp {
    position: relative;
    display: inline-flex;
  }

  /* Trigger: line preview on the left, numeric value on the right. */
  .wp-trigger {
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
    gap: 8px;
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    transition: background var(--duration-quick) var(--ease-in-out-soft);
  }
  .wp-trigger:hover {
    background: var(--color-surface-2);
  }
  .wp-trigger[aria-expanded="true"] {
    background: var(--color-accent-bg-subtle);
    color: var(--color-accent-fg);
  }
  .wp-line {
    display: inline-block;
    width: 18px;
    border-radius: 999px;
    background: var(--color-fg);
    flex-shrink: 0;
  }
  .wp-value {
    font-size: var(--text-xs);
    color: var(--color-fg-muted);
  }

  /* Panel — wider than SizePicker because each row is line + number. */
  .wp-panel {
    position: absolute;
    top: calc(100% + 8px);
    left: 50%;
    transform: translateX(-50%);
    z-index: 30;
    width: 168px;
    background: var(--color-surface-1);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-lg);
    padding: 6px;
    animation: wp-pop var(--duration-quick) var(--ease-out-snappy);
  }

  .wp-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .wp-option {
    height: 30px;
    padding: 0 8px;
    border: none;
    background: transparent;
    color: var(--color-fg);
    border-radius: var(--radius-xs);
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    gap: 10px;
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
    transition: background var(--duration-quick) var(--ease-in-out-soft);
  }
  .wp-option:hover {
    background: var(--color-surface-2);
  }
  .wp-option.current {
    background: var(--color-surface-2);
  }
  .wp-check {
    width: 12px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: var(--color-accent);
  }
  .wp-option .wp-line {
    flex: 1;
    width: auto;
  }
  .wp-num {
    font-size: var(--text-xs);
    color: var(--color-fg-muted);
    min-width: 22px;
    text-align: right;
  }

  @keyframes wp-pop {
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
