<script lang="ts">
  /**
   * Font-family picker — pill trigger shows the current font label,
   * click opens a popover listing the available fonts. Each row in
   * the list is rendered with its own font so the user previews the
   * look before committing.
   *
   * Same shape as SizePicker / StrokePicker (no border, ghost
   * background, accent-tinted open state) so the toolbar reads as
   * one consistent design language.
   */
  import { Check } from "@lucide/svelte";
  import { outsideDismiss } from "./outsideDismiss";

  type FontOption = { label: string; value: string };

  type Props = {
    value?: string;
    options: readonly FontOption[];
    onchange?: (font: string) => void;
    ariaLabel?: string;
  };

  let {
    value = $bindable(""),
    options,
    onchange,
    ariaLabel = "Font",
  }: Props = $props();

  let open = $state(false);
  let trigger = $state<HTMLButtonElement | undefined>(undefined);

  function pick(v: string) {
    value = v;
    onchange?.(v);
    open = false;
  }

  function currentLabel(): string {
    return options.find((o) => o.value === value)?.label ?? "—";
  }

  function isCurrent(v: string): boolean {
    return v === value;
  }
</script>

<div class="fp">
  <button
    bind:this={trigger}
    type="button"
    class="fp-trigger"
    onclick={() => (open = !open)}
    title={ariaLabel}
    aria-label={ariaLabel}
    aria-expanded={open}
  >
    <span class="fp-label">{currentLabel()}</span>
  </button>

  {#if open}
    <div
      class="fp-panel"
      role="dialog"
      aria-label={ariaLabel}
      use:outsideDismiss={{ trigger, onDismiss: () => (open = false) }}
    >
      <div class="fp-list">
        {#each options as opt (opt.value)}
          <button
            type="button"
            class="fp-option"
            class:current={isCurrent(opt.value)}
            onclick={() => pick(opt.value)}
            title={opt.label}
          >
            <span class="fp-check">
              {#if isCurrent(opt.value)}<Check size={10} strokeWidth={3} />{/if}
            </span>
            <span class="fp-preview" style:font={`14px ${opt.value}`}>
              {opt.label}
            </span>
          </button>
        {/each}
      </div>
    </div>
  {/if}
</div>

<style>
  .fp {
    position: relative;
    display: inline-flex;
  }

  /* Trigger — matches Size / Stroke pickers exactly. */
  .fp-trigger {
    height: 28px;
    min-width: 90px;
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
    font-family: inherit;
    transition: background var(--duration-quick) var(--ease-in-out-soft);
  }
  .fp-trigger:hover {
    background: var(--color-surface-2);
  }
  .fp-trigger[aria-expanded="true"] {
    background: var(--color-accent-bg-subtle);
    color: var(--color-accent-fg);
  }
  .fp-label {
    font-size: var(--text-sm);
  }

  .fp-panel {
    position: absolute;
    top: calc(100% + 8px);
    left: 50%;
    transform: translateX(-50%);
    z-index: 30;
    width: 160px;
    background: var(--color-surface-1);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-lg);
    padding: 6px;
    animation: fp-pop var(--duration-quick) var(--ease-out-snappy);
  }

  /* Same row geometry as SizePicker / StrokePicker option rows. */
  .fp-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .fp-option {
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
    transition: background var(--duration-quick) var(--ease-in-out-soft);
  }
  .fp-option:hover {
    background: var(--color-surface-2);
  }
  .fp-option.current {
    background: var(--color-surface-2);
  }
  .fp-check {
    width: 12px;
    flex-shrink: 0;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: var(--color-accent);
  }
  /* Live font preview — the row text is set in the actual font,
   * so the user sees "Calibri" in Calibri, "Mono" in mono, etc. */
  .fp-preview {
    flex: 1;
    text-align: left;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  @keyframes fp-pop {
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
