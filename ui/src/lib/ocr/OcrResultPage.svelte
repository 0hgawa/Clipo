<script lang="ts">
  /**
   * OCR result window.
   *
   * Pre-declared `ocr` window. The daemon shows it, emits `ocr:start`,
   * runs the recogniser on a worker, then emits `ocr:result`. We pull
   * the actual outcome via `take_ocr_result` — listen-only would race
   * the worker on cold starts (event lands before the JS handler is
   * attached). Pulling lets the same path serve both the warm and
   * cold case.
   *
   * Dismiss = ESC. The window stays alive between runs; never close().
   */
  import { invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { save } from "@tauri-apps/plugin-dialog";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import { Copy, Save, ScanText, X } from "@lucide/svelte";
  import { onDestroy, onMount } from "svelte";
  import Button from "../components/Button.svelte";
  import StatusMessage from "../components/StatusMessage.svelte";
  import { dismissOffscreen } from "../dismissOffscreen";
  import { initLocaleSync, t } from "../i18n/index.svelte";

  type OcrLine = { text: string };
  type OcrText = { fullText: string; lines: OcrLine[] };
  type PullResult = { Ok: OcrText } | { Err: string };

  const COPY_FEEDBACK_MS = 600;

  let payload = $state<OcrText | null>(null);
  let errorMessage = $state<string | null>(null);
  let recognizing = $state(false);
  let copied = $state(false);
  let copyTimer: number | undefined;
  let unlistenStart: UnlistenFn | undefined;
  let unlistenResult: UnlistenFn | undefined;
  let unlistenLocale: UnlistenFn | undefined;

  const win = getCurrentWindow();

  const lineCount = $derived(payload?.lines.length ?? 0);
  const wordCount = $derived(
    payload ? payload.fullText.split(/\s+/).filter((w) => w.length > 0).length : 0,
  );

  function reset() {
    payload = null;
    errorMessage = null;
    recognizing = true;
    copied = false;
    clearTimeout(copyTimer);
  }

  async function dismiss() {
    clearTimeout(copyTimer);
    await dismissOffscreen(win);
  }

  function onKey(event: KeyboardEvent) {
    if (event.key === "Escape") {
      event.preventDefault();
      void dismiss();
      return;
    }
    if (
      (event.ctrlKey || event.metaKey) &&
      event.key.toLowerCase() === "c" &&
      !hasUserSelection()
    ) {
      event.preventDefault();
      void doCopy();
    }
  }

  function hasUserSelection(): boolean {
    const sel = window.getSelection();
    return !!(sel && sel.toString().length > 0);
  }

  async function autoCopy() {
    if (!payload || payload.fullText.trim().length === 0) return;
    try {
      await invoke("copy_text_to_clipboard", { text: payload.fullText });
    } catch (e) {
      console.error("ocr auto-copy", e);
    }
  }

  async function doCopy() {
    if (!payload) return;
    try {
      await invoke("copy_text_to_clipboard", { text: payload.fullText });
      copied = true;
      clearTimeout(copyTimer);
      copyTimer = window.setTimeout(dismiss, COPY_FEEDBACK_MS);
    } catch (e) {
      console.error("copy_text_to_clipboard", e);
    }
  }

  async function doSave() {
    if (!payload) return;
    try {
      const dst = await save({
        defaultPath: t().ocrSaveDefaultName,
        filters: [{ name: t().ocrSaveFilterName, extensions: ["txt"] }],
      });
      if (!dst) return;
      await invoke("write_text_file", { path: dst, contents: payload.fullText });
      void dismiss();
    } catch (e) {
      console.error("write_text_file", e);
    }
  }

  async function pullOnce() {
    try {
      const cached = await invoke<PullResult | null>("take_ocr_result", {
        label: win.label,
      });
      if (!cached) return;
      if ("Ok" in cached) {
        payload = cached.Ok;
        recognizing = false;
        void autoCopy();
      } else {
        errorMessage = cached.Err || t().ocrUnknownError;
        recognizing = false;
      }
    } catch (e) {
      errorMessage = e instanceof Error ? e.message : String(e);
      recognizing = false;
    }
  }

  onMount(async () => {
    unlistenLocale = await initLocaleSync();
    unlistenStart = await listen("ocr:start", () => reset());
    unlistenResult = await listen("ocr:result", () => void pullOnce());
    // Cold start: if the daemon stashed a result before the listeners
    // attached, this catches it on the first mount.
    await pullOnce();
  });

  onDestroy(() => {
    clearTimeout(copyTimer);
    unlistenStart?.();
    unlistenResult?.();
    unlistenLocale?.();
  });
</script>

<svelte:window onkeydown={onKey} />

<div class="root">
  <header data-tauri-drag-region>
    <div class="title" data-tauri-drag-region>
      <ScanText size={14} />
      <span data-tauri-drag-region>{t().ocrTitle}</span>
    </div>
    <Button iconOnly ghost size="sm" onclick={dismiss} ariaLabel={t().commonClose} title={t().commonCloseEsc}>
      <X size={13} />
    </Button>
  </header>

  {#if recognizing}
    <StatusMessage loading title={t().ocrRecognizing} />
  {:else if errorMessage}
    <StatusMessage variant="error" title={t().ocrFailed} reason={errorMessage} />
  {:else if payload && payload.fullText.trim().length === 0}
    <StatusMessage
      title={t().ocrNoText}
      reason={t().ocrNoTextReason}
    />
  {:else if payload}
    <textarea
      class="output"
      readonly
      spellcheck="false"
      value={payload.fullText}
      aria-label={t().ocrExtractedTextAria}
    ></textarea>
    <footer>
      <span class="meta">
        {lineCount} {lineCount === 1 ? t().ocrLine : t().ocrLines}
        ·
        {wordCount} {wordCount === 1 ? t().ocrWord : t().ocrWords}
      </span>
      <span class="spacer"></span>
      <Button ghost size="sm" onclick={doSave} title={t().ocrSaveTitle}>
        <Save size={13} />
        <span>{t().commonSaveAs}</span>
      </Button>
      <Button size="sm" variant="primary" onclick={doCopy} title={t().ocrCopyTitle}>
        <Copy size={13} />
        <span>{copied ? t().commonCopied : t().commonCopy}</span>
      </Button>
    </footer>
  {/if}
</div>

<style>
  .root {
    display: flex;
    flex-direction: column;
    height: 100vh;
  }

  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 10px 14px;
  }

  .title {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-size: var(--text-md);
    font-weight: 600;
    color: var(--color-fg);
  }

  .output {
    flex: 1;
    width: 100%;
    box-sizing: border-box;
    margin: 0;
    padding: 16px 20px;
    border: none;
    background: var(--color-surface-input);
    color: var(--color-fg);
    font-family: var(--font-mono);
    font-size: var(--text-sm);
    /* Dense mono dumps read better at ~1.4 — `--leading-normal`
     * (1.5) makes log lines feel airy and inflates total height. */
    line-height: 1.4;
    resize: none;
    outline: none;
    user-select: text;
    white-space: pre-wrap;
  }
  .output:focus-visible {
    box-shadow: inset 0 0 0 1px var(--color-border-accent);
  }

  footer {
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 14px;
  }
  .meta {
    font-size: var(--text-xs);
    color: var(--color-fg-muted);
    font-variant-numeric: tabular-nums;
  }
  .spacer {
    flex: 1;
  }

</style>
