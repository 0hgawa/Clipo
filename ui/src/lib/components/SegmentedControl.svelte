<script lang="ts" generics="T extends string | number">
  /**
   * Segmented control — pill of mutually-exclusive options (iOS /
   * macOS / Win11 style). Used by date / kind filters in History
   * and the FPS / upload-service chips in Settings.
   *
   * The chrome was duplicated 4 ways across HistoryPage + SettingsPage
   * (same CSS, same markup) before this extraction. Same value type
   * across callers (T constrained to `string | number` so the only
   * three current consumers — string union, string union, number —
   * all fit without coercion at the call site).
   */
  type Option = { value: T; label: string };
  type Props = {
    value: T;
    options: readonly Option[];
    onchange: (next: T) => void;
    ariaLabel?: string;
    disabled?: boolean;
  };
  let { value, options, onchange, ariaLabel, disabled = false }: Props = $props();
</script>

<div class="chips" role="group" aria-label={ariaLabel}>
  {#each options as opt (opt.value)}
    <button
      type="button"
      class="chip"
      class:active={value === opt.value}
      aria-pressed={value === opt.value}
      {disabled}
      onclick={() => onchange(opt.value)}
    >{opt.label}</button>
  {/each}
</div>

<style>
  /* Segmented control (iOS/macOS style) — accent-tinted active state
   * matches the canonical `.btn.active` + the editor's `.tools button.on`
   * so the "this is the active choice" cue reads the same way across
   * every surface. */
  .chips {
    display: inline-flex;
    background: var(--color-surface-1);
    border-radius: var(--radius-full);
    padding: 2px;
    gap: 2px;
  }
  .chip {
    height: 24px;
    padding: 0 12px;
    border: none;
    background: transparent;
    color: var(--color-fg-muted);
    font-family: inherit;
    font-size: var(--text-sm);
    border-radius: var(--radius-full);
    cursor: pointer;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
  }
  /* Hover on inactive chips picks up a `surface-2` fill so the
   * affordance matches `Button` / picker triggers — colour-only
   * hover was a duplicate of the disabled state at a glance and
   * read less responsive than the rest of the toolbar. `.active`
   * is excluded so the accent fill never gets overwritten. */
  .chip:hover:not(.active):not(:disabled) {
    background: var(--color-surface-2);
    color: var(--color-fg);
  }
  .chip:disabled {
    cursor: not-allowed;
    opacity: 0.6;
  }
  .chip.active {
    background: var(--color-accent-bg-subtle);
    color: var(--color-accent-fg);
  }
</style>
