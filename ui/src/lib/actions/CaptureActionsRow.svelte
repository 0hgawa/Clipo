<script lang="ts">
  /**
   * Renders a row of capture-action buttons from a list of IDs.
   *
   * All three surfaces (history hover, lightbox toolbar, post-capture
   * panel) share this row — they just pass different `actions` and
   * style options. The registry knows the icon + label + invoker; the
   * row knows the in-flight tracking, the destructive confirm, and
   * how to surface upload feedback to the parent.
   *
   * Upload is special: it's the only action whose return value the
   * caller cares about. The row tracks per-path in-flight state with
   * `SvelteSet` (so multiple captures can upload in parallel each
   * with its own spinner) and forwards success / error through
   * `onUploadResult` — the parent owns toast placement and timing.
   */
  import Button from "../components/Button.svelte";
  import { SvelteSet } from "svelte/reactivity";
  import { Check, Link2 } from "@lucide/svelte";
  import { invoke } from "@tauri-apps/api/core";
  import { onDestroy } from "svelte";
  import { ACTION_REGISTRY, type ActionId, type UploadResult } from "./captureRegistry";
  import { fmt, t } from "../i18n/index.svelte";

  /** Actions whose backend has no visible side effect (no window
   * opens, no card disappears, no toast surfaces) — these get a
   * brief check-mark in place of the icon after success so the user
   * knows the click landed. Today: just Copy. */
  const SILENT_ACTIONS: ReadonlySet<ActionId> = new Set(["copy"]);
  const CONFIRM_MS = 1500;

  type Entry = {
    path: string;
    filename: string;
  };

  type Props = {
    entry: Entry;
    /** Ordered list of action IDs to render. */
    actions: readonly ActionId[];
    /** Lightbox-style buttons sit on top of imagery — `ghost` keeps
     * them quiet. Other surfaces use the default filled chip. */
    ghost?: boolean;
    size?: "sm" | "md";
    iconSize?: number;
    /** Cached upload URL for this entry, if any. When present the
     * Upload button changes to a Link icon and a click copies the
     * URL instead of re-uploading — avoids needless hits against
     * the host's rate limits. */
    cachedUrl?: string | undefined;
    /** Fires whenever the count of in-flight slow actions transitions
     * empty ↔ non-empty. Lets the parent pause anything that
     * shouldn't run mid-request (the post-capture panel's
     * 5 s auto-dismiss, for example). Single callback covers both
     * upload + GIF export — caller can't distinguish, but doesn't
     * need to: any in-flight slow action is reason enough to wait. */
    onBusyChange?: (busy: boolean) => void;
    /** Called with the resolved URL or error message after each
     * upload finishes. Lets the parent decide where + how long the
     * feedback lives, AND update its own URL cache (the row holds
     * its source of truth in the parent so multiple surfaces stay in
     * sync). */
    onUploadResult?: (result: UploadResult, path: string) => void;
    /** Fires after any non-upload action successfully resolves.
     * Surfaces like the post-capture panel use this to auto-dismiss
     * once the user has acted. Errors don't fire it. */
    onActionComplete?: (id: ActionId) => void;
    /** Confirmation message template for destructive actions. The
     * parent typically overrides to localise / customise wording. */
    confirmMessage?: (entry: Entry) => string;
  };

  let {
    entry,
    actions,
    ghost = false,
    size = "md",
    iconSize = 16,
    cachedUrl,
    onBusyChange,
    onUploadResult,
    onActionComplete,
    confirmMessage = (e) => fmt(t().actionConfirmDelete, { filename: e.filename }),
  }: Props = $props();

  /** Per-action-per-path in-flight tracking for actions flagged
   * `slow` in the registry (currently Upload + GIF export). Key is
   * `${actionId}\x00${path}` so two slow actions on the same entry
   * (e.g. uploading + converting to GIF at once) each get their own
   * spinner instead of cross-contaminating. SvelteSet because plain
   * Set isn't tracked deeply — `.add()` / `.delete()` need to
   * surface to the disabled / spinner bindings below. */
  const inflight = new SvelteSet<string>();
  const inflightKey = (id: ActionId, path: string) => `${id}\x00${path}`;

  /** Last silent-action id that just completed — the icon swaps to a
   * checkmark for `CONFIRM_MS`. Single slot is enough because the
   * row only ever runs one Copy at a time per entry. */
  let confirmedActionId = $state<ActionId | null>(null);
  let confirmTimer: ReturnType<typeof setTimeout> | undefined;

  onDestroy(() => clearTimeout(confirmTimer));

  // Broadcast busy transitions (empty → non-empty and back). The
  // local `lastBusy` mirror prevents emitting on intermediate
  // transitions like 1→2 or 2→1 where the parent's pause logic
  // doesn't care — only the boundary matters.
  let lastBusy = false;
  $effect(() => {
    const busy = inflight.size > 0;
    if (busy !== lastBusy) {
      onBusyChange?.(busy);
      lastBusy = busy;
    }
  });

  async function handleClick(id: ActionId) {
    const def = ACTION_REGISTRY[id];
    if (def.destructive && !window.confirm(confirmMessage(entry))) return;

    // Upload has its own flow: cached-link shortcut + URL result fed
    // back through `onUploadResult`. Otherwise it shares the slow-
    // action spinner machinery below.
    if (id === "upload") {
      // Cached path: copy the existing URL — no network round-trip,
      // no rate-limit pressure, instant feedback via the same
      // inline check that confirms a Copy.
      if (cachedUrl !== undefined) {
        try {
          await invoke("copy_text_to_clipboard", { text: cachedUrl });
          clearTimeout(confirmTimer);
          confirmedActionId = id;
          confirmTimer = setTimeout(() => {
            confirmedActionId = null;
          }, CONFIRM_MS);
        } catch (e) {
          console.error("copy_text_to_clipboard", e);
        }
        return;
      }
      const key = inflightKey(id, entry.path);
      if (inflight.has(key)) return;
      inflight.add(key);
      try {
        const url = (await def.invoke(entry.path)) as string;
        onUploadResult?.({ kind: "done", url }, entry.path);
      } catch (e) {
        console.error("upload_capture", e);
        onUploadResult?.(
          { kind: "error", message: e instanceof Error ? e.message : String(e) },
          entry.path,
        );
      } finally {
        inflight.delete(key);
      }
      return;
    }

    // Slow actions (currently just GIF export): swap the icon for a
    // spinner and disable re-clicks until the backend resolves. No
    // result fan-out — backend emits `capture:saved` for the side
    // effect, parent surfaces refresh on that.
    if (def.slow) {
      const key = inflightKey(id, entry.path);
      if (inflight.has(key)) return;
      inflight.add(key);
      try {
        await def.invoke(entry.path);
        onActionComplete?.(id);
      } catch (e) {
        console.error(id, e);
      } finally {
        inflight.delete(key);
      }
      return;
    }

    try {
      await def.invoke(entry.path);
      if (SILENT_ACTIONS.has(id)) {
        clearTimeout(confirmTimer);
        confirmedActionId = id;
        confirmTimer = setTimeout(() => {
          confirmedActionId = null;
        }, CONFIRM_MS);
      }
      onActionComplete?.(id);
    } catch (e) {
      console.error(id, e);
    }
  }
