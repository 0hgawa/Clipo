/**
 * Shared helpers for hotkey combos in the
 * `tauri-plugin-global-shortcut` accelerator format (e.g.
 * `CommandOrControl+Shift+KeyS`, `PrintScreen`, `Alt+F4`).
 *
 * Both the [KeyCaptureInput] component and the Settings page import
 * `formatCombo` for display, so it lives here instead of inside the
 * component (Svelte 5 only exports the component itself from a
 * `.svelte` file).
 */

/** Modifier-only `KeyboardEvent.code`s — these alone don't commit;
 * the capture waits for a real key on top. */
const MODIFIER_CODES = new Set([
  "ControlLeft", "ControlRight",
  "AltLeft", "AltRight",
  "ShiftLeft", "ShiftRight",
  "MetaLeft", "MetaRight",
  "OSLeft", "OSRight",
]);

/** Codes that are allowed to bind without any modifier. A bare letter
 * / digit binding would swallow every keystroke; function keys and
 * media keys are reserved enough to stand alone. */
function standaloneOk(code: string): boolean {
  return (
    /^F\d{1,2}$/.test(code) ||
    code === "PrintScreen" ||
    code === "ScrollLock" ||
    code === "Pause" ||
    code.startsWith("Audio") ||
    code.startsWith("Media")
  );
}

/** Convert a `KeyboardEvent` to a Tauri accelerator string, or
 * `null` when we should keep waiting (modifier-only press, or a bare
 * letter without modifiers). */
export function eventToCombo(e: KeyboardEvent): string | null {
  if (MODIFIER_CODES.has(e.code)) return null;
  const parts: string[] = [];
  if (e.ctrlKey || e.metaKey) parts.push("CommandOrControl");
  if (e.altKey) parts.push("Alt");
  if (e.shiftKey) parts.push("Shift");
  if (parts.length === 0 && !standaloneOk(e.code)) return null;
  parts.push(e.code);
  return parts.join("+");
}

/** Pretty-print an accelerator string for the user. Strips the
 * `Key` / `Digit` / `Arrow` prefixes that the Tauri format inherits
 * from `KeyboardEvent.code`, and shortens `CommandOrControl` →
 * `Ctrl`. Display only; the raw string stays the source of truth. */
export function formatCombo(combo: string): string {
  if (!combo) return "—";
  return combo
    .split("+")
    .map((part) => {
      if (part === "CommandOrControl") return "Ctrl";
      if (part === "Meta" || part === "Super") return "Win";
      if (part.startsWith("Key")) return part.slice(3);
      if (part.startsWith("Digit")) return part.slice(5);
      if (part.startsWith("Numpad")) return `Num${part.slice(6)}`;
      if (part.startsWith("Arrow")) return part.slice(5);
      return part;
    })
    .join("+");
}
