<script lang="ts"></script>

<main class="root">
  <header class="brand">
    <h1>Clipo is running</h1>
    <p class="subtitle">Capture anything on your screen. Lives in the tray.</p>
  </header>

  <section class="shortcuts">
    <div class="row">
      <span class="combo">
        <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>S</kbd>
      </span>
      <span class="desc">Capture an area</span>
    </div>
    <div class="row">
      <span class="combo">
        <kbd>PrintScreen</kbd>
      </span>
      <span class="desc">Capture the full screen</span>
    </div>
    <div class="row">
      <span class="combo">
        <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>K</kbd>
      </span>
      <span class="desc">Open the capture menu</span>
    </div>
    <div class="row">
      <span class="combo">
        <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>A</kbd>
      </span>
      <span class="desc">Quick Access (recent captures)</span>
    </div>
  </section>

  <footer>
    Right-click the tray icon for History, Settings, and more.
  </footer>
</main>

<style>
  .root {
    height: 100vh;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    padding: 24px;
    gap: 20px;
    text-align: center;
  }

  .brand h1 {
    margin: 0;
    font-size: var(--text-xl);
    font-weight: 600;
  }
  .brand .subtitle {
    margin: 6px 0 0;
    font-size: var(--text-sm);
    color: var(--color-fg-muted);
  }

  .shortcuts {
    display: flex;
    flex-direction: column;
    gap: 8px;
    width: 100%;
    max-width: 340px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 12px;
    background: var(--color-surface-1);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius-md);
  }
  .combo {
    display: inline-flex;
    align-items: center;
    gap: 2px;
    min-width: 132px;
    justify-content: center;
  }
  kbd {
    display: inline-block;
    padding: 2px 6px;
    margin: 0 1px;
    border: 1px solid var(--color-border-subtle);
    background: var(--color-surface-2);
    border-radius: var(--radius-xs);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    color: var(--color-fg);
  }
  .desc {
    font-size: var(--text-sm);
    color: var(--color-fg-muted);
    text-align: left;
    flex: 1;
  }

  footer {
    font-size: var(--text-xs);
    color: var(--color-fg-subtle);
  }
</style>
