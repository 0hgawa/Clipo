<script lang="ts">
  /**
   * Shared action button. Use everywhere an icon / icon+text / text
   * action lives outside the editor's toggle-group (tools, widths,
   * swatches stay specialised in `EditorPage`).
   *
   * Sizes
   * - `md` (default): 28px tall — headers, action panels, dialogs.
   * - `sm`: 26px tall — dense rows (history grid, quick access).
   *
   * Pass `iconOnly` to switch from rectangular to square (width =
   * height) — saves you from styling padding per-call.
   *
   * Active toggle state is exposed via `active` so picker components
   * (tools, color swatches in future) can reuse the same look without
   * defining their own classes.
   */
  import type { Snippet } from "svelte";

  type Variant = "default" | "primary" | "danger";
  type Size = "md" | "sm";

  let {
    variant = "default" as Variant,
    size = "md" as Size,
    iconOnly = false,
    ghost = false,
    active = false,
    disabled = false,
    title,
    ariaLabel,
    onclick,
    children,
  }: {
    variant?: Variant;
    size?: Size;
    iconOnly?: boolean;
    /** Transparent at rest, surface-2 (or danger) on hover. Use in
     * dense / image-first contexts (lightbox, viewers) where the
     * buttons must not compete with the content. */
    ghost?: boolean;
    active?: boolean;
    disabled?: boolean;
    title?: string;
    ariaLabel?: string;
    onclick?: (ev: MouseEvent) => void;
    children: Snippet;
  } = $props();
</script>

<button
  type="button"
  class="btn"
  class:primary={variant === "primary"}
  class:danger={variant === "danger"}
  class:sm={size === "sm"}
  class:icon-only={iconOnly}
  class:ghost
  class:active
  {disabled}
  {title}
  aria-label={ariaLabel}
  {onclick}
>
  {@render children()}
</button>

<style>
  .btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    gap: 5px;
    /* 28 px matches the picker triggers (Size/Font), Editor tools,
     * and the bg-trigger — keeps every "trigger-ish" control on one
     * row optically aligned. Previous 30 px left a 2 px height
     * mismatch with the toolbar peers. */
    height: 28px;
    padding: 0 10px;
    border: none;
    background: var(--color-surface-1);
    color: var(--color-fg);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-size: var(--text-sm);
    font-family: inherit;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
  }
  .btn:hover:not(:disabled) {
    background: var(--color-surface-2);
  }
  .btn:active:not(:disabled) {
    background: var(--color-surface-3);
  }
  .btn:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }

  .btn.sm {
    height: 26px;
    padding: 0 8px;
    gap: 4px;
    font-size: var(--text-xs);
  }

  .btn.icon-only {
    padding: 0;
    width: 28px;
    color: var(--color-fg-muted);
  }
  .btn.icon-only:hover:not(:disabled) {
    color: var(--color-fg);
  }
  .btn.icon-only.sm {
    width: 26px;
  }

  /* Ghost — transparent base, surface-2 on hover. Stacks with any
   * variant (default / danger). Combined with `iconOnly` it gives
   * the "quiet toolbar" look used in viewers and lightboxes. */
  .btn.ghost {
    background: transparent;
  }
  .btn.ghost:hover:not(:disabled) {
    background: var(--color-surface-2);
  }

  .btn.active {
    background: var(--color-accent-bg-subtle);
    color: var(--color-accent-fg);
  }

  .btn.primary {
    background: var(--color-accent-bg-subtle);
    color: var(--color-accent-fg);
  }
  .btn.primary:hover:not(:disabled) {
    background: var(--color-accent-bg-strong);
    color: #fff;
  }

  .btn.danger:hover:not(:disabled) {
    background: var(--color-danger-bg-strong);
    color: var(--color-danger-hover);
  }
</style>
