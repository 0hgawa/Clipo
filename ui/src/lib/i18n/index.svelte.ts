/**
 * i18n — plain-dict pattern, no library. The `Dict` type derived from
 * `en` enforces key parity at compile time: a translator who skips a
 * key gets a type error.
 *
 * Per-webview state: each surface imports its own module instance, so
 * `locale` $state is per-surface. Cross-surface sync via the
 * `settings:language-changed` Tauri event from the daemon.
 *
 * All 12 catalogs ship in every bundle (~7 KB each). Dynamic import
 * per locale would shave that but adds an async boot step.
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import en from "./en";
import pt from "./pt";
import es from "./es";
import fr from "./fr";
import de from "./de";
import it from "./it";
import ja from "./ja";
import ko from "./ko";
import zh from "./zh";
import ru from "./ru";
import hi from "./hi";
import ar from "./ar";

export type Dict = Record<keyof typeof en, string>;

const dicts: Record<string, Dict> = {
  en, pt, es, fr, de, it, ja, ko, zh, ru, hi, ar,
};

/** Locale display names — written in the locale itself
 * ("Français", not "French"). */
export const LOCALE_LABELS: Record<string, string> = {
  en: "English",
  pt: "Português",
  es: "Español",
  fr: "Français",
  de: "Deutsch",
  it: "Italiano",
  ja: "日本語",
  ko: "한국어",
  zh: "中文",
  ru: "Русский",
  hi: "हिन्दी",
  ar: "العربية",
};

export const SUPPORTED_LOCALES = Object.keys(dicts);

let locale = $state("en");
// `en` is canonical so always present; non-null assertion silences
// noUncheckedIndexedAccess.
let dict = $derived<Dict>(dicts[locale] ?? dicts.en!);

/** Call in templates as `{t().key}` — Svelte tracks the read through
 * the function so the component re-renders on locale change. */
export function t(): Dict {
  return dict;
}

export function setLocale(code: string) {
  if (dicts[code]) locale = code;
}

export function getLocale(): string {
  return locale;
}

/** Substitute `{name}` placeholders. Unknown placeholders pass through
 * unchanged so missed substitutions surface as a visible bug, not a
 * silent empty string. */
export function fmt(template: string, vars: Record<string, string | number>): string {
  return template.replace(/\{(\w+)\}/g, (_, k) => {
    const v = vars[k];
    return v === undefined ? `{${k}}` : String(v);
  });
}

/** Boot fetch + live listener. Call in onMount, store the unlisten
 * in onDestroy. */
export async function initLocaleSync(): Promise<UnlistenFn> {
  try {
    const s = await invoke<{ language?: string }>("get_settings");
    if (s.language) setLocale(s.language);
  } catch (e) {
    // Silent: surface stays on English; listener below catches the
    // next change.
    console.warn("i18n init: get_settings failed", e);
  }
  return listen<string>("settings:language-changed", (e) => {
    setLocale(e.payload);
  });
}
