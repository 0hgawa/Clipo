/**
 * Theme — frontend-only visual preference (System / Light / Dark).
 *
 * Lives in localStorage so it can be applied before first paint by the
 * inline script in index.html (without that the window flashes the
 * wrong colours). The inline script mirrors `resolve` below and must
 * stay a self-contained copy since it runs before the bundle loads.
 *
 * Cross-window sync: the `storage` event fires in OTHER same-origin
 * documents when another window writes the key. `matchMedia` keeps
 * "System" tracking the OS.
 */

export type ThemePref = "system" | "light" | "dark";

const STORAGE_KEY = "clipo-theme";

const prefersDark = () => window.matchMedia("(prefers-color-scheme: dark)").matches;

function readPref(): ThemePref {
  const v = localStorage.getItem(STORAGE_KEY);
  return v === "light" || v === "dark" || v === "system" ? v : "system";
}

function resolve(pref: ThemePref): "light" | "dark" {
  return pref === "system" ? (prefersDark() ? "dark" : "light") : pref;
}

function apply(p: ThemePref) {
  document.documentElement.dataset.theme = resolve(p);
}

let pref = $state<ThemePref>(readPref());

/** Read as `getThemePref()` so Svelte tracks the read and the picker
 * re-renders when the preference flips. */
export function getThemePref(): ThemePref {
  return pref;
}

export function setThemePref(next: ThemePref) {
  pref = next;
  localStorage.setItem(STORAGE_KEY, next);
  apply(next);
}

/** Wire live updates for a surface. Returns a teardown. */
export function initThemeSync(): () => void {
  const onStorage = (e: StorageEvent) => {
    if (e.key !== STORAGE_KEY) return;
    pref = readPref();
    apply(pref);
  };
  const mq = window.matchMedia("(prefers-color-scheme: dark)");
  const onOs = () => {
    if (pref === "system") apply(pref);
  };
  window.addEventListener("storage", onStorage);
  mq.addEventListener("change", onOs);
  return () => {
    window.removeEventListener("storage", onStorage);
    mq.removeEventListener("change", onOs);
  };
}
