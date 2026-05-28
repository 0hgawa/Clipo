import type { Action } from "svelte/action";

/**
 * Svelte action: dismiss a popover panel on pointerdown outside both
 * the panel and its trigger. Consolidates the click-outside dance the
 * editor pickers (Color / Font / Size / Stroke) were each
 * re-implementing — register/deregister + the bounding-box check
 * lived inline in each picker (~18 lines × 4).
 *
 * Usage: place the action on the panel itself, inside an `{#if open}`
 * block so mount/destroy lines up with the popover's visible state.
 * That way the global `pointerdown` listener exists for the panel's
 * lifetime only — no need for an `active` flag.
 *
 *   {#if open}
 *     <div bind:this={panel}
 *          use:outsideDismiss={{ trigger, onDismiss: () => open = false }}>
 *       …
 *     </div>
 *   {/if}
 *
 * `capture: true` so the dismiss fires before any child handlers
 * (matches the original FontPicker / ColorPicker behaviour).
 */
export type OutsideDismissOptions = {
  /** The element that toggles `open`; clicks on it are ignored so the
   * toggle handler can flip `open` itself (one source of truth for
   * the state machine). */
  trigger?: HTMLElement;
  onDismiss: () => void;
};

export const outsideDismiss: Action<HTMLElement, OutsideDismissOptions> = (
  node,
  initial,
) => {
  let opts = initial;
  function onPointerDown(ev: PointerEvent) {
    const target = ev.target as Node;
    if (node.contains(target) || opts.trigger?.contains(target)) return;
    opts.onDismiss();
  }
  document.addEventListener("pointerdown", onPointerDown, true);
  return {
    update(next: OutsideDismissOptions) {
      opts = next;
    },
    destroy() {
      document.removeEventListener("pointerdown", onPointerDown, true);
    },
  };
};
