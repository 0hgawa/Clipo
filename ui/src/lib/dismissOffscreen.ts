/**
 * Shared dismiss helper for popup surfaces (timer / ocr / menu / quick
 * / tray-menu / window-picker).
 *
 * Park the window off-screen BEFORE hiding so WebView2 can't leave a
 * stale frame visible during the swap — defensive against the ghost-
 * surface bug class. The 30k offset is far past any legitimate virtual
 * desktop coordinate.
 */

import { LogicalPosition } from "@tauri-apps/api/dpi";
import type { Window } from "@tauri-apps/api/window";

const OFFSCREEN = new LogicalPosition(-30000, -30000);

export async function dismissOffscreen(win: Window): Promise<void> {
  try {
    await win.setPosition(OFFSCREEN);
    await win.hide();
  } catch (e) {
    console.error(`dismiss ${win.label}`, e);
  }
}
