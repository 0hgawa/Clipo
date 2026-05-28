<script lang="ts">
  /**
   * Centred status / empty / error / loading message.
   * Drops into any container that flex-grows to fill space (e.g. a
   * surface's main scroll area when there's nothing to show).
   *
   * Props:
   * - `title`: short headline, e.g. "No captures yet".
   * - `reason`: optional secondary line (long-form explanation,
   *   error detail, hint). Wraps inside a 360px max-width column.
   * - `variant`: `"info"` (default) tints the title white;
   *   `"error"` tints it red.
   * - `loading`: when true, prepends a spinning Loader icon — used
   *   for OCR's "Recognizing…" state. Defaults to false.
   *
   * Examples:
   *   <StatusMessage loading title="Recognizing…" />
   *   <StatusMessage variant="error" title="OCR failed" reason={msg} />
   *   <StatusMessage title="No text found" reason="Image has no text." />
   */
  import { Loader } from "@lucide/svelte";

  type Variant = "info" | "error";

  let {
    variant = "info" as Variant,
    loading = false,
    title,
    reason,
  }: {
    variant?: Variant;
    loading?: boolean;
    title: string;
    reason?: string;
  } = $props();
</script>

<div class="state" class:error={variant === "error"} role="status" aria-live="polite">
  {#if loading}
    <Loader class="status-spinner" size={20} />
  {/if}
  <p class="title">{title}</p>
  {#if reason}
    <p class="reason">{reason}</p>
  {/if}
</div>

<style>
  .state {
    flex: 1;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 6px;
    padding: 24px;
    color: var(--color-fg-muted);
    text-align: center;
  }

  .title {
    margin: 0;
    color: var(--color-fg);
    font-size: var(--text-md);
    font-weight: 600;
  }
  .state.error .title {
    color: var(--color-danger-hover);
  }

  .reason {
    margin: 0;
    color: var(--color-fg-muted);
    font-size: var(--text-sm);
    max-width: 360px;
    line-height: var(--leading-normal);
  }

  /* Lucide renders into an SVG outside this component's scope; the
   * `:global` is what lets the class actually apply. The class name is
   * specific enough to avoid collisions elsewhere. */
  :global(.status-spinner) {
    animation: status-spin 900ms linear infinite;
    color: var(--color-fg-muted);
  }
  @keyframes status-spin {
    to {
      transform: rotate(360deg);
    }
  }
</style>
