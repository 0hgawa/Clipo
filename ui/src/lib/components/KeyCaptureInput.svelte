<script lang="ts">
  /**
   * Capture a hotkey combo from the user and emit it in the
   * `tauri-plugin-global-shortcut` accelerator format (e.g.
   * `CommandOrControl+Shift+KeyS`, `PrintScreen`, `Alt+F4`).
   *
   * Click → input enters "recording" state, the next valid keydown
   * commits the combo. `Esc` cancels (keeps the previous value). The
   * window-level listener is only attached while recording so we
   * don't intercept hotkeys for the rest of the app.
   */
  import { Eraser } from "@lucide/svelte";
  import { eventToCombo, formatCombo } from "./keyCombo";
  import { t } from "../i18n/index.svelte";

  type Props = {
    /** Tauri accelerator string (e.g. `CommandOrControl+Shift+KeyS`). */
    value: string;
    /** Disabled while the recording-aware app is busy or the daemon
     * is mid-recording (where any rebind would race). */
    disabled?: boolean;
    onChange: (next: string) => void;
  };

  let { value, disabled = false, onChange }: Props = $props();

  let recording = $state(false);

  function start() {
    if (disabled) return;
    recording = true;
  }

  function cancel() {
    recording = false;
  }

  function onKey(e: KeyboardEvent) {
    if (!recording) return;
    e.preventDefault();
    e.stopPropagation();
    if (e.key === "Escape") {
      cancel();
      return;
    }
    const combo = eventToCombo(e);
    if (combo === null) return; // waiting for a non-modifier
    onChange(combo);
    recording = false;
  }
</script>

<svelte:window onkeydown={onKey} />

<button
  type="button"
  class="capture"
  class:recording
  class:disabled
  {disabled}
  onclick={start}
  onblur={cancel}
  aria-label={t().keyCaptureRebindAria}
>
  {#if recording}
    <span class="hint">{t().keyCaptureRecordingHint}</span>
  {:else}
    <span class="combo">{formatCombo(value)}</span>
  {/if}
  {#if !recording && value}
    <span class="trailing" aria-hidden="true"><Eraser size={12} /></span>
  {/if}
</button>

<style>
  .capture {
    display: inline-flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    min-width: 160px;
    height: 30px;
    padding: 0 10px;
    background: var(--color-surface-input);
    color: var(--color-fg);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius-sm);
    font-family: var(--font-mono);
    font-size: var(--text-sm);
    cursor: pointer;
    transition:
      border-color var(--duration-quick) var(--ease-in-out-soft),
      background var(--duration-quick) var(--ease-in-out-soft);
  }
  .capture:hover:not(.disabled):not(.recording) {
    border-color: var(--color-border-strong);
  }
  .capture.recording {
    border-color: var(--color-border-accent-strong);
    background: var(--color-accent-bg-subtle);
    color: var(--color-accent-fg);
    cursor: default;
  }
  .capture.disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .combo {
    flex: 1 1 auto;
    text-align: left;
  }
  .hint {
    flex: 1 1 auto;
    font-family: var(--font-sans);
    font-size: var(--text-xs);
    color: var(--color-accent-fg);
  }
  .trailing {
    color: var(--color-fg-subtle);
    display: inline-flex;
  }
  .capture:hover .trailing { color: var(--color-fg-muted); }
</style>
