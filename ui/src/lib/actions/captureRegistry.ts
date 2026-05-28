/**
 * Single source of truth for capture actions.
 *
 * Every surface that lets the user act on a capture (history hover
 * overlay, lightbox toolbar, post-capture panel) iterates over a
 * subset of these IDs instead of repeating icon/label/invoke wiring.
 * Adding a new action — or renaming one — is a one-line edit here
 * that propagates everywhere automatically.
 */
import { invoke } from "@tauri-apps/api/core";
import { CloudUpload, Copy, ExternalLink, FileImage, FolderOpen, Pencil, ScanText, Trash2 } from "@lucide/svelte";
import type { Component } from "svelte";
import type { Dict } from "../i18n/index.svelte";

export type ActionId = "edit" | "ocr" | "copy" | "upload" | "reveal" | "delete" | "open" | "gif";

export type ActionDef = {
  icon: Component;
  /** i18n catalog key for the ARIA label + tooltip title. Consumers
   * resolve via `t()[def.labelKey]` at render so a language flip
   * re-labels every action in place. */
  labelKey: keyof Dict;
  /** Destructive actions get the `danger` Button variant and a
   * confirm dialog from the row component. */
  destructive?: boolean;
  /** Backend takes seconds (subprocess, network round-trip, decode).
   * The row component swaps the button icon for an inline spinner
   * while the call is in flight, and disables re-clicks. Fast
   * actions (copy / reveal / open / edit / ocr / delete) skip this
   * and just resolve their Promise quietly. */
  slow?: boolean;
  /** Tauri command invocation. Upload returns the public URL; the
   * others resolve to void. The row component knows how to handle
   * `upload`'s return value specifically (in-flight tracking +
   * propagating the URL to the parent's toast). */
  invoke: (path: string) => Promise<unknown>;
};

export const ACTION_REGISTRY: Record<ActionId, ActionDef> = {
  edit: {
    icon: Pencil,
    labelKey: "actionEdit",
    invoke: (path) => invoke("open_editor", { path }),
  },
  ocr: {
    icon: ScanText,
    labelKey: "actionOcr",
    invoke: (path) => invoke("ocr_extract", { path }),
  },
  copy: {
    icon: Copy,
    labelKey: "actionCopyImage",
    invoke: (path) => invoke("copy_capture_image", { path }),
  },
  upload: {
    icon: CloudUpload,
    labelKey: "actionUpload",
    slow: true,
    invoke: (path) => invoke<string>("upload_capture", { path }),
  },
  reveal: {
    icon: FolderOpen,
    labelKey: "actionReveal",
    invoke: (path) => invoke("reveal_in_folder", { path }),
  },
  open: {
    icon: ExternalLink,
    labelKey: "actionOpenFile",
    invoke: (path) => invoke("open_file", { path }),
  },
  delete: {
    icon: Trash2,
    labelKey: "actionDelete",
    destructive: true,
    invoke: (path) => invoke("delete_capture", { path }),
  },
  gif: {
    icon: FileImage,
    labelKey: "actionExportGif",
    slow: true,
    invoke: (path) => invoke<string>("export_to_gif", { path }),
  },
};

/** Shape used by both the per-window toast and the surfaces that
 * surrender feedback to it. Kept here so the registry stays the
 * single import for "everything about capture actions". */
export type UploadResult =
  | { kind: "idle" }
  | { kind: "done"; url: string }
  | { kind: "error"; message: string };