</script>

{#each actions as id (id)}
  {@const def = ACTION_REGISTRY[id]}
  {@const Icon = def.icon}
  {@const isInflight = inflight.has(inflightKey(id, entry.path))}
  {@const isCached = id === "upload" && cachedUrl !== undefined}
  {@const isConfirmed = confirmedActionId === id}
  {@const label = t()[def.labelKey]}
  <Button
    {ghost}
    {size}
    iconOnly
    variant={def.destructive ? "danger" : "default"}
    disabled={isInflight}
    onclick={() => handleClick(id)}
    ariaLabel={isCached ? t().actionCopyLink : label}
    title={isCached ? t().actionCopyLink : label}
  >
    {#if isInflight}
      <span class="spinner" aria-hidden="true" style:--spinner-size="{iconSize}px"></span>
    {:else if isConfirmed}
      <Check size={iconSize} />
    {:else if isCached}
      <Link2 size={iconSize} />
    {:else}
      <Icon size={iconSize} />
    {/if}
  </Button>
{/each}

<style>
  /* Sized from the inline custom property so the spinner matches the
   * icon it temporarily replaces — same row, same caller, no extra
   * variants to maintain. */
  .spinner {
    width: var(--spinner-size, 16px);
    height: var(--spinner-size, 16px);
    border: 2px solid rgb(255 255 255 / 0.25);
    border-top-color: currentColor;
    border-radius: 50%;
    animation: spin 700ms linear infinite;
  }
  @keyframes spin {
    to {
      transform: rotate(360deg);
    }
  }
</style>
