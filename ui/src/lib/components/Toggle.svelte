<script lang="ts">
  /**
   * Pill switch used for any on/off setting. Two-state, no
   * indeterminate. Render the label OUTSIDE so the parent controls
   * layout (Settings uses a two-column row; Resize wants an inline
   * "<Toggle/> <label>" pair).
   *
   * Use `bind:checked={x}` on the parent — Svelte 5 wires it up
   * automatically via the `$bindable` prop.
   */
  type Props = {
    checked?: boolean;
    disabled?: boolean;
    ariaLabel?: string;
    onchange?: (next: boolean) => void;
  };

  let {
    checked = $bindable(false),
    disabled = false,
    ariaLabel,
    onchange,
  }: Props = $props();

  function toggle() {
    if (disabled) return;
    checked = !checked;
    onchange?.(checked);
  }
</script>

<button
  type="button"
  class="toggle"
  class:on={checked}
  {disabled}
  role="switch"
  aria-checked={checked}
  aria-label={ariaLabel}
  onclick={toggle}
>
  <span class="knob"></span>
</button>

<style>
  /* Windows 11 Fluent toggle: 40×20 pill, 12px knob, ~4px inset.
   * Off has a perceptible border; On goes solid accent. */
  .toggle {
    flex-shrink: 0;
    position: relative;
    width: 40px;
    height: 20px;
    border-radius: var(--radius-full);
    border: 1px solid var(--color-border-strong);
    background: transparent;
    cursor: pointer;
    padding: 0;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      border-color var(--duration-quick) var(--ease-in-out-soft);
  }
  .toggle:hover:not(:disabled) {
    background: var(--color-surface-1);
  }
  .toggle .knob {
    position: absolute;
    top: 50%;
    left: 3px;
    width: 12px;
    height: 12px;
    margin-top: -6px;
    background: var(--color-fg-muted);
    border-radius: 50%;
    transition:
      left var(--duration-base) var(--ease-in-out-soft),
      background var(--duration-quick) var(--ease-in-out-soft);
  }
  .toggle.on {
    background: var(--color-accent);
    border-color: var(--color-accent);
  }
  .toggle.on .knob {
    left: 23px;
    background: #fff;
  }
  .toggle.on:hover:not(:disabled) {
    background: var(--color-accent-hover);
    border-color: var(--color-accent-hover);
  }
  .toggle:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
</style>
