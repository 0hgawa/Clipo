<script lang="ts">
  /**
   * Window controls (minimize / maximize-restore / close).
   *
   * Drops into any custom titlebar that has `data-tauri-drag-region`
   * on a parent — buttons here naturally opt out of the drag region
   * (Tauri 2 exempts interactive children by default).
   *
   * `win.close()` is intercepted by the daemon's global window-event
   * handler, which calls `hide()` instead of destroying the WebView2
   * surface (the ghost-rectangle bug class). So "close" here means
   * "hide" for every chrome window in the app — exactly what the user
   * expects from a tray-resident app.
   */
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { Maximize2, Minimize2, Minus, X } from "@lucide/svelte";
  import { onDestroy, onMount } from "svelte";

  /**
   * Optional override for the close button. Use it when the surface
   * needs to intercept (e.g. "Discard unsaved annotations?" prompt
   * in the editor). Default just hides the window via the daemon's
   * global close-requested handler.
   */
  let { onClose }: { onClose?: () => void | Promise<void> } = $props();

  let isMaximized = $state(false);
  let unlistenResize: (() => void) | undefined;

  const win = getCurrentWindow();

  async function refreshMaximized() {
    try {
      isMaximized = await win.isMaximized();
    } catch (e) {
      console.error("isMaximized", e);
    }
  }

  async function onMinimize() {
    try {
      await win.minimize();
    } catch (e) {
      console.error("minimize", e);
    }
  }

  async function onToggleMax() {
    try {
      await win.toggleMaximize();
      await refreshMaximized();
    } catch (e) {
      console.error("toggleMaximize", e);
    }
  }

  async function onCloseClick() {
    if (onClose) {
      try {
        await onClose();
      } catch (e) {
        console.error("custom close", e);
      }
      return;
    }
    try {
      await win.close();
    } catch (e) {
      console.error("close", e);
    }
  }

  onMount(async () => {
    await refreshMaximized();
    unlistenResize = await win.onResized(() => void refreshMaximized());
  });

  onDestroy(() => {
    unlistenResize?.();
  });
</script>

<div class="chrome">
  <button class="btn" onclick={onMinimize} aria-label="Minimize" title="Minimize">
    <Minus size={14} />
  </button>
  <button class="btn" onclick={onToggleMax} aria-label={isMaximized ? "Restore" : "Maximize"} title={isMaximized ? "Restore" : "Maximize"}>
    {#if isMaximized}
      <Minimize2 size={12} />
    {:else}
      <Maximize2 size={12} />
    {/if}
  </button>
  <button class="btn close" onclick={onCloseClick} aria-label="Close" title="Close">
    <X size={14} />
  </button>
</div>

<style>
  .chrome {
    display: inline-flex;
    align-items: stretch;
    flex-shrink: 0;
  }

  .btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 42px;
    height: 100%;
    min-height: 30px;
    border: none;
    background: transparent;
    color: var(--color-fg-muted);
    cursor: pointer;
    padding: 0;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
  }
  .btn:hover {
    background: var(--color-surface-2);
    color: var(--color-fg);
  }
  .btn.close:hover {
    background: var(--color-danger);
    color: #fff;
  }
</style>
