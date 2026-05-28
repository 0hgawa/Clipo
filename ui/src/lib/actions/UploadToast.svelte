<script lang="ts">
  /**
   * Floating upload feedback toast.
   *
   * Stateless: caller owns the `UploadResult` and decides when to
   * reset it to idle. The toast is invisible when state is `idle`,
   * shows a copy-confirmation row on `done`, and a red one-liner on
   * `error`. `position` selects placement — `fixed` for the History
   * window bottom-centre, `absolute` for inside the Lightbox stage
   * (which uses its own absolute container).
   */
  import { Copy } from "@lucide/svelte";
  import type { UploadResult } from "./captureRegistry";
  import { fmt, t } from "../i18n/index.svelte";

  type Props = {
    state: UploadResult;
    /** `fixed` floats over the entire viewport; `absolute` anchors to
     * the nearest positioned ancestor (use inside Lightbox so the
     * toast stays above the backdrop). */
    position?: "fixed" | "absolute";
  };

  let { state, position = "fixed" }: Props = $props();
</script>

{#if state.kind === "done"}
  <div class="toast done" role="status" style:position>
    <Copy size={14} />
    <span>{t().toastCopiedToClipboard}</span>
    <span class="url" title={state.url}>{state.url}</span>
  </div>
{:else if state.kind === "error"}
  <div class="toast error" role="alert" style:position>
    {fmt(t().toastUploadFailed, { message: state.message })}
  </div>
{/if}

<style>
  .toast {
    bottom: 20px;
    left: 50%;
    transform: translateX(-50%);
    max-width: 70%;
    padding: 10px 14px;
    display: inline-flex;
    align-items: center;
    gap: 8px;
    background: rgb(0 0 0 / 0.88);
    color: #fff;
    border: 1px solid rgb(255 255 255 / 0.12);
    border-radius: var(--radius-md);
    font-size: var(--text-sm);
    backdrop-filter: blur(8px);
    z-index: 50;
    animation: toast-in 200ms ease-out;
  }
  .toast.error {
    border-color: var(--color-danger);
  }
  .url {
    font-family: var(--font-mono);
    color: rgb(255 255 255 / 0.85);
    user-select: text;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 380px;
  }
  @keyframes toast-in {
    from {
      opacity: 0;
      transform: translate(-50%, 8px);
    }
    to {
      opacity: 1;
      transform: translate(-50%, 0);
    }
  }
</style>
