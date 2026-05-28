<script lang="ts">
  import StatusPage from "./lib/StatusPage.svelte";
  import PostCaptureActions from "./lib/actions/PostCaptureActions.svelte";
  import EditorPage from "./lib/editor/EditorPage.svelte";
  import HistoryPage from "./lib/history/HistoryPage.svelte";
  import MenuPage from "./lib/menu/MenuPage.svelte";
  import OcrResultPage from "./lib/ocr/OcrResultPage.svelte";
  import QuickAccessPage from "./lib/quick/QuickAccessPage.svelte";
  import RecordingBarPage from "./lib/recording/RecordingBarPage.svelte";
  import SettingsPage from "./lib/settings/SettingsPage.svelte";
  import TimerPage from "./lib/timer/TimerPage.svelte";
  import TrayMenuPage from "./lib/tray/TrayMenuPage.svelte";
  import WindowPickerPage from "./lib/window-picker/WindowPickerPage.svelte";
  import { onDestroy, onMount } from "svelte";
  import { initThemeSync } from "./lib/theme.svelte";

  /**
   * Each Tauri window points at `index.html` with a `?surface=` query
   * so the same bundle can render different chromes. New surfaces
   * land here as their backend wiring goes live.
   */
  const surface = new URLSearchParams(window.location.search).get("surface");

  // Theme is applied pre-paint by the inline script in index.html; this
  // keeps the surface live — syncing when another window changes the
  // preference and when the OS flips while on "System".
  let teardownTheme: (() => void) | undefined;
  onMount(() => {
    teardownTheme = initThemeSync();
  });
  onDestroy(() => teardownTheme?.());
</script>

{#if surface === "actions"}
  <PostCaptureActions />
{:else if surface === "history"}
  <HistoryPage />
{:else if surface === "settings"}
  <SettingsPage />
{:else if surface === "timer"}
  <TimerPage />
{:else if surface === "quick"}
  <QuickAccessPage />
{:else if surface === "menu"}
  <MenuPage />
{:else if surface === "ocr"}
  <OcrResultPage />
{:else if surface === "editor"}
  <EditorPage />
{:else if surface === "tray-menu"}
  <TrayMenuPage />
{:else if surface === "recording-bar"}
  <RecordingBarPage />
{:else if surface === "window-picker"}
  <WindowPickerPage />
{:else if surface === null || surface === ""}
  <StatusPage />
{:else}
  <p class="placeholder">Surface "{surface}" is not implemented yet.</p>
{/if}

<style>
  .placeholder {
    margin: 0;
    padding: 24px;
    color: var(--color-fg-muted);
    height: 100vh;
    box-sizing: border-box;
  }
</style>
