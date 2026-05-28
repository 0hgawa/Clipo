<script lang="ts">
  /**
   * Annotation editor.
   *
   * Pre-declared `editor` window. Loads a saved capture by raw bytes
   * (asset:// would taint the canvas and break toBlob), lets the user
   * draw arrows / rects / freehand / highlights, then either Saves
   * (overwrites the source PNG) or Copies (re-publishes the annotated
   * image to the clipboard, no disk touch).
   *
   * Coordinates: the canvas backing store is at source-image resolution,
   * CSS scales it down to fit the viewport. Pointer events get mapped
   * back to source coords via `clientToCanvas`, so a stroke drawn near
   * a pixel at display time saves at the same pixel on the PNG.
   *
   * Dismiss = ESC (with confirm when dirty). Undo = Ctrl+Z. No redo
   * in v1: a third of the bug surface, a tenth of the value.
   */
  import { convertFileSrc, invoke } from "@tauri-apps/api/core";
  import { listen, type UnlistenFn } from "@tauri-apps/api/event";
  import { getCurrentWindow } from "@tauri-apps/api/window";
  import {
    ArrowUpRight,
    Check,
    Copy,
    Crop,
    EyeOff,
    Hand,
    Hash,
    Highlighter,
    Maximize,
    Minus,
    MousePointer2,
    PaintBucket,
    Pen,
    Plus,
    Redo2,
    Save,
    Square,
    Type,
    Undo2,
    X,
  } from "@lucide/svelte";
  import { onDestroy, onMount } from "svelte";
  import WindowChrome from "../chrome/WindowChrome.svelte";
  import Button from "../components/Button.svelte";
  import ColorPicker from "./ColorPicker.svelte";
  import FontPicker from "../components/FontPicker.svelte";
  import SegmentedControl from "../components/SegmentedControl.svelte";
  import SizePicker from "../components/SizePicker.svelte";
  import Slider from "../components/Slider.svelte";
  import StrokePicker from "./StrokePicker.svelte";
  import { outsideDismiss } from "../components/outsideDismiss";
  import Toggle from "../components/Toggle.svelte";

  type Tool =
    | "select"
    | "hand"
    | "arrow"
    | "rect"
    | "pen"
    | "highlight"
    | "blur"
    | "crop"
    | "text"
    | "number";
  type Rect = { x: number; y: number; w: number; h: number };
  type HandleId = "nw" | "n" | "ne" | "e" | "se" | "s" | "sw" | "w";
  type Point = { x: number; y: number };
  /** Blur is a redaction rect: gaussian blur of the underlying frame
   * via `ctx.filter`. WebView2 is Chromium, so `filter` is reliable
   * here. `strength` is the blur radius in image pixels. */
  type Annotation =
    | { kind: "arrow"; x1: number; y1: number; x2: number; y2: number; color: string; width: number }
    | { kind: "rect"; x: number; y: number; w: number; h: number; color: string; width: number }
    | { kind: "pen"; points: Point[]; color: string; width: number }
    | { kind: "highlight"; points: Point[]; color: string; width: number }
    | { kind: "blur"; x: number; y: number; w: number; h: number; strength: number }
    | { kind: "text"; x: number; y: number; text: string; color: string; fontSize: number; fontFamily: string }
    | { kind: "number"; cx: number; cy: number; n: number; color: string; radius: number };

  /** Font families exposed in the text-tool dropdown. CSS shorthand
   * (not just family name) so the rendered canvas and editing
   * textarea use the same fallback chain — no visual jump on commit. */
  const TEXT_FONTS = [
    { label: "Sans", value: '"Segoe UI Variable Text", "Segoe UI", system-ui, sans-serif' },
    { label: "Calibri", value: 'Calibri, "Segoe UI", sans-serif' },
    { label: "Serif", value: 'Cambria, Georgia, "Times New Roman", serif' },
    { label: "Mono", value: '"Cascadia Mono", Consolas, "Courier New", monospace' },
    { label: "Display", value: 'Impact, "Arial Narrow Bold", sans-serif' },
  ] as const;
  const TEXT_SIZES = [12, 16, 20, 24, 32, 48, 64, 96, 128] as const;
  /** Badge diameters for the Number tool. Stored as the pixel
   * diameter the user picks; we halve it internally to get the
   * canvas-side radius. Five values spaced for the actual range
   * a screenshot annotator uses — below 20 the digit doesn't read,
   * above 48 it overpowers any UI screenshot. */
  const NUMBER_SIZES = [20, 24, 32, 40, 48, 56] as const;

  /** Background presets — a non-destructive frame painted around the
   * source image at save time. CleanShot X's "Background" feature
   * minus the gimmicks: solids, gradients, and a self-blur. `none`
   * disables the entire pipeline (zero overhead in `redraw`). */
  type BgPreset =
    | { id: string; kind: "none" }
    | { id: string; kind: "solid"; color: string }
    | { id: string; kind: "gradient"; from: string; to: string; angle: number }
    | { id: string; kind: "blur" };
  /* Tile order is curated for visual reading: the two solids (slate /
   * snow) act as neutral anchors next to "none", then four gradients
   * span the hue wheel — cool blues, fresh greens, warm pinks, deep
   * violets, fire reds — so every quadrant of the colour space is
   * covered and no two tiles read as variations of the same hue. */
  const BG_PRESETS: readonly BgPreset[] = [
    { id: "none", kind: "none" },
    { id: "slate", kind: "solid", color: "#0f172a" },
    { id: "snow", kind: "solid", color: "#f8fafc" },
    { id: "ocean", kind: "gradient", from: "#38bdf8", to: "#1d4ed8", angle: 135 },
    { id: "mint", kind: "gradient", from: "#34d399", to: "#06b6d4", angle: 135 },
    { id: "sunset", kind: "gradient", from: "#fb923c", to: "#ec4899", angle: 135 },
    { id: "aurora", kind: "gradient", from: "#a855f7", to: "#2563eb", angle: 135 },
    { id: "ember", kind: "gradient", from: "#f43f5e", to: "#7c2d12", angle: 135 },
    { id: "blur", kind: "blur" },
  ] as const;
  /** Sentinel id resolved against `bgCustomColor` — the swatch lives
   * outside `BG_PRESETS` so the curated grid stays a `const`. */
  const BG_CUSTOM_ID = "custom";
  const BG_PADDINGS = [0, 16, 32, 64, 96] as const;
  const BG_RADII = [0, 8, 16, 24, 32] as const;
  /** Output aspect ratio. `free` keeps the natural padded shape;
   * the others grow the canvas on the shorter axis to hit the
   * target ratio while centering the image. Lets users export
   * "for IG / Twitter" without external resizing. */
  type BgAspectId = "free" | "1x1" | "16x9" | "9x16" | "4x3";
  const BG_ASPECTS: readonly { value: BgAspectId; label: string }[] = [
    { value: "free", label: "Free" },
    { value: "1x1", label: "1:1" },
    { value: "16x9", label: "16:9" },
    { value: "9x16", label: "9:16" },
    { value: "4x3", label: "4:3" },
  ];
  const BG_DEFAULT_PRESET = "none";
  /** Defaults MUST be values present in `BG_PADDINGS` / `BG_RADII` —
   * otherwise the segmented control's "active chip" lookup
   * (`value === opt.value`) silently misses on first open. */
  const BG_DEFAULT_PADDING = 32;
  const BG_DEFAULT_RADIUS = 16;
  const BG_DEFAULT_SHADOW = true;
  /* 60 % strength maps to the previous hard-coded `rgba(0,0,0,0.42)`
   * shadow opacity via `0.7 * strength/100` — so the default look
   * after introducing the slider is bit-identical to before. */
  const BG_DEFAULT_SHADOW_STRENGTH = 60;
  const BG_SHADOW_MAX_OPACITY = 0.7;
  const BG_DEFAULT_ASPECT: BgAspectId = "free";
  const BG_DEFAULT_CUSTOM_COLOR = "#6366f1";

  const WIDTHS = [2, 4, 6, 10, 16] as const;
  /** Default = first preset of the shared `ColorPicker` (red). */
  const DEFAULT_COLOR = "#ff1744";
  /** Default = middle of the WIDTHS scale (6 px), the "general
   * purpose" stroke that works for most arrows / rectangles. */
  const DEFAULT_WIDTH: number = WIDTHS[2];
  const COPIED_RESET_MS = 800;

  let tool = $state<Tool>("arrow");
  let color = $state<string>(DEFAULT_COLOR);
  let width = $state<number>(DEFAULT_WIDTH);
  let annotations = $state<Annotation[]>([]);
  let drafting = $state<Annotation | null>(null);
  let sourcePath = $state<string | null>(null);
  let dirty = $state(false);
  let copied = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);

  let canvas: HTMLCanvasElement;
  let stage: HTMLDivElement;
  /** The source image, normalised into a canvas. Using a canvas (not
   * `HTMLImageElement`) as the canonical type lets crop / resize
   * swap the source instantly without re-encoding through a PNG and
   * a new `Image()` load. `drawImage` accepts both shapes, so call
   * sites are unchanged. */
  let image = $state<HTMLCanvasElement | null>(null);
  let unlistenOpen: UnlistenFn | undefined;
  let copyTimer: number | undefined;

  /** Per-checkpoint snapshot for undo. Created via `pushHistory()`
   * BEFORE any destructive change (new annotation, crop, resize).
   * `image` is a clone — undoing must not aliased-mutate the live
   * source. Cap at HISTORY_LIMIT so a long session doesn't blow up
   * memory with 50 cloned canvases. */
  type Snapshot = {
    image: HTMLCanvasElement;
    annotations: Annotation[];
    bgPreset: string;
    bgPadding: number;
    bgRadius: number;
    bgShadow: boolean;
    bgShadowStrength: number;
    bgAspect: BgAspectId;
    bgCustomColor: string;
  };
  const HISTORY_LIMIT = 30;
  let history = $state<Snapshot[]>([]);
  /** Pop-stack of states that were just undone. Cleared the moment
   * the user makes a new edit — Ctrl+Z then drawing should not
   * resurrect the redone state with Ctrl+Y. */
  let redoHistory = $state<Snapshot[]>([]);

  // Crop tool state — `null` when not cropping. Coordinates are
  // always in image-pixel space (1:1 with the canvas backing store),
  // so zoom/pan don't enter the math here.
  let cropRect = $state<Rect | null>(null);
  let activeHandle: HandleId | null = null;
  let cropDragStart: { mx: number; my: number; rect: Rect } | null = null;

  // Resize popover state.
  let resizeOpen = $state(false);
  let resizeW = $state(0);
  let resizeH = $state(0);
  let resizeLockAspect = $state(true);

  /** In-progress text being typed. The textarea overlay binds to this
   * shape; `commitText()` materialises it into an annotation, then
   * `redraw()` bakes it into the canvas via `ctx.fillText`. */
  let textDraft = $state<
    | { x: number; y: number; text: string; color: string; fontSize: number; fontFamily: string }
    | null
  >(null);
  let textInput = $state<HTMLTextAreaElement | undefined>(undefined);
  /** When non-null, `textDraft` is editing the annotation at this
   * index instead of creating a new one. The renderer skips that
   * annotation (the textarea covers it visually), and `commitText`
   * replaces it in place — or removes it if the user empties the
   * field. */
  let editingIndex = $state<number | null>(null);
  /** Sticky font choices for the text tool. Persist between text
   * inserts so "type label, click elsewhere, type another label"
   * keeps the same look without re-picking. */
  let textFontFamily = $state<string>(TEXT_FONTS[0].value);
  /** Default size must match one of `TEXT_SIZES` so the `<select
   * bind:value>` finds a matching `<option>` — otherwise it renders
   * blank. 24px is the standard "subheading" weight. */
  let textFontSize = $state<number>(24);
  /** Diameter (px) for new Number-tool badges. Must be in
   * `NUMBER_SIZES` for the picker to highlight the current value. */
  let numberSize = $state<number>(32);

  /** Background state. `bgPreset === "none"` is the default and skips
   * the entire compositing path in `redraw`, so the feature has zero
   * cost when unused. */
  let bgPreset = $state<string>(BG_DEFAULT_PRESET);
  let bgPadding = $state<number>(BG_DEFAULT_PADDING);
  let bgRadius = $state<number>(BG_DEFAULT_RADIUS);
  let bgShadow = $state<boolean>(BG_DEFAULT_SHADOW);
  let bgShadowStrength = $state<number>(BG_DEFAULT_SHADOW_STRENGTH);
  /* Flag is `true` between the first `oninput` and the matching
   * `onchange` of a slider drag. Lets us snapshot history once at
   * the start of a drag (correct undo target) and then update the
   * live value per-frame without flooding the stack. Mirrors the
   * "snapshot once on entry, then update in place" pattern that
   * `setBgCustomColor` already uses for the OS colour picker. */
  let bgShadowDragging = false;
  let bgAspect = $state<BgAspectId>(BG_DEFAULT_ASPECT);
  let bgCustomColor = $state<string>(BG_DEFAULT_CUSTOM_COLOR);
  let bgPanelOpen = $state(false);
  let bgTrigger = $state<HTMLButtonElement | undefined>(undefined);
  /** Cached downscaled+blurred source. Keyed by image identity (a
   * fresh canvas from crop/resize/undo invalidates) so the heavy
   * `ctx.filter` work runs once per source, not once per frame. */
  let bgBlurCache: HTMLCanvasElement | null = null;
  let bgBlurCacheSource: HTMLCanvasElement | null = null;

  /** Currently selected annotation (index into `annotations`). Only
   * meaningful when `tool === "select"`. Cleared on tool switch and
   * on click in empty space. */
  let selected = $state<number | null>(null);
  /** Drag-to-move bookkeeping: mouse coords when the drag started
   * and a clone of the annotation before any movement, so we can
   * compute the delta without floating-point drift. */
  let moveStart: { mx: number; my: number; original: Annotation } | null = null;

  // Zoom / pan — purely CSS-transform driven so the canvas's intrinsic
  // (image-resolution) coordinate space stays untouched. Drawing math
  // never needs to know the zoom level; `getBoundingClientRect`
  // already reports the post-transform rect, so `clientToCanvas` keeps
  // working as-is.
  const ZOOM_MIN = 0.1;
  const ZOOM_MAX = 8;
  const ZOOM_STEP = 1.15;
  let zoom = $state(1);
  let panX = $state(0);
  let panY = $state(0);
  let panning = $state(false);
  let spaceHeld = $state(false);
  let lastPanPoint: { x: number; y: number } | null = null;

  const win = getCurrentWindow();

  async function loadImage(path: string) {
    busy = true;
    error = null;
    try {
      // crossOrigin = "anonymous" so `canvas.toBlob()` later doesn't
      // throw SecurityError — Tauri's asset protocol responds with
      // Access-Control-Allow-Origin: *, so the request is CORS-clean
      // and the canvas stays untainted.
      const img = new Image();
      img.crossOrigin = "anonymous";
      img.src = convertFileSrc(path);
      // `decode()` resolves after the image is parsed AND ready to
      // paint — browsers run it off the main thread, so 4K decodes
      // don't block UI like `onload` + first paint would. Throws on
      // any load/decode failure (replaces onerror).
      await img.decode();
      // Materialise into a canvas (the canonical source) so crop /
      // resize can swap without going through a PNG round-trip.
      const src = document.createElement("canvas");
      src.width = img.naturalWidth;
      src.height = img.naturalHeight;
      src.getContext("2d")?.drawImage(img, 0, 0);

      image = src;
      sourcePath = path;
      canvas.width = src.width;
      canvas.height = src.height;
      annotations = [];
      drafting = null;
      history = [];
      redoHistory = [];
      bgPreset = BG_DEFAULT_PRESET;
      bgPadding = BG_DEFAULT_PADDING;
      bgRadius = BG_DEFAULT_RADIUS;
      bgShadow = BG_DEFAULT_SHADOW;
      bgShadowStrength = BG_DEFAULT_SHADOW_STRENGTH;
      bgAspect = BG_DEFAULT_ASPECT;
      bgCustomColor = BG_DEFAULT_CUSTOM_COLOR;
      bgBlurCache = null;
      bgBlurCacheSource = null;
      dirty = false;
      fit();
      scheduleRedraw();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  /** Deep-copy the current image into a fresh canvas so a later undo
   * can restore pixels even after crop/resize discard them. */
  function cloneCanvas(src: HTMLCanvasElement): HTMLCanvasElement {
    const dst = document.createElement("canvas");
    dst.width = src.width;
    dst.height = src.height;
    dst.getContext("2d")?.drawImage(src, 0, 0);
    return dst;
  }

  /** Take a snapshot BEFORE mutating state. Caller doesn't need to
   * worry about ordering — push first, then change. Also clears the
   * redo stack: a new edit branch makes the old "redo future"
   * obsolete (standard text-editor undo/redo semantics). */
  function pushHistory() {
    if (!image) return;
    history.push({
      image: cloneCanvas(image),
      annotations: [...annotations],
      bgPreset,
      bgPadding,
      bgRadius,
      bgShadow,
      bgShadowStrength,
      bgAspect,
      bgCustomColor,
    });
    if (history.length > HISTORY_LIMIT) {
      // Drop the oldest so memory doesn't grow unbounded.
      history.shift();
    }
    redoHistory = [];
    dirty = true;
  }

  /** Entering crop mode seeds the rect with the full image so the
   * user can immediately drag the handles. Leaving crop mode (via
   * any tool switch) discards the unconfirmed rect — same as ESC.
   * Either edge also re-fits + redraws so the background frame
   * (suppressed during crop) appears/disappears in sync. */
  $effect(() => {
    if (tool === "crop" && image && !cropRect) {
      cropRect = { x: 0, y: 0, w: image.width, h: image.height };
      if (bgPreset !== "none") {
        fit();
        scheduleRedraw();
      }
    } else if (tool !== "crop" && cropRect) {
      cropRect = null;
      if (bgPreset !== "none") {
        fit();
        scheduleRedraw();
      }
    }
  });

  function clampRectToImage(r: Rect): Rect {
    if (!image) return r;
    const x = Math.max(0, Math.min(image.width, r.x));
    const y = Math.max(0, Math.min(image.height, r.y));
    const maxW = image.width - x;
    const maxH = image.height - y;
    const w = Math.max(1, Math.min(maxW, r.w));
    const h = Math.max(1, Math.min(maxH, r.h));
    return { x, y, w, h };
  }

  /** Map a client (mouse) coord into image-pixel space — undoes the
   * pan + zoom transform. */
  function clientToImage(cx: number, cy: number): { x: number; y: number } {
    const rect = stage.getBoundingClientRect();
    return {
      x: (cx - rect.left - panX) / zoom,
      y: (cy - rect.top - panY) / zoom,
    };
  }

  function onHandleDown(ev: PointerEvent, id: HandleId) {
    if (!cropRect) return;
    ev.stopPropagation();
    ev.preventDefault();
    activeHandle = id;
    (ev.currentTarget as Element).setPointerCapture?.(ev.pointerId);
  }

  function onCropRectDown(ev: PointerEvent) {
    if (!cropRect) return;
    if (ev.button !== 0) return;
    // Only count drags that land on the rect itself, not on a handle.
    const target = ev.target as HTMLElement;
    if (target.classList.contains("crop-handle")) return;
    ev.stopPropagation();
    ev.preventDefault();
    cropDragStart = {
      mx: ev.clientX,
      my: ev.clientY,
      rect: { ...cropRect },
    };
    (ev.currentTarget as Element).setPointerCapture?.(ev.pointerId);
  }

  function onCropPointerMove(ev: PointerEvent) {
    if (!cropRect || !image) return;
    // Defensive: only act if a primary button is actually held.
    // Mouse-leave-then-re-enter scenarios can leave the capture
    // alive without an active drag — without this guard the handle
    // would "follow" the cursor uncommanded.
    if (ev.buttons === 0) {
      activeHandle = null;
      cropDragStart = null;
      return;
    }
    if (activeHandle) {
      const p = clientToImage(ev.clientX, ev.clientY);
      cropRect = adjustRect(cropRect, activeHandle, p);
      return;
    }
    if (cropDragStart) {
      const dx = (ev.clientX - cropDragStart.mx) / zoom;
      const dy = (ev.clientY - cropDragStart.my) / zoom;
      const next = {
        x: cropDragStart.rect.x + dx,
        y: cropDragStart.rect.y + dy,
        w: cropDragStart.rect.w,
        h: cropDragStart.rect.h,
      };
      // Constrain so the rect can't be dragged outside the image.
      next.x = Math.max(0, Math.min(image.width - next.w, next.x));
      next.y = Math.max(0, Math.min(image.height - next.h, next.y));
      cropRect = next;
    }
  }

  function onCropPointerUp() {
    activeHandle = null;
    cropDragStart = null;
  }

  /** Resize-from-handle math. `id` says which edges move; mouse pos
   * becomes the new edge value. Negative w/h are normalised so a
   * "drag past the opposite handle" flips the rect cleanly. */
  function adjustRect(r: Rect, id: HandleId, p: { x: number; y: number }): Rect {
    let { x, y, w, h } = r;
    if (id.includes("n")) {
      h += y - p.y;
      y = p.y;
    }
    if (id.includes("s")) {
      h = p.y - y;
    }
    if (id.includes("w")) {
      w += x - p.x;
      x = p.x;
    }
    if (id.includes("e")) {
      w = p.x - x;
    }
    if (w < 0) {
      x += w;
      w = -w;
    }
    if (h < 0) {
      y += h;
      h = -h;
    }
    return clampRectToImage({ x, y, w, h });
  }

  function applyCrop() {
    if (!cropRect || !image) return;
    const r = clampRectToImage(cropRect);
    if (r.w < 1 || r.h < 1) return;

    const off = document.createElement("canvas");
    off.width = Math.round(r.w);
    off.height = Math.round(r.h);
    off.getContext("2d")?.drawImage(
      image,
      r.x,
      r.y,
      r.w,
      r.h,
      0,
      0,
      off.width,
      off.height,
    );

    pushHistory();
    image = off;
    // Translate existing annotations so they stay aligned with the
    // pixels they were drawn on. Items entirely outside the crop are
    // kept (translated): user can still undo to see them again.
    const dx = -r.x;
    const dy = -r.y;
    annotations = annotations.map((a) => translateAnnotation(a, dx, dy));
    // Explicit reset (vs. relying on the tool-switch effect): keeps
    // applyCrop self-contained if the trailing `tool = "arrow"` line
    // is ever refactored away.
    selected = null;
    cropRect = null;
    tool = "arrow";
    fit();
    scheduleRedraw();
  }

  function cancelCrop() {
    cropRect = null;
    tool = "arrow";
  }

  function translateAnnotation(a: Annotation, dx: number, dy: number): Annotation {
    if (a.kind === "arrow") {
      return { ...a, x1: a.x1 + dx, y1: a.y1 + dy, x2: a.x2 + dx, y2: a.y2 + dy };
    }
    if (a.kind === "rect" || a.kind === "blur" || a.kind === "text") {
      return { ...a, x: a.x + dx, y: a.y + dy };
    }
    if (a.kind === "number") {
      return { ...a, cx: a.cx + dx, cy: a.cy + dy };
    }
    return {
      ...a,
      points: a.points.map((p) => ({ x: p.x + dx, y: p.y + dy })),
    };
  }

  /** Open the resize popover. Seeds the W/H fields with the current
   * size so the user can tweak either side OR pick a % preset. */
  function openResize() {
    if (!image) return;
    resizeW = image.width;
    resizeH = image.height;
    resizeOpen = true;
  }

  function applyResizePreset(factor: number) {
    if (!image) return;
    resizeW = Math.round(image.width * factor);
    resizeH = Math.round(image.height * factor);
  }

  /** Highlight the preset button whose factor produces the current
   * width AND height — strict match (both axes) so typing a custom
   * value deselects every preset, exactly what the user expects. */
  function isResizePresetActive(factor: number): boolean {
    if (!image) return false;
    return (
      Math.round(image.width * factor) === resizeW &&
      Math.round(image.height * factor) === resizeH
    );
  }

  /** Mirror W↔H when aspect-lock is on. Each side updates the other
   * based on the source aspect ratio (uses the live image, not the
   * other field, so error doesn't accumulate). */
  function onResizeWChanged(value: string) {
    const n = Math.max(1, Math.floor(Number(value) || 0));
    resizeW = n;
    if (resizeLockAspect && image) {
      resizeH = Math.max(1, Math.round((n * image.height) / image.width));
    }
  }
  function onResizeHChanged(value: string) {
    const n = Math.max(1, Math.floor(Number(value) || 0));
    resizeH = n;
    if (resizeLockAspect && image) {
      resizeW = Math.max(1, Math.round((n * image.width) / image.height));
    }
  }

  function applyResize() {
    if (!image || resizeW < 1 || resizeH < 1) return;
    const off = document.createElement("canvas");
    off.width = Math.round(resizeW);
    off.height = Math.round(resizeH);
    const ctx = off.getContext("2d");
    if (!ctx) return;
    ctx.imageSmoothingEnabled = true;
    ctx.imageSmoothingQuality = "high";
    ctx.drawImage(image, 0, 0, off.width, off.height);

    // Scale annotations to the new size. Pixel positions move
    // proportionally; widths/strengths also scale so a 6px arrow at
    // 1× becomes 3px at 0.5× (still looks the same on screen).
    const sx = off.width / image.width;
    const sy = off.height / image.height;

    pushHistory();
    image = off;
    annotations = annotations.map((a) => scaleAnnotation(a, sx, sy));
    resizeOpen = false;
    fit();
    scheduleRedraw();
  }

  /** Spawn a fresh text draft at `(x, y)` (image-pixel coords). If
   * the user was already mid-text-edit, commit that one first so we
   * don't lose typed content. Font choices come from the dropdowns
   * (sticky across inserts). */
  function startText(x: number, y: number) {
    if (textDraft) commitText();
    textDraft = {
      x,
      y,
      text: "",
      color,
      fontSize: textFontSize,
      fontFamily: textFontFamily,
    };
    // Focus on next tick: the <textarea> doesn't exist until Svelte
    // mounts the overlay, which happens after the current pointer
    // event's microtask. `queueMicrotask` lands the focus right
    // after the DOM update.
    queueMicrotask(() => textInput?.focus());
  }

  /** Edit an existing text annotation in place. The annotation at
   * `index` is hidden from the canvas while editing (the textarea
   * overlay replaces it visually). Toolbar font/size/colour sync to
   * the annotation so the user sees the current style; changes
   * propagate live via the existing $effect mirror. */
  function startTextEdit(index: number) {
    const a = annotations[index];
    if (!a || a.kind !== "text") return;
    if (textDraft) commitText();
    editingIndex = index;
    textDraft = {
      x: a.x,
      y: a.y,
      text: a.text,
      color: a.color,
      fontSize: a.fontSize,
      fontFamily: a.fontFamily,
    };
    // Sync toolbar so subsequent typing picks up the same style.
    textFontFamily = a.fontFamily;
    textFontSize = a.fontSize;
    color = a.color;
    tool = "text";
    scheduleRedraw();
    queueMicrotask(() => {
      textInput?.focus();
      // Select all text so user can immediately type to replace.
      textInput?.select();
    });
  }

  /** Switching tools always clears any active selection — otherwise
   * the dashed bbox would linger and confuse the next interaction. */
  $effect(() => {
    if (tool !== "select" && selected !== null) selected = null;
  });

  /** Hit-test the annotation list back-to-front (topmost first, so a
   * later-drawn annotation wins over an older one beneath it). Returns
   * the index of the first hit, or null. */
  function hitTest(x: number, y: number): number | null {
    for (let i = annotations.length - 1; i >= 0; i -= 1) {
      const a = annotations[i];
      if (a && hitsAnnotation(a, x, y)) return i;
    }
    return null;
  }

  function hitsAnnotation(a: Annotation, x: number, y: number): boolean {
    if (a.kind === "rect" || a.kind === "blur") {
      const x0 = Math.min(a.x, a.x + a.w);
      const y0 = Math.min(a.y, a.y + a.h);
      const x1 = Math.max(a.x, a.x + a.w);
      const y1 = Math.max(a.y, a.y + a.h);
      return x >= x0 && x <= x1 && y >= y0 && y <= y1;
    }
    if (a.kind === "arrow") {
      const tol = Math.max(8, a.width + 4);
      return distancePointToSegment(x, y, a.x1, a.y1, a.x2, a.y2) <= tol;
    }
    if (a.kind === "pen" || a.kind === "highlight") {
      const tol = Math.max(8, a.width + 4);
      for (let i = 1; i < a.points.length; i += 1) {
        const p0 = a.points[i - 1];
        const p1 = a.points[i];
        if (p0 && p1 && distancePointToSegment(x, y, p0.x, p0.y, p1.x, p1.y) <= tol) {
          return true;
        }
      }
      return false;
    }
    if (a.kind === "number") {
      return Math.hypot(x - a.cx, y - a.cy) <= a.radius + 2;
    }
    if (a.kind === "text") {
      // Approximate bbox via canvas measurement.
      const ctx = canvas.getContext("2d");
      if (!ctx) return false;
      ctx.save();
      ctx.font = `${a.fontSize}px ${a.fontFamily}`;
      const lines = a.text.split("\n");
      const lineHeight = a.fontSize * 1.25;
      const w = Math.max(0, ...lines.map((l) => ctx.measureText(l).width));
      const h = lines.length * lineHeight;
      ctx.restore();
      return x >= a.x && x <= a.x + w && y >= a.y && y <= a.y + h;
    }
    return false;
  }

  function distancePointToSegment(
    px: number,
    py: number,
    x1: number,
    y1: number,
    x2: number,
    y2: number,
  ): number {
    const dx = x2 - x1;
    const dy = y2 - y1;
    const lenSq = dx * dx + dy * dy;
    if (lenSq === 0) return Math.hypot(px - x1, py - y1);
    let t = ((px - x1) * dx + (py - y1) * dy) / lenSq;
    t = Math.max(0, Math.min(1, t));
    return Math.hypot(px - (x1 + t * dx), py - (y1 + t * dy));
  }

  /** Bounding box of an annotation in image-pixel space — drives the
   * dashed selection outline. Returns null for kinds we don't draw a
   * marquee around (currently every kind has a bbox). */
  function bboxOf(a: Annotation): { x: number; y: number; w: number; h: number } | null {
    if (a.kind === "rect" || a.kind === "blur") {
      const x = Math.min(a.x, a.x + a.w);
      const y = Math.min(a.y, a.y + a.h);
      return { x, y, w: Math.abs(a.w), h: Math.abs(a.h) };
    }
    if (a.kind === "arrow") {
      const x = Math.min(a.x1, a.x2);
      const y = Math.min(a.y1, a.y2);
      return { x, y, w: Math.abs(a.x2 - a.x1), h: Math.abs(a.y2 - a.y1) };
    }
    if (a.kind === "pen" || a.kind === "highlight") {
      const xs = a.points.map((p) => p.x);
      const ys = a.points.map((p) => p.y);
      if (xs.length === 0) return null;
      const x = Math.min(...xs);
      const y = Math.min(...ys);
      return { x, y, w: Math.max(...xs) - x, h: Math.max(...ys) - y };
    }
    if (a.kind === "number") {
      return { x: a.cx - a.radius, y: a.cy - a.radius, w: a.radius * 2, h: a.radius * 2 };
    }
    if (a.kind === "text") {
      const ctx = canvas.getContext("2d");
      if (!ctx) return null;
      ctx.save();
      ctx.font = `${a.fontSize}px ${a.fontFamily}`;
      const lines = a.text.split("\n");
      const lineHeight = a.fontSize * 1.25;
      const w = Math.max(0, ...lines.map((l) => ctx.measureText(l).width));
      const h = lines.length * lineHeight;
      ctx.restore();
      return { x: a.x, y: a.y, w, h };
    }
    return null;
  }

  /** Next sequential number for badge tool. Reads the max existing
   * `n` in the annotations list so the count survives undo, delete,
   * and reorder — exactly what the user expects from CleanShot's
   * "Step" tool. */
  function nextBadgeNumber(): number {
    let max = 0;
    for (const a of annotations) {
      if (a.kind === "number" && a.n > max) max = a.n;
    }
    return max + 1;
  }

  function deleteSelected() {
    if (selected === null) return;
    pushHistory();
    annotations = annotations.filter((_, i) => i !== selected);
    selected = null;
    scheduleRedraw();
  }

  /** While the user has a live text draft, mirror the toolbar
   * choices (font + size + colour) into it so changes show up live
   * in the textarea AND will commit to the annotation as-is. */
  $effect(() => {
    if (textDraft) {
      textDraft.fontFamily = textFontFamily;
      textDraft.fontSize = textFontSize;
      textDraft.color = color;
    }
  });

  /** Blur handler for the text overlay. Pickers (font / size / colour)
   * live in the toolbar, outside the textarea, so opening one steals
   * focus and would commit the draft prematurely — losing the
   * in-progress edit. When the next focus target is a control marked
   * `data-text-control`, we keep the draft alive and bounce focus
   * back on the next microtask (after the picker handles its click).
   * Anything else (canvas, save, undo, click-outside) commits as
   * usual. */
  function onTextBlur(ev: FocusEvent) {
    const next = ev.relatedTarget as HTMLElement | null;
    if (next && next.closest("[data-text-control]")) {
      queueMicrotask(() => textInput?.focus());
      return;
    }
    commitText();
  }

  /** Materialise the draft into a real annotation. Called on
   * textarea blur, Esc, or before starting a new draft.
   *
   * Three paths:
   *   1. New text + non-empty   → append annotation
   *   2. Editing + non-empty    → replace at `editingIndex`
   *   3. Editing + empty        → delete annotation at `editingIndex`
   *   4. New text + empty       → no-op (just drop the draft)
   */
  function commitText() {
    if (!textDraft) return;
    const trimmed = textDraft.text.trim();
    const draft = textDraft;
    const editing = editingIndex;
    textDraft = null;
    editingIndex = null;

    if (editing !== null) {
      // Edit case: replace or remove the original.
      pushHistory();
      if (trimmed.length > 0) {
        annotations = annotations.map((a, i) =>
          i === editing
            ? {
                kind: "text",
                x: draft.x,
                y: draft.y,
                text: draft.text,
                color: draft.color,
                fontSize: draft.fontSize,
                fontFamily: draft.fontFamily,
              }
            : a,
        );
      } else {
        annotations = annotations.filter((_, i) => i !== editing);
      }
    } else if (trimmed.length > 0) {
      // New text case: append.
      pushHistory();
      annotations = [
        ...annotations,
        {
          kind: "text",
          x: draft.x,
          y: draft.y,
          text: draft.text,
          color: draft.color,
          fontSize: draft.fontSize,
          fontFamily: draft.fontFamily,
        },
      ];
    }
    scheduleRedraw();
  }

  function onTextKeyDown(ev: KeyboardEvent) {
    // Esc commits. (If the user typed nothing, `commitText` will
    // drop the empty draft, so it doubles as "cancel".) Stop
    // propagation so the global handler doesn't also try to dismiss
    // the editor.
    if (ev.key === "Escape") {
      ev.preventDefault();
      ev.stopPropagation();
      commitText();
    }
    // Plain Enter newlines the textarea by default — that's what
    // we want, so no preventDefault.
  }

  function scaleAnnotation(a: Annotation, sx: number, sy: number): Annotation {
    // Use the average of sx/sy for stroke widths — non-uniform scale
    // is rare (user picks % presets), so this stays visually correct.
    const sStroke = (sx + sy) / 2;
    if (a.kind === "arrow") {
      return {
        ...a,
        x1: a.x1 * sx,
        y1: a.y1 * sy,
        x2: a.x2 * sx,
        y2: a.y2 * sy,
        width: a.width * sStroke,
      };
    }
    if (a.kind === "rect") {
      return { ...a, x: a.x * sx, y: a.y * sy, w: a.w * sx, h: a.h * sy, width: a.width * sStroke };
    }
    if (a.kind === "blur") {
      return { ...a, x: a.x * sx, y: a.y * sy, w: a.w * sx, h: a.h * sy, strength: a.strength * sStroke };
    }
    if (a.kind === "text") {
      return { ...a, x: a.x * sx, y: a.y * sy, fontSize: a.fontSize * sStroke };
    }
    if (a.kind === "number") {
      return { ...a, cx: a.cx * sx, cy: a.cy * sy, radius: a.radius * sStroke };
    }
    return {
      ...a,
      points: a.points.map((p) => ({ x: p.x * sx, y: p.y * sy })),
      width: a.width * sStroke,
    };
  }

  /** Clamp+set zoom. If `anchorX/anchorY` (stage-local pixels) are
   * given, the point under that cursor stays fixed — the standard
   * cursor-anchored zoom you get in Figma / Photoshop. */
  function setZoom(target: number, anchorX?: number, anchorY?: number) {
    const next = Math.max(ZOOM_MIN, Math.min(ZOOM_MAX, target));
    if (anchorX !== undefined && anchorY !== undefined && zoom > 0) {
      const ratio = next / zoom;
      panX = anchorX - (anchorX - panX) * ratio;
      panY = anchorY - (anchorY - panY) * ratio;
    }
    zoom = next;
  }

  /** Fit-to-window: scale so the image (plus any background frame)
   * fits with 16px padding, never upscale above 1×, then centre it
   * inside the stage. */
  function fit() {
    if (!image || !stage) return;
    const rect = stage.getBoundingClientRect();
    const stagePad = 32;
    const { fullW, fullH } = frameMetrics();
    const sx = (rect.width - stagePad) / fullW;
    const sy = (rect.height - stagePad) / fullH;
    const z = Math.min(sx, sy, 1);
    zoom = z;
    panX = (rect.width - fullW * z) / 2;
    panY = (rect.height - fullH * z) / 2;
  }

  /** Reset to 100%, recentred on the stage. */
  function zoomActual() {
    if (!image || !stage) return;
    const rect = stage.getBoundingClientRect();
    const { fullW, fullH } = frameMetrics();
    panX = (rect.width - fullW) / 2;
    panY = (rect.height - fullH) / 2;
    zoom = 1;
  }

  /** Aspect-ratio numeric value (w/h) for the active id, or `null`
   * when the output should keep the natural padded shape. */
  function bgAspectRatio(): number | null {
    switch (bgAspect) {
      case "free": return null;
      case "1x1": return 1;
      case "16x9": return 16 / 9;
      case "9x16": return 9 / 16;
      case "4x3": return 4 / 3;
    }
  }

  /** Single source of truth for the framed-output geometry. Returns
   * the full canvas size plus the image's offset inside it. Used by
   * `redraw`, `clientToCanvas`, `fit`, and `zoomActual` — everywhere
   * else stays oblivious to the frame. Crop mode collapses to the
   * raw image (bg suppressed) so the crop overlay positioning stays
   * trivial. */
  function frameMetrics(): {
    offX: number;
    offY: number;
    fullW: number;
    fullH: number;
  } {
    if (!image || bgPreset === "none" || tool === "crop") {
      return {
        offX: 0,
        offY: 0,
        fullW: image?.width ?? 0,
        fullH: image?.height ?? 0,
      };
    }
    const baseW = image.width + bgPadding * 2;
    const baseH = image.height + bgPadding * 2;
    const aspect = bgAspectRatio();
    let fullW = baseW;
    let fullH = baseH;
    if (aspect !== null) {
      // Grow the shorter axis to hit the target ratio. Never shrink
      // below the padded base — the image always gets its margin.
      if (baseW / baseH < aspect) {
        fullW = Math.round(baseH * aspect);
      } else {
        fullH = Math.round(baseW / aspect);
      }
    }
    return {
      offX: Math.round((fullW - image.width) / 2),
      offY: Math.round((fullH - image.height) / 2),
      fullW,
      fullH,
    };
  }

  function currentBgPreset(): BgPreset {
    if (bgPreset === BG_CUSTOM_ID) {
      return { id: BG_CUSTOM_ID, kind: "solid", color: bgCustomColor };
    }
    return BG_PRESETS.find((p) => p.id === bgPreset) ?? BG_PRESETS[0]!;
  }

  /** Build (or reuse) a heavily-blurred, slightly-darkened miniature
   * of the source image. Cached by source-canvas identity so the cost
   * is paid once per source — re-renders just stretch the cached
   * bitmap. Downsampling before blur is the standard trick for cheap
   * gaussian on large frames: blur(20) on a 500-px miniature looks
   * the same as blur(80) on the original, at a fraction of the cost. */
  function getBgBlur(): HTMLCanvasElement | null {
    if (!image) return null;
    if (bgBlurCacheSource === image && bgBlurCache) return bgBlurCache;
    const max = 500;
    const s = Math.min(max / image.width, max / image.height, 1);
    const sw = Math.max(1, Math.round(image.width * s));
    const sh = Math.max(1, Math.round(image.height * s));
    const c = document.createElement("canvas");
    c.width = sw;
    c.height = sh;
    const cx = c.getContext("2d");
    if (!cx) return null;
    cx.filter = "blur(20px)";
    cx.drawImage(image, 0, 0, sw, sh);
    cx.filter = "none";
    // Subtle darken — keeps the foreground card legible on bright
    // images without going all the way to a black overlay.
    cx.fillStyle = "rgba(0, 0, 0, 0.16)";
    cx.fillRect(0, 0, sw, sh);
    bgBlurCache = c;
    bgBlurCacheSource = image;
    return c;
  }

  function paintBackground(ctx: CanvasRenderingContext2D, w: number, h: number) {
    const preset = currentBgPreset();
    if (preset.kind === "solid") {
      ctx.fillStyle = preset.color;
      ctx.fillRect(0, 0, w, h);
      return;
    }
    if (preset.kind === "gradient") {
      const a = (preset.angle * Math.PI) / 180;
      const cx = w / 2;
      const cy = h / 2;
      const len = Math.hypot(w, h) / 2;
      const gx = Math.cos(a) * len;
      const gy = Math.sin(a) * len;
      const g = ctx.createLinearGradient(cx - gx, cy - gy, cx + gx, cy + gy);
      g.addColorStop(0, preset.from);
      g.addColorStop(1, preset.to);
      ctx.fillStyle = g;
      ctx.fillRect(0, 0, w, h);
      return;
    }
    if (preset.kind === "blur") {
      const src = getBgBlur();
      if (!src) return;
      // Cover-fit so the blur fills the frame without showing
      // transparent edges, regardless of aspect.
      const r = Math.max(w / src.width, h / src.height);
      const dw = src.width * r;
      const dh = src.height * r;
      ctx.drawImage(src, (w - dw) / 2, (h - dh) / 2, dw, dh);
    }
  }

  function setBgPreset(id: string) {
    if (bgPreset === id) return;
    pushHistory();
    bgPreset = id;
    fit();
    scheduleRedraw();
  }
  function setBgPadding(v: number) {
    if (bgPadding === v) return;
    pushHistory();
    bgPadding = v;
    fit();
    scheduleRedraw();
  }
  function setBgRadius(v: number) {
    if (bgRadius === v) return;
    pushHistory();
    bgRadius = v;
    scheduleRedraw();
  }
  function toggleBgShadow(v: boolean) {
    if (bgShadow === v) return;
    pushHistory();
    bgShadow = v;
    scheduleRedraw();
  }
  /** Live updates from the strength slider during a drag. Snapshots
   * history exactly once — on the first frame — so the whole drag
   * collapses to a single undo step that restores the pre-drag
   * value. Cheap: only state mutation + a `scheduleRedraw`, no
   * canvas work happens here. */
  function previewBgShadowStrength(v: number) {
    if (bgShadowStrength === v) return;
    if (!bgShadowDragging) {
      pushHistory();
      bgShadowDragging = true;
    }
    bgShadowStrength = v;
    dirty = true;
    scheduleRedraw();
  }
  /** Slider `onchange` (release / keyboard step). Ends the drag
   * group so the next interaction starts a fresh history entry. */
  function commitBgShadowStrength() {
    bgShadowDragging = false;
  }
  function setBgAspect(v: BgAspectId) {
    if (bgAspect === v) return;
    pushHistory();
    bgAspect = v;
    fit();
    scheduleRedraw();
  }
  /** Native OS picker emits live as the user drags through the
   * dialog — a snapshot per frame would flood the undo stack. We
   * snapshot once on switching INTO the custom preset, then update
   * the colour in place. Subsequent preset switches push a fresh
   * snapshot via `setBgPreset`. */
  function setBgCustomColor(c: string) {
    const normalized = c.toLowerCase();
    // Coming from "none" the canvas dimensions change (image-only →
    // image+padding/frame), so `fit()` is needed to recentre the
    // viewport — otherwise the custom-coloured frame lands off-screen
    // or cropped at the stage edge.
    const wasNone = bgPreset === "none";
    if (bgPreset !== BG_CUSTOM_ID) {
      pushHistory();
      bgPreset = BG_CUSTOM_ID;
    }
    bgCustomColor = normalized;
    dirty = true;
    if (wasNone) fit();
    scheduleRedraw();
  }
  function resetBackground() {
    if (
      bgPreset === BG_DEFAULT_PRESET &&
      bgPadding === BG_DEFAULT_PADDING &&
      bgRadius === BG_DEFAULT_RADIUS &&
      bgShadow === BG_DEFAULT_SHADOW &&
      bgShadowStrength === BG_DEFAULT_SHADOW_STRENGTH &&
      bgAspect === BG_DEFAULT_ASPECT &&
      bgCustomColor === BG_DEFAULT_CUSTOM_COLOR
    ) {
      return;
    }
    pushHistory();
    bgPreset = BG_DEFAULT_PRESET;
    bgPadding = BG_DEFAULT_PADDING;
    bgRadius = BG_DEFAULT_RADIUS;
    bgShadow = BG_DEFAULT_SHADOW;
    bgShadowStrength = BG_DEFAULT_SHADOW_STRENGTH;
    bgAspect = BG_DEFAULT_ASPECT;
    bgCustomColor = BG_DEFAULT_CUSTOM_COLOR;
    fit();
    scheduleRedraw();
  }

  /** Coalesce multiple `redraw()` requests within the same frame.
   * Pointer events at 60 Hz used to trigger 60 full canvas redraws
   * per second; with rAF, the browser paints exactly once per frame
   * regardless of how many handlers call us. Saves ~30-50% CPU on
   * pen / pan / drag flows over 4K captures. */
  let rafPending = false;
  function scheduleRedraw() {
    if (rafPending) return;
    rafPending = true;
    requestAnimationFrame(() => {
      rafPending = false;
      redraw();
    });
  }

  function redraw() {
    if (!canvas || !image) return;
    const ctx = canvas.getContext("2d");
    if (!ctx) return;

    const { offX, offY, fullW, fullH } = frameMetrics();
    // Resize-on-demand: avoids the implicit context reset when
    // dimensions don't actually change between frames.
    if (canvas.width !== fullW) canvas.width = fullW;
    if (canvas.height !== fullH) canvas.height = fullH;

    ctx.clearRect(0, 0, fullW, fullH);

    const bgActive = bgPreset !== "none" && tool !== "crop";
    if (bgActive) {
      paintBackground(ctx, fullW, fullH);
      // Card shadow: a single offset+blurred fill of the rounded
      // image bounds. Drawing the shadow this way (instead of
      // shadow-while-drawImage) avoids re-shadowing every pixel of a
      // 4K image and lets us reset shadow state before drawing the
      // image and annotations.
      if (bgShadow) {
        // Scale with the user-picked padding, NOT the actual offset —
        // an aspect-ratio crop (e.g. 16:9 on a portrait image) can
        // push offY to many hundreds of px and would otherwise blow
        // the shadow up to absurd proportions.
        const baseScale = Math.max(bgPadding, 16);
        // Opacity is the user-tunable axis; blur/offset stay locked
        // to padding so the shadow's geometry keeps matching the
        // card's scale. Strength → opacity is linear and capped at
        // 0.7 (full-black past that just turns the corners muddy).
        const shadowOpacity =
          BG_SHADOW_MAX_OPACITY * (bgShadowStrength / 100);
        ctx.save();
        ctx.shadowColor = `rgba(0, 0, 0, ${shadowOpacity})`;
        ctx.shadowBlur = Math.max(16, baseScale * 0.6);
        ctx.shadowOffsetY = Math.max(8, baseScale * 0.25);
        ctx.fillStyle = "#000";
        ctx.beginPath();
        ctx.roundRect(offX, offY, image.width, image.height, bgRadius);
        ctx.fill();
        ctx.restore();
      }
    }

    ctx.save();
    if (bgActive && bgRadius > 0) {
      // Clip the image + annotation layer to a rounded rect so
      // strokes that brush the edge respect the card silhouette.
      ctx.beginPath();
      ctx.roundRect(offX, offY, image.width, image.height, bgRadius);
      ctx.clip();
    }
    if (offX > 0 || offY > 0) ctx.translate(offX, offY);
    ctx.drawImage(image, 0, 0);
    for (let i = 0; i < annotations.length; i += 1) {
      // The annotation currently being edited is hidden — the
      // textarea overlay covers its position. Drawing it would
      // double-up under the textarea.
      if (i === editingIndex) continue;
      const a = annotations[i];
      if (a) drawAnnotation(ctx, a);
    }
    if (drafting) drawAnnotation(ctx, drafting);
    // Selection marquee — drawn AFTER annotations so it always sits
    // on top. Dashed outline matching the crop visual language;
    // padded 4px around the bbox for breathing room.
    if (selected !== null && tool === "select") {
      const a = annotations[selected];
      if (a) {
        const bb = bboxOf(a);
        if (bb) {
          ctx.lineWidth = 1.5;
          ctx.strokeStyle = "rgba(255, 255, 255, 0.95)";
          ctx.setLineDash([6, 4]);
          ctx.strokeRect(bb.x - 4, bb.y - 4, bb.w + 8, bb.h + 8);
        }
      }
    }
    ctx.restore();
  }

  function drawAnnotation(ctx: CanvasRenderingContext2D, a: Annotation) {
    if (a.kind === "blur") {
      drawBlur(ctx, a);
      return;
    }
    if (a.kind === "text") {
      drawText(ctx, a);
      return;
    }
    if (a.kind === "number") {
      drawNumber(ctx, a);
      return;
    }
    // Past the early return TS narrows `a` to the colour+width shapes.
    ctx.lineCap = "round";
    ctx.lineJoin = "round";
    ctx.strokeStyle = a.color;
    ctx.fillStyle = a.color;
    ctx.lineWidth = a.width;
    if (a.kind === "highlight") {
      const prev = ctx.globalAlpha;
      ctx.globalAlpha = 0.35;
      drawPath(ctx, a.points);
      ctx.globalAlpha = prev;
    } else if (a.kind === "pen") {
      drawPath(ctx, a.points);
    } else if (a.kind === "rect") {
      ctx.strokeRect(a.x, a.y, a.w, a.h);
    } else {
      drawArrow(ctx, a.x1, a.y1, a.x2, a.y2, a.width);
    }
  }

  /** Filled circle with a centered number. Drop shadow for legibility
   * on any background. Text colour auto-picks white/black based on
   * the badge fill brightness (high-contrast badges on light fills). */
  function drawNumber(
    ctx: CanvasRenderingContext2D,
    a: { kind: "number"; cx: number; cy: number; n: number; color: string; radius: number },
  ) {
    ctx.save();
    ctx.shadowColor = "rgba(0,0,0,0.45)";
    ctx.shadowBlur = 6;
    ctx.shadowOffsetY = 2;
    ctx.fillStyle = a.color;
    ctx.beginPath();
    ctx.arc(a.cx, a.cy, a.radius, 0, Math.PI * 2);
    ctx.fill();
    // Reset shadow so the number text isn't doubly-blurred.
    ctx.shadowColor = "transparent";
    ctx.fillStyle = contrastingTextColor(a.color);
    const fontSize = Math.round(a.radius * 1.1);
    ctx.font = `700 ${fontSize}px ${TEXT_FONTS[0].value}`;
    ctx.textAlign = "center";
    ctx.textBaseline = "middle";
    // +1 visual offset: most fonts sit slightly above the baseline
    // for digits; nudges the number to feel centred.
    ctx.fillText(String(a.n), a.cx, a.cy + 1);
    ctx.restore();
  }

  /** Black/white for the badge number based on the fill brightness.
   * Pure luminance approximation — sufficient for the 6 colours we
   * expose, and degrades gracefully for any user-typed hex. */
  function contrastingTextColor(hex: string): string {
    if (hex.length !== 7 || hex[0] !== "#") return "#ffffff";
    const r = parseInt(hex.slice(1, 3), 16);
    const g = parseInt(hex.slice(3, 5), 16);
    const b = parseInt(hex.slice(5, 7), 16);
    const lum = (0.299 * r + 0.587 * g + 0.114 * b) / 255;
    return lum > 0.6 ? "#000" : "#fff";
  }

  /** Paint a text annotation. Multiline is supported (split by `\n`),
   * subtle drop shadow keeps it legible on any background. Matches
   * the textarea overlay exactly so commit/uncommit looks seamless. */
  function drawText(
    ctx: CanvasRenderingContext2D,
    a: {
      kind: "text";
      x: number;
      y: number;
      text: string;
      color: string;
      fontSize: number;
      fontFamily: string;
    },
  ) {
    if (!a.text) return;
    ctx.save();
    ctx.fillStyle = a.color;
    ctx.font = `${a.fontSize}px ${a.fontFamily}`;
    ctx.textBaseline = "top";
    // Soft shadow → text stays readable on any background colour.
    ctx.shadowColor = "rgba(0, 0, 0, 0.55)";
    ctx.shadowBlur = 3;
    ctx.shadowOffsetY = 1;
    const lines = a.text.split("\n");
    const lineHeight = a.fontSize * 1.25;
    for (let i = 0; i < lines.length; i += 1) {
      const line = lines[i] ?? "";
      ctx.fillText(line, a.x, a.y + i * lineHeight);
    }
    ctx.restore();
  }

  /** Gaussian-blur the source pixels inside the rect. The clip + the
   * blur radius margin combo matters: if we just `drawImage(image)`
   * with the filter set, the blur would extend beyond the rect into
   * neighboring annotations. The clip confines it; the margin around
   * the rect prevents a soft transparent halo at the clip boundary
   * (browser blurs against transparent outside the source draw). */
  function drawBlur(
    ctx: CanvasRenderingContext2D,
    a: { kind: "blur"; x: number; y: number; w: number; h: number; strength: number },
  ) {
    if (!image) return;
    // Normalize: the user can drag any direction.
    const ax = Math.min(a.x, a.x + a.w);
    const ay = Math.min(a.y, a.y + a.h);
    const aw = Math.abs(a.w);
    const ah = Math.abs(a.h);
    if (aw < 2 || ah < 2) return;

    const strength = Math.max(2, a.strength);
    // 1.5× the radius covers the gaussian falloff in practice.
    const margin = Math.ceil(strength * 1.5);

    ctx.save();
    ctx.beginPath();
    ctx.rect(ax, ay, aw, ah);
    ctx.clip();
    ctx.filter = `blur(${strength}px)`;

    // Draw the source region with margin so the blur kernel has real
    // pixels to sample outside the clip — otherwise the edges fade
    // into transparency.
    const sx = Math.max(0, ax - margin);
    const sy = Math.max(0, ay - margin);
    const sw = Math.min(image.width - sx, aw + margin * 2);
    const sh = Math.min(image.height - sy, ah + margin * 2);
    if (sw > 0 && sh > 0) {
      ctx.drawImage(image, sx, sy, sw, sh, sx, sy, sw, sh);
    }
    ctx.restore();
  }

  function drawPath(ctx: CanvasRenderingContext2D, points: Point[]) {
    const first = points[0];
    if (!first) return;
    ctx.beginPath();
    ctx.moveTo(first.x, first.y);
    for (let i = 1; i < points.length; i += 1) {
      const p = points[i];
      if (p) ctx.lineTo(p.x, p.y);
    }
    ctx.stroke();
  }

  /** Photoshop "Arrow 1" / Figma / Notion / Linear default —
   * a constant-width shaft with a clean triangular head. The
   * universal annotation arrow: reads sharply on any background,
   * the head dominates so direction is unambiguous, no chevron or
   * taper gimmicks. Single filled polygon (7 vertices), so no
   * stroke + triangle gap-fighting and no separate path math. */
  function drawArrow(
    ctx: CanvasRenderingContext2D,
    x1: number,
    y1: number,
    x2: number,
    y2: number,
    w: number,
  ) {
    const dx = x2 - x1;
    const dy = y2 - y1;
    const len = Math.hypot(dx, dy);
    if (len < 1) return;

    // Unit direction + perpendicular normal (CCW rotation).
    const ux = dx / len;
    const uy = dy / len;
    const nx = -uy;
    const ny = ux;

    // Head ~4× the stroke width, capped at 55% of total length so
    // a short arrow stays balanced (head not dwarfing the shaft).
    const headLen = Math.min(Math.max(w * 4, 18), len * 0.55);
    const headHalf = w * 1.3;  // flare half-width — ~2.6× shaft
    const shaftHalf = w / 2;

    // Junction between shaft end and head base.
    const jx = x1 + ux * (len - headLen);
    const jy = y1 + uy * (len - headLen);

    ctx.beginPath();
    ctx.moveTo(x1 + nx * shaftHalf, y1 + ny * shaftHalf); // tail TOP
    ctx.lineTo(jx + nx * shaftHalf, jy + ny * shaftHalf); // shaft-end TOP
    ctx.lineTo(jx + nx * headHalf, jy + ny * headHalf);   // flare TOP
    ctx.lineTo(x2, y2);                                    // tip
    ctx.lineTo(jx - nx * headHalf, jy - ny * headHalf);   // flare BOT
    ctx.lineTo(jx - nx * shaftHalf, jy - ny * shaftHalf); // shaft-end BOT
    ctx.lineTo(x1 - nx * shaftHalf, y1 - ny * shaftHalf); // tail BOT
    ctx.closePath();
    ctx.fill();
  }

  function clientToCanvas(ev: PointerEvent): Point {
    const rect = canvas.getBoundingClientRect();
    const sx = canvas.width / rect.width;
    const sy = canvas.height / rect.height;
    // Subtract the frame offset so callers always receive image-
    // pixel coords. Without this every hit-test / drafting math would
    // need to know about the frame; instead it stays a redraw-only
    // concern.
    const { offX, offY } = frameMetrics();
    return {
      x: (ev.clientX - rect.left) * sx - offX,
      y: (ev.clientY - rect.top) * sy - offY,
    };
  }

  function effectiveWidth(): number {
    return tool === "highlight" ? Math.max(width * 3, 14) : width;
  }

  function onPointerDown(ev: PointerEvent) {
    if (!image) return;
    // Pointer-events on widgets that overlay the stage (zoom-bar,
    // future toolbars) must reach their own buttons. Without this
    // guard, `stage.setPointerCapture` below steals the pointerup, so
    // the synthetic `click` never fires and the buttons feel dead.
    if ((ev.target as HTMLElement | null)?.closest("button, .zoom-bar")) {
      return;
    }
    // Three paths to panning:
    //   1. Middle-click drag (any tool)
    //   2. Space + left-click drag (any tool — power-user shortcut)
    //   3. Hand tool active + left-click drag (discoverable affordance)
    const wantPan =
      ev.button === 1 ||
      (ev.button === 0 && (spaceHeld || tool === "hand"));
    if (wantPan) {
      panning = true;
      lastPanPoint = { x: ev.clientX, y: ev.clientY };
      stage.setPointerCapture(ev.pointerId);
      ev.preventDefault();
      return;
    }
    // In crop mode, the overlay (handles + rect) owns the pointer.
    // Bail BEFORE setPointerCapture so the overlay actually receives
    // its drag events. Middle-click pan above still works (returns).
    if (tool === "crop") return;
    if (ev.button !== 0) return;
    // Select tool: hit-test, maybe start a move drag, or deselect
    // when the user clicks empty space.
    if (tool === "select") {
      ev.preventDefault();
      const p = clientToCanvas(ev);
      const hit = hitTest(p.x, p.y);
      selected = hit;
      if (hit !== null) {
        const a = annotations[hit];
        if (a) {
          moveStart = { mx: ev.clientX, my: ev.clientY, original: a };
          stage.setPointerCapture(ev.pointerId);
        }
      }
      scheduleRedraw();
      return;
    }
    // Text tool: click on existing text annotation enters edit mode
    // for that one; click on empty area starts a fresh draft.
    if (tool === "text") {
      ev.preventDefault();
      const p = clientToCanvas(ev);
      const hit = hitTest(p.x, p.y);
      if (hit !== null && annotations[hit]?.kind === "text") {
        startTextEdit(hit);
      } else {
        startText(p.x, p.y);
      }
      return;
    }
    // Number tool: stamp a numbered badge at the click point. Auto-
    // increment from the max existing number in the list, so a
    // sequence stays sequential even after undo / delete.
    // If the user clicks ON top of an existing badge, swap to Select
    // and select it instead of stacking a new badge on the same spot
    // (annoying behaviour that was flagged in the editor audit).
    if (tool === "number") {
      ev.preventDefault();
      const p = clientToCanvas(ev);
      const hit = hitTest(p.x, p.y);
      if (hit !== null && annotations[hit]?.kind === "number") {
        tool = "select";
        selected = hit;
        scheduleRedraw();
        return;
      }
      const nextN = nextBadgeNumber();
      pushHistory();
      annotations = [
        ...annotations,
        {
          kind: "number",
          cx: p.x,
          cy: p.y,
          n: nextN,
          color,
          // `numberSize` is the diameter the user picked; canvas
          // wants the radius. Halving here keeps the rest of the
          // pipeline (rendering, hit-test, scale, translate)
          // working off the same `radius` field as before.
          radius: numberSize / 2,
        },
      ];
      scheduleRedraw();
      return;
    }
    const p = clientToCanvas(ev);
    stage.setPointerCapture(ev.pointerId);
    const w = effectiveWidth();
    if (tool === "arrow") {
      drafting = { kind: "arrow", x1: p.x, y1: p.y, x2: p.x, y2: p.y, color, width: w };
    } else if (tool === "rect") {
      drafting = { kind: "rect", x: p.x, y: p.y, w: 0, h: 0, color, width: w };
    } else if (tool === "pen" || tool === "highlight") {
      drafting = { kind: tool, points: [p], color, width: w };
    } else if (tool === "blur") {
      // Blur radius scales with the width picker so the user can pick
      // softer / heavier redactions. Floor at 6px so a tiny width
      // still smears text past readability.
      drafting = {
        kind: "blur",
        x: p.x,
        y: p.y,
        w: 0,
        h: 0,
        strength: Math.max(6, width * 2),
      };
    }
    // tool === "hand" never reaches here — handled in `wantPan` branch.
    scheduleRedraw();
  }

  function onPointerMove(ev: PointerEvent) {
    if (panning && lastPanPoint) {
      panX += ev.clientX - lastPanPoint.x;
      panY += ev.clientY - lastPanPoint.y;
      lastPanPoint = { x: ev.clientX, y: ev.clientY };
      return;
    }
    // Dragging the selected annotation: translate from the original
    // by the cumulative mouse delta. Using the snapshot avoids the
    // drift you'd get from incremental updates.
    if (moveStart && selected !== null) {
      const dx = (ev.clientX - moveStart.mx) / zoom;
      const dy = (ev.clientY - moveStart.my) / zoom;
      annotations = annotations.map((a, i) =>
        i === selected ? translateAnnotation(moveStart!.original, dx, dy) : a,
      );
      scheduleRedraw();
      return;
    }
    if (!drafting) return;
    const p = clientToCanvas(ev);
    // Shift held → constrain the in-progress shape. Standard editor
    // affordance (Figma / Photoshop / Excalidraw): arrows snap to
    // 45° angles; rectangles / blurs become perfect squares. Pen and
    // highlight stay freeform — constraining them makes no sense.
    if (drafting.kind === "arrow") {
      if (ev.shiftKey) {
        const dx = p.x - drafting.x1;
        const dy = p.y - drafting.y1;
        const step = Math.PI / 4;
        const angle = Math.round(Math.atan2(dy, dx) / step) * step;
        const dist = Math.hypot(dx, dy);
        drafting.x2 = drafting.x1 + Math.cos(angle) * dist;
        drafting.y2 = drafting.y1 + Math.sin(angle) * dist;
      } else {
        drafting.x2 = p.x;
        drafting.y2 = p.y;
      }
    } else if (drafting.kind === "rect" || drafting.kind === "blur") {
      let dw = p.x - drafting.x;
      let dh = p.y - drafting.y;
      if (ev.shiftKey) {
        // Square: pick the dominant axis, mirror its sign to both
        // sides so the rect grows in whichever quadrant the cursor is.
        const size = Math.max(Math.abs(dw), Math.abs(dh));
        dw = (dw >= 0 ? 1 : -1) * size;
        dh = (dh >= 0 ? 1 : -1) * size;
      }
      drafting.w = dw;
      drafting.h = dh;
    } else if (drafting.kind === "pen" || drafting.kind === "highlight") {
      if (ev.shiftKey) {
        // Shift-constrain: collapse the in-progress path to its
        // anchor (first point) + the current cursor → straight
        // line. Photoshop's brush-with-shift idiom. Releasing
        // Shift mid-drag resumes freeform from that point.
        const first = drafting.points[0];
        if (first) drafting.points = [first, p];
      } else {
        drafting.points.push(p);
      }
    }
    // Text annotations never enter the `drafting` pipeline; they
    // route through `textDraft` and `commitText`.
    scheduleRedraw();
  }

  function onPointerUp(ev: PointerEvent) {
    if (panning) {
      panning = false;
      lastPanPoint = null;
      try {
        stage.releasePointerCapture(ev.pointerId);
      } catch {
        // capture may have moved silently — fine
      }
      return;
    }
    // End-of-move for the select tool. We snapshot the pre-move
    // state into history only if the position actually changed
    // (a click without drag shouldn't pollute undo).
    if (moveStart && selected !== null) {
      try {
        stage.releasePointerCapture(ev.pointerId);
      } catch {
        // ignore
      }
      const moved = annotations[selected];
      if (moved && moved !== moveStart.original) {
        // Push the pre-move state; replace the live entry with the
        // original first so pushHistory captures the right baseline.
        const dx = ev.clientX - moveStart.mx;
        const dy = ev.clientY - moveStart.my;
        if (Math.hypot(dx, dy) > 2) {
          const movedNow = moved;
          annotations = annotations.map((a, i) =>
            i === selected ? moveStart!.original : a,
          );
          pushHistory();
          annotations = annotations.map((a, i) =>
            i === selected ? movedNow : a,
          );
        }
      }
      moveStart = null;
      return;
    }
    if (!drafting) return;
    try {
      stage.releasePointerCapture(ev.pointerId);
    } catch {
      // ignore
    }
    // Skip "degenerate" drafts (a single click on arrow/rect makes a
    // zero-length stroke that's invisible and just clutters undo).
    const keep = isMeaningful(drafting);
    if (keep) {
      pushHistory();
      annotations = [...annotations, drafting];
    }
    drafting = null;
    scheduleRedraw();
  }

  /** Select tool + double-click on a text annotation → edit it
   * in place. Matches Figma / Sketch / Excalidraw / CleanShot
   * convention. Other tools ignore double-click. */
  function onStageDoubleClick(ev: MouseEvent) {
    if (tool !== "select" || !image) return;
    if ((ev.target as HTMLElement | null)?.closest("button, .zoom-bar, .crop-frame")) return;
    const rect = canvas.getBoundingClientRect();
    const sx = canvas.width / rect.width;
    const sy = canvas.height / rect.height;
    const p = {
      x: (ev.clientX - rect.left) * sx,
      y: (ev.clientY - rect.top) * sy,
    };
    const hit = hitTest(p.x, p.y);
    if (hit !== null && annotations[hit]?.kind === "text") {
      startTextEdit(hit);
    }
  }

  /** Ctrl/Cmd + wheel → cursor-anchored zoom. Plain wheel → pan. */
  function onWheel(ev: WheelEvent) {
    if (!image) return;
    if (ev.ctrlKey || ev.metaKey) {
      ev.preventDefault();
      const rect = stage.getBoundingClientRect();
      const ax = ev.clientX - rect.left;
      const ay = ev.clientY - rect.top;
      const factor = ev.deltaY < 0 ? ZOOM_STEP : 1 / ZOOM_STEP;
      setZoom(zoom * factor, ax, ay);
      return;
    }
    ev.preventDefault();
    panX -= ev.deltaX;
    panY -= ev.deltaY;
  }

  function isMeaningful(a: Annotation): boolean {
    if (a.kind === "arrow") {
      return Math.hypot(a.x2 - a.x1, a.y2 - a.y1) > 4;
    }
    if (a.kind === "rect" || a.kind === "blur") {
      return Math.abs(a.w) > 4 && Math.abs(a.h) > 4;
    }
    if (a.kind === "pen" || a.kind === "highlight") {
      return a.points.length > 1;
    }
    // Text annotations are committed via `commitText` (which already
    // skips empty text); the draft pipeline never sees them.
    return true;
  }

  function undo() {
    const prev = history.pop();
    if (!prev) return;
    // Snapshot the CURRENT state into redo before swapping. This is
    // the state Ctrl+Y will restore.
    if (image) {
      redoHistory.push({
        image: cloneCanvas(image),
        annotations: [...annotations],
        bgPreset,
        bgPadding,
        bgRadius,
        bgShadow,
        bgShadowStrength,
        bgAspect,
        bgCustomColor,
      });
    }
    image = prev.image;
    annotations = prev.annotations;
    bgPreset = prev.bgPreset;
    bgPadding = prev.bgPadding;
    bgRadius = prev.bgRadius;
    bgShadow = prev.bgShadow;
    bgShadowStrength = prev.bgShadowStrength;
    bgAspect = prev.bgAspect;
    bgCustomColor = prev.bgCustomColor;
    dirty = history.length > 0;
    // Re-fit so a cropped → undone restoration recentres the larger
    // image. Cheap (no encoding); user expects the framing to recover.
    fit();
    scheduleRedraw();
  }

  function redo() {
    const next = redoHistory.pop();
    if (!next) return;
    // Mirror of `undo`: park the CURRENT state in history before
    // restoring the popped redo snapshot.
    if (image) {
      history.push({
        image: cloneCanvas(image),
        annotations: [...annotations],
        bgPreset,
        bgPadding,
        bgRadius,
        bgShadow,
        bgShadowStrength,
        bgAspect,
        bgCustomColor,
      });
      if (history.length > HISTORY_LIMIT) history.shift();
    }
    image = next.image;
    annotations = next.annotations;
    bgPreset = next.bgPreset;
    bgPadding = next.bgPadding;
    bgRadius = next.bgRadius;
    bgShadow = next.bgShadow;
    bgShadowStrength = next.bgShadowStrength;
    bgAspect = next.bgAspect;
    bgCustomColor = next.bgCustomColor;
    dirty = true;
    fit();
    scheduleRedraw();
  }

  function toPngBytes(): Promise<Uint8Array> {
    return new Promise((resolve, reject) => {
      canvas.toBlob(async (blob) => {
        if (!blob) {
          reject(new Error("canvas.toBlob returned null"));
          return;
        }
        resolve(new Uint8Array(await blob.arrayBuffer()));
      }, "image/png");
    });
  }

  async function doSave() {
    if (!sourcePath || busy) return;
    busy = true;
    error = null;
    try {
      const bytes = await toPngBytes();
      await invoke("save_annotated", { path: sourcePath, bytes });
      dirty = false;
      await dismiss(true);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function doCopy() {
    if (busy) return;
    busy = true;
    error = null;
    try {
      const bytes = await toPngBytes();
      await invoke("copy_annotated_to_clipboard", { bytes });
      copied = true;
      clearTimeout(copyTimer);
      copyTimer = window.setTimeout(() => (copied = false), COPIED_RESET_MS);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function dismiss(skipDirtyCheck = false) {
    if (!skipDirtyCheck && dirty) {
      const ok = window.confirm("Discard unsaved annotations?");
      if (!ok) return;
    }
    clearTimeout(copyTimer);
    try {
      await win.hide();
    } catch (e) {
      console.error("editor hide", e);
    }
  }

  function onKey(ev: KeyboardEvent) {
    // Don't fight typing inside form controls — none today, but cheap
    // future-proofing.
    const target = ev.target as HTMLElement | null;
    if (target?.tagName === "INPUT" || target?.tagName === "TEXTAREA") return;

    if (ev.key === "Escape") {
      ev.preventDefault();
      // ESC inside crop mode cancels the crop (matches CleanShot /
      // Snagit); only escapes the window when no modal flow is open.
      if (tool === "crop") {
        cancelCrop();
        return;
      }
      if (resizeOpen) {
        resizeOpen = false;
        return;
      }
      if (selected !== null) {
        selected = null;
        scheduleRedraw();
        return;
      }
      void dismiss();
      return;
    }
    // Delete / Backspace removes the selected annotation. Backspace
    // also so users coming from Photoshop / Linear feel at home.
    if ((ev.key === "Delete" || ev.key === "Backspace") && selected !== null) {
      ev.preventDefault();
      deleteSelected();
      return;
    }
    if (ev.key === "Enter") {
      if (tool === "crop") {
        ev.preventDefault();
        applyCrop();
        return;
      }
      if (resizeOpen) {
        ev.preventDefault();
        applyResize();
        return;
      }
    }
    if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "z") {
      ev.preventDefault();
      // Shift+Z = Redo (Photoshop / Linear convention). Plain Z = Undo.
      if (ev.shiftKey) redo();
      else undo();
      return;
    }
    if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "y") {
      ev.preventDefault();
      redo();
      return;
    }
    if (ev.key === " " && !ev.repeat) {
      spaceHeld = true;
      ev.preventDefault();
      return;
    }
    // Tool shortcut: H toggles Hand. Matches Figma/Photoshop.
    if (ev.key.toLowerCase() === "h" && !ev.ctrlKey && !ev.metaKey) {
      ev.preventDefault();
      tool = tool === "hand" ? "arrow" : "hand";
      return;
    }
    // T → Text tool. Standard across image editors.
    if (ev.key.toLowerCase() === "t" && !ev.ctrlKey && !ev.metaKey) {
      ev.preventDefault();
      tool = "text";
      return;
    }
    // V → Select / Move (Figma convention).
    if (ev.key.toLowerCase() === "v" && !ev.ctrlKey && !ev.metaKey) {
      ev.preventDefault();
      tool = "select";
      return;
    }
    // N → Number badge tool (CleanShot uses S, but S would clash
    // with Save; N for "number" is unambiguous).
    if (ev.key.toLowerCase() === "n" && !ev.ctrlKey && !ev.metaKey) {
      ev.preventDefault();
      tool = "number";
      return;
    }
    // Zoom shortcuts (no modifier) — match the Figma/Sketch defaults.
    if (ev.key === "0" || ev.key.toLowerCase() === "f") {
      ev.preventDefault();
      fit();
      return;
    }
    if (ev.key === "1") {
      ev.preventDefault();
      zoomActual();
      return;
    }
    if (ev.key === "+" || ev.key === "=") {
      ev.preventDefault();
      setZoom(zoom * ZOOM_STEP);
      return;
    }
    if (ev.key === "-" || ev.key === "_") {
      ev.preventDefault();
      setZoom(zoom / ZOOM_STEP);
    }
  }

  function onKeyUp(ev: KeyboardEvent) {
    if (ev.key === " ") {
      spaceHeld = false;
    }
  }

  onMount(async () => {
    unlistenOpen = await listen<string>("editor:open", (e) => {
      void loadImage(e.payload);
    });
    // Cold-start: if the daemon stashed the source before listeners
    // mounted, pick it up here.
    try {
      const pending = await invoke<string | null>("take_editor_source", {
        label: win.label,
      });
      if (pending) await loadImage(pending);
    } catch (e) {
      console.error("take_editor_source", e);
    }
  });

  onDestroy(() => {
    clearTimeout(copyTimer);
    unlistenOpen?.();
  });
</script>

<svelte:window onkeydown={onKey} onkeyup={onKeyUp} />

<main class="root">
  <header class="titlebar" data-tauri-drag-region>
    <span class="title" data-tauri-drag-region>Editor</span>
    <WindowChrome onClose={() => dismiss()} />
  </header>

  <div class="toolbar">
    <div class="group tools">
      <button class:on={tool === "select"} onclick={() => (tool = "select")} title="Select / Move (V)" aria-label="Select">
        <MousePointer2 size={16} />
      </button>
      <button class:on={tool === "hand"} onclick={() => (tool = "hand")} title="Hand — drag to pan (H)" aria-label="Hand">
        <Hand size={16} />
      </button>
      <button class:on={tool === "crop"} onclick={() => (tool = "crop")} title="Crop — drag handles, Enter to apply" aria-label="Crop">
        <Crop size={16} />
      </button>
      <button class:on={tool === "arrow"} onclick={() => (tool = "arrow")} title="Arrow" aria-label="Arrow">
        <ArrowUpRight size={16} />
      </button>
      <button class:on={tool === "rect"} onclick={() => (tool = "rect")} title="Rectangle" aria-label="Rectangle">
        <Square size={16} />
      </button>
      <button class:on={tool === "pen"} onclick={() => (tool = "pen")} title="Pen" aria-label="Pen">
        <Pen size={16} />
      </button>
      <button class:on={tool === "highlight"} onclick={() => (tool = "highlight")} title="Highlighter" aria-label="Highlighter">
        <Highlighter size={16} />
      </button>
      <button class:on={tool === "text"} onclick={() => (tool = "text")} title="Text (T)" aria-label="Text">
        <Type size={16} />
      </button>
      <button class:on={tool === "number"} onclick={() => (tool = "number")} title="Number badge (N)" aria-label="Number">
        <Hash size={16} />
      </button>
      <button class:on={tool === "blur"} onclick={() => (tool = "blur")} title="Blur — hide sensitive content" aria-label="Blur">
        <EyeOff size={16} />
      </button>

      <span class="tools-divider" role="presentation"></span>

      <div class="bg-wrap">
        <button
          bind:this={bgTrigger}
          type="button"
          class="bg-trigger"
          class:on={bgPreset !== "none"}
          aria-expanded={bgPanelOpen}
          aria-label="Background"
          title="Background — frame the image"
          disabled={!image}
          onclick={() => (bgPanelOpen = !bgPanelOpen)}
        >
          <PaintBucket size={16} />
        </button>

        {#if bgPanelOpen}
          <div
            class="bg-panel"
            role="dialog"
            aria-label="Background options"
            use:outsideDismiss={{ trigger: bgTrigger, onDismiss: () => (bgPanelOpen = false) }}
          >
            <div class="bg-grid">
              {#each BG_PRESETS as preset (preset.id)}
                <button
                  type="button"
                  class="bg-swatch"
                  class:current={bgPreset === preset.id}
                  data-kind={preset.kind}
                  style:--swatch={preset.kind === "solid"
                    ? preset.color
                    : preset.kind === "gradient"
                      ? `linear-gradient(${preset.angle}deg, ${preset.from}, ${preset.to})`
                      : "transparent"}
                  onclick={() => setBgPreset(preset.id)}
                  title={preset.id}
                  aria-label={preset.id}
                  aria-pressed={bgPreset === preset.id}
                >
                  {#if preset.kind === "none"}<X size={12} />{/if}
                  {#if preset.kind === "blur"}<EyeOff size={12} />{/if}
                </button>
              {/each}

              <label
                class="bg-swatch bg-swatch-custom"
                class:current={bgPreset === BG_CUSTOM_ID}
                style:--swatch={bgCustomColor}
                title="Custom colour"
                aria-label="Custom colour"
              >
                <input
                  type="color"
                  value={bgCustomColor}
                  oninput={(ev) => setBgCustomColor((ev.target as HTMLInputElement).value)}
                />
                <span class="bg-swatch-plus">+</span>
              </label>
            </div>

            {#if bgPreset !== "none"}
              <div class="bg-divider"></div>

              <div class="bg-row bg-row-stacked">
                <span class="bg-row-label">Aspect</span>
                <SegmentedControl
                  value={bgAspect}
                  options={BG_ASPECTS}
                  onchange={(v) => setBgAspect(v)}
                  ariaLabel="Output aspect ratio"
                />
              </div>

              <div class="bg-row bg-row-stacked">
                <span class="bg-row-label">Padding</span>
                <SegmentedControl
                  value={bgPadding}
                  options={BG_PADDINGS.map((p) => ({ value: p, label: String(p) }))}
                  onchange={(v) => setBgPadding(v)}
                  ariaLabel="Background padding"
                />
              </div>

              <div class="bg-row bg-row-stacked">
                <span class="bg-row-label">Radius</span>
                <SegmentedControl
                  value={bgRadius}
                  options={BG_RADII.map((r) => ({ value: r, label: String(r) }))}
                  onchange={(v) => setBgRadius(v)}
                  ariaLabel="Card corner radius"
                />
              </div>

              <div class="bg-divider"></div>

              <div class="bg-row">
                <span class="bg-row-label">Shadow</span>
                <Toggle
                  checked={bgShadow}
                  onchange={(v) => toggleBgShadow(v)}
                  ariaLabel="Card drop shadow"
                />
              </div>

              {#if bgShadow}
                <div class="bg-row bg-row-slider">
                  <span class="bg-row-label">Strength</span>
                  <Slider
                    value={bgShadowStrength}
                    min={0}
                    max={100}
                    step={1}
                    oninput={previewBgShadowStrength}
                    onchange={commitBgShadowStrength}
                    ariaLabel="Shadow strength"
                  />
                </div>
              {/if}

              <button class="bg-reset" type="button" onclick={resetBackground}>
                Reset background
              </button>
            {/if}
          </div>
        {/if}
      </div>
    </div>

    <div class="group colors" data-text-control>
      <ColorPicker bind:value={color} ariaLabel="Annotation colour" />
    </div>

    {#if tool !== "select" && tool !== "hand" && tool !== "crop" && tool !== "text" && tool !== "number"}
      <StrokePicker bind:value={width} options={WIDTHS} ariaLabel="Stroke width" />
    {/if}

    {#if tool === "text"}
      <div class="group text-controls" data-text-control>
        <FontPicker bind:value={textFontFamily} options={TEXT_FONTS} ariaLabel="Font family" />
        <SizePicker bind:value={textFontSize} options={TEXT_SIZES} ariaLabel="Font size" />
      </div>
    {/if}

    {#if tool === "number"}
      <div class="group text-controls">
        <SizePicker bind:value={numberSize} options={NUMBER_SIZES} ariaLabel="Badge size" />
      </div>
    {/if}

    <span class="spacer"></span>

    {#if error}
      <span class="error" title={error}>{error}</span>
    {/if}

    <!-- History controls first (Figma / Photoshop / CleanShot
         convention): reversible actions, then destructive ops, then
         exports, then primary "Save" rightmost. -->
    <div class="group">
      <Button iconOnly onclick={undo} disabled={history.length === 0} title="Undo (Ctrl+Z)" ariaLabel="Undo">
        <Undo2 size={14} />
      </Button>
      <Button iconOnly onclick={redo} disabled={redoHistory.length === 0} title="Redo (Ctrl+Y)" ariaLabel="Redo">
        <Redo2 size={14} />
      </Button>
    </div>

    <Button onclick={openResize} disabled={!image} title="Resize image">
      <Maximize size={14} /> <span>Resize</span>
    </Button>

    <Button onclick={doCopy} disabled={busy || !sourcePath} title="Copy annotated image">
      <Copy size={14} /> <span>{copied ? "Copied" : "Copy"}</span>
    </Button>
    <Button variant="primary" onclick={doSave} disabled={busy || !sourcePath || !dirty} title="Save over original">
      <Save size={14} /> <span>Save</span>
    </Button>
  </div>

  <div
    bind:this={stage}
    class="stage"
    class:panning
    class:hand={tool === "hand" && !panning}
    class:select={tool === "select" && !panning}
    class:space-pan={spaceHeld && tool !== "hand" && !panning}
    data-tool={tool}
    role="application"
    aria-label="Annotation canvas"
    onwheel={onWheel}
    onpointerdown={onPointerDown}
    onpointermove={onPointerMove}
    onpointerup={onPointerUp}
    onpointercancel={onPointerUp}
    ondblclick={onStageDoubleClick}
  >
    {#if !sourcePath && !error}
      <p class="empty">Loading image…</p>
    {/if}
    <canvas
      bind:this={canvas}
      class:pixelated={zoom >= 1.5}
      style:transform="translate({panX}px, {panY}px) scale({zoom})"
    ></canvas>

    {#if image && cropRect && tool === "crop"}
      <!-- Crop overlay: matches the canvas pose (same translate + scale)
           so a rect at (10,10) in image-pixel space lands on the same
           visual pixel as the underlying canvas. Handles invert the
           scale via --inverse-zoom so they stay a constant 12px on
           screen regardless of zoom level. -->
      <div
        class="crop-frame"
        role="presentation"
        style:transform="translate({panX}px, {panY}px) scale({zoom})"
        style:width="{image.width}px"
        style:height="{image.height}px"
        style:--inverse-zoom={1 / zoom}
        onpointermove={onCropPointerMove}
        onpointerup={onCropPointerUp}
        onpointercancel={onCropPointerUp}
      >
        <div
          class="crop-rect"
          style:left="{cropRect.x}px"
          style:top="{cropRect.y}px"
          style:width="{cropRect.w}px"
          style:height="{cropRect.h}px"
          onpointerdown={onCropRectDown}
          role="presentation"
        >
          <button class="crop-handle nw" onpointerdown={(e) => onHandleDown(e, "nw")} aria-label="Resize NW"></button>
          <button class="crop-handle n" onpointerdown={(e) => onHandleDown(e, "n")} aria-label="Resize N"></button>
          <button class="crop-handle ne" onpointerdown={(e) => onHandleDown(e, "ne")} aria-label="Resize NE"></button>
          <button class="crop-handle e" onpointerdown={(e) => onHandleDown(e, "e")} aria-label="Resize E"></button>
          <button class="crop-handle se" onpointerdown={(e) => onHandleDown(e, "se")} aria-label="Resize SE"></button>
          <button class="crop-handle s" onpointerdown={(e) => onHandleDown(e, "s")} aria-label="Resize S"></button>
          <button class="crop-handle sw" onpointerdown={(e) => onHandleDown(e, "sw")} aria-label="Resize SW"></button>
          <button class="crop-handle w" onpointerdown={(e) => onHandleDown(e, "w")} aria-label="Resize W"></button>
        </div>
      </div>

      <div class="crop-actions">
        <span class="crop-dims">{Math.round(cropRect.w)} × {Math.round(cropRect.h)}</span>
        <Button size="sm" onclick={cancelCrop} title="Cancel (Esc)">
          <X size={14} /> <span>Cancel</span>
        </Button>
        <Button size="sm" variant="primary" onclick={applyCrop} title="Apply crop (Enter)">
          <Check size={14} /> <span>Apply</span>
        </Button>
      </div>
    {/if}

    {#if textDraft && image}
      <!-- Text editing overlay: same transform as the canvas so the
           textarea lives in image-pixel space. `field-sizing: content`
           auto-grows the box with the typed content (Chromium 123+,
           WebView2 has it). Blur OR Esc commits via `commitText`. -->
      <div
        class="text-overlay"
        role="presentation"
        style:transform="translate({panX}px, {panY}px) scale({zoom})"
        style:width="{image.width}px"
        style:height="{image.height}px"
      >
        <textarea
          bind:this={textInput}
          bind:value={textDraft.text}
          onkeydown={onTextKeyDown}
          onblur={onTextBlur}
          style:left="{textDraft.x}px"
          style:top="{textDraft.y}px"
          style:color={textDraft.color}
          style:font="{textDraft.fontSize}px {textDraft.fontFamily}"
          spellcheck="false"
          placeholder="Type…"
        ></textarea>
      </div>
    {/if}

    {#if image && tool !== "crop"}
      <div class="zoom-bar">
        <Button iconOnly size="sm" onclick={() => setZoom(zoom / ZOOM_STEP)} title="Zoom out (−)" ariaLabel="Zoom out">
          <Minus size={14} />
        </Button>
        <button class="zoom-level" onclick={zoomActual} title="Reset to 100% (1) · F or 0 = fit" aria-label="Reset to 100%">
          {Math.round(zoom * 100)}%
        </button>
        <Button iconOnly size="sm" onclick={() => setZoom(zoom * ZOOM_STEP)} title="Zoom in (+)" ariaLabel="Zoom in">
          <Plus size={14} />
        </Button>
      </div>
    {/if}
  </div>

  {#if resizeOpen && image}
    <div
      class="modal-scrim"
      onpointerdown={() => (resizeOpen = false)}
      role="presentation"
    >
      <div class="resize-modal" tabindex="-1" onpointerdown={(e) => e.stopPropagation()} role="dialog" aria-label="Resize image">
        <header>
          <h2>Resize image</h2>
          <span class="resize-current">Current: {image.width} × {image.height}</span>
        </header>

        <div class="resize-presets">
          {#each [0.25, 0.5, 0.75, 1, 1.5, 2] as preset (preset)}
            <button
              class="preset"
              class:active={isResizePresetActive(preset)}
              onclick={() => applyResizePreset(preset)}
            >
              {preset * 100}%
            </button>
          {/each}
        </div>

        <div class="resize-inputs">
          <label>
            <span>Width</span>
            <input
              type="number"
              min="1"
              value={resizeW}
              oninput={(e) => onResizeWChanged((e.target as HTMLInputElement).value)}
            />
          </label>
          <label>
            <span>Height</span>
            <input
              type="number"
              min="1"
              value={resizeH}
              oninput={(e) => onResizeHChanged((e.target as HTMLInputElement).value)}
            />
          </label>
          <div class="resize-lock">
            <Toggle bind:checked={resizeLockAspect} ariaLabel="Lock aspect ratio" />
            <span>Lock aspect ratio</span>
          </div>
        </div>

        <footer>
          <Button size="sm" onclick={() => (resizeOpen = false)}>
            Cancel
          </Button>
          <Button size="sm" variant="primary" onclick={applyResize}>
            Apply
          </Button>
        </footer>
      </div>
    </div>
  {/if}
</main>

<style>
  .root {
    display: flex;
    flex-direction: column;
    height: 100vh;
    overflow: hidden;
  }

  .titlebar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    height: var(--titlebar-h);
    padding-left: var(--space-4);
    border-bottom: 1px solid var(--color-border-subtle);
    flex-shrink: 0;
  }
  .title {
    font-size: var(--text-md);
    font-weight: 600;
    color: var(--color-fg);
  }

  .toolbar {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 8px 12px;
    border-bottom: 1px solid var(--color-border-subtle);
    background: var(--color-surface-1);
    flex-shrink: 0;
  }

  .group {
    display: inline-flex;
    align-items: center;
    gap: 4px;
  }

  /* Direct-child selector — the Background popover is mounted INSIDE
   * `.tools` (via `.bg-wrap`), so a descendant selector would also
   * size every `.bg-swatch` and `.bg-reset` button to 30×28 and
   * shatter the swatch grid. Tools are flat children of `.tools`;
   * everything deeper has its own styles. */
  .tools > button {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 30px;
    height: 28px;
    border: none;
    background: transparent;
    color: var(--color-fg-muted);
    border-radius: var(--radius-sm);
    cursor: pointer;
    padding: 0;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
  }
  .tools > button:hover {
    background: var(--color-surface-2);
    color: var(--color-fg);
  }
  .tools > button.on {
    background: var(--color-accent-bg-subtle);
    color: var(--color-accent-fg);
  }

  .spacer {
    flex: 1;
  }

  .error {
    font-size: var(--text-xs);
    color: var(--color-danger-hover);
    max-width: 280px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }


  .stage {
    flex: 1;
    position: relative;
    overflow: hidden;
    cursor: crosshair;
    /* Light checkerboard hints at transparency / canvas edges so the
     * user knows where the image ends when zoomed out. */
    background:
      repeating-conic-gradient(
        var(--color-surface-0) 0% 25%,
        var(--color-surface-1) 0% 50%
      )
      50% / 16px 16px;
    /* The toolbar floats over a small zoom bar; nothing inside scrolls,
     * pan is purely transform-driven. */
  }
  .stage.hand,
  .stage.space-pan {
    cursor: grab;
  }
  .stage.select {
    cursor: default;
  }
  .stage.panning {
    cursor: grabbing;
  }

  .empty {
    position: absolute;
    inset: 0;
    margin: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--color-fg-muted);
    font-size: var(--text-sm);
    pointer-events: none;
  }

  canvas {
    position: absolute;
    top: 0;
    left: 0;
    transform-origin: top left;
    background: white;
    box-shadow: var(--shadow-lg);
    /* Don't intercept pointer events directly — the parent stage owns
     * pan/draw routing so panning works even when the mouse leaves the
     * image. */
    pointer-events: none;
  }
  canvas.pixelated {
    image-rendering: pixelated;
  }

  .zoom-bar {
    position: absolute;
    bottom: 12px;
    right: 12px;
    display: inline-flex;
    align-items: center;
    gap: 4px;
    padding: 3px;
    /* Docked toolbar — tonal elevation (lift via `surface-2` + a
     * 1 px hairline) rather than a drop shadow. The previous
     * `var(--shadow-md)` faked depth that the toolbar doesn't
     * actually have: it lives inside the canvas, never overlaps the
     * canvas edge, and the shadow blurred onto the checkerboard
     * background. CLAUDE.md / 2026 Fluent guidance is to keep
     * shadows for true floaters (modals, popovers, toasts) only. */
    background: var(--color-surface-2);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius-md);
  }
  .zoom-level {
    min-width: 52px;
    height: 26px;
    padding: 0 8px;
    border: none;
    background: transparent;
    color: var(--color-fg);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    font-variant-numeric: tabular-nums;
    cursor: pointer;
    border-radius: var(--radius-xs);
    transition: background var(--duration-quick) var(--ease-in-out-soft);
  }
  .zoom-level:hover {
    background: var(--color-surface-2);
  }

  /* ─── CROP OVERLAY ─────────────────────────────────────────────
   * The frame matches the canvas pose (same transform), so anything
   * inside it lives in image-pixel coords. Handles undo the scale via
   * the inverse-zoom CSS var so they remain 12px regardless of zoom. */
  .crop-frame {
    position: absolute;
    top: 0;
    left: 0;
    transform-origin: top left;
    overflow: visible;
    pointer-events: none;
  }
  /* Figma-style crop: white dashed outline (universal — works on
   * any screenshot colour, including the app's own accent colour)
   * with brand-coloured handles. Circle corners + pill mid-edges
   * tell the user at a glance which axes a given handle controls. */
  .crop-rect {
    position: absolute;
    box-sizing: border-box;
    border: 1.5px dashed rgba(255, 255, 255, 0.95);
    box-shadow: 0 0 0 99999px rgba(0, 0, 0, 0.55);
    cursor: move;
    pointer-events: auto;
  }

  .crop-handle {
    position: absolute;
    padding: 0;
    background: #fff;
    border: 1.5px solid var(--color-accent);
    box-shadow: 0 1px 3px rgba(0, 0, 0, 0.35);
    cursor: nwse-resize;
    transform: scale(var(--inverse-zoom, 1));
    transform-origin: center;
    pointer-events: auto;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      box-shadow var(--duration-quick) var(--ease-in-out-soft);
  }
  .crop-handle:hover {
    background: var(--color-accent);
    box-shadow:
      0 1px 4px rgba(99, 102, 241, 0.5),
      0 0 0 2px rgba(255, 255, 255, 0.65);
  }

  /* Corner handles — round dots, drag two edges at once. */
  .crop-handle.nw,
  .crop-handle.ne,
  .crop-handle.se,
  .crop-handle.sw {
    width: 12px;
    height: 12px;
    margin: -6px;
    border-radius: 50%;
  }

  /* Mid-edge handles — pill / capsule. Horizontal pill on top/bottom,
   * vertical pill on left/right. Shape itself signals "this edge
   * only" vs corners which drag two. */
  .crop-handle.n,
  .crop-handle.s {
    width: 22px;
    height: 8px;
    margin: -4px -11px;
    border-radius: 999px;
  }
  .crop-handle.e,
  .crop-handle.w {
    width: 8px;
    height: 22px;
    margin: -11px -4px;
    border-radius: 999px;
  }
  /* Position each handle on a corner / midpoint of the rect. The
   * negative margins above pull each one half-its-size out, so the
   * stated `top:0/50%/100%` lands the handle centred on the edge. */
  .crop-handle.nw { top: 0; left: 0; cursor: nwse-resize; }
  .crop-handle.n  { top: 0; left: 50%; cursor: ns-resize; }
  .crop-handle.ne { top: 0; left: 100%; cursor: nesw-resize; }
  .crop-handle.e  { top: 50%; left: 100%; cursor: ew-resize; }
  .crop-handle.se { top: 100%; left: 100%; cursor: nwse-resize; }
  .crop-handle.s  { top: 100%; left: 50%; cursor: ns-resize; }
  .crop-handle.sw { top: 100%; left: 0; cursor: nesw-resize; }
  .crop-handle.w  { top: 50%; left: 0; cursor: ew-resize; }

  .crop-actions {
    position: absolute;
    bottom: 12px;
    left: 50%;
    transform: translateX(-50%);
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 4px 4px 4px 12px;
    /* See `.zoom-bar` — tonal elevation (surface-2 + hairline) for
     * a docked toolbar, no shadow. */
    background: var(--color-surface-2);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius-md);
  }
  .crop-dims {
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    font-variant-numeric: tabular-nums;
    color: var(--color-fg-muted);
    margin-right: 4px;
  }

  /* Tool-contextual control group (text + number). The pickers
   * inside (FontPicker / SizePicker) own their own visuals. */
  .text-controls {
    gap: 6px;
  }

  /* ─── TEXT EDITING OVERLAY ─────────────────────────────────────
   * The overlay lives in image-pixel space (same transform as the
   * canvas), so positioning the textarea by `left/top` in those
   * coords maps to the same visual pixel the user clicked. */
  .text-overlay {
    position: absolute;
    top: 0;
    left: 0;
    transform-origin: top left;
    pointer-events: none;
  }
  .text-overlay textarea {
    position: absolute;
    pointer-events: auto;
    /* `field-sizing: content` auto-sizes to typed content; lands in
     * Chromium 123 → WebView2 has it. */
    field-sizing: content;
    min-width: 80px;
    min-height: 1em;
    margin: 0;
    padding: 0;
    border: none;
    background: transparent;
    /* Soft dashed outline so the user sees the edit region against
     * any image; matches the in-editor accents elsewhere. */
    outline: 1.5px dashed rgba(255, 255, 255, 0.75);
    outline-offset: 3px;
    resize: none;
    line-height: 1.25;
    /* Same drop shadow we bake onto the canvas — no visual jump on
     * commit. */
    text-shadow: 0 1px 2px rgba(0, 0, 0, 0.55);
    overflow: hidden;
    white-space: pre-wrap;
    word-break: break-word;
  }
  .text-overlay textarea::placeholder {
    color: currentColor;
    opacity: 0.45;
  }

  /* ─── RESIZE MODAL ─────────────────────────────────────────────── */
  .modal-scrim {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.55);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 10;
  }
  .resize-modal {
    width: 340px;
    background: var(--color-surface-0);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-lg);
    padding: 16px 18px;
    box-sizing: border-box;
  }
  .resize-modal header {
    margin-bottom: 12px;
  }
  .resize-modal h2 {
    margin: 0 0 4px;
    font-size: var(--text-md);
    font-weight: 600;
  }
  .resize-current {
    font-size: var(--text-xs);
    color: var(--color-fg-muted);
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
  }
  .resize-presets {
    display: grid;
    grid-template-columns: repeat(6, 1fr);
    gap: 4px;
    margin-bottom: 14px;
  }
  .preset {
    height: 26px;
    border: none;
    background: var(--color-surface-1);
    color: var(--color-fg);
    border-radius: var(--radius-xs);
    font-size: var(--text-xs);
    cursor: pointer;
    transition: background var(--duration-quick) var(--ease-in-out-soft);
  }
  .preset:hover {
    background: var(--color-surface-2);
  }
  .preset.active {
    background: var(--color-accent-bg-subtle);
    color: var(--color-accent-fg);
  }
  .preset.active:hover {
    background: var(--color-accent-bg-strong);
  }
  .resize-inputs {
    display: flex;
    flex-direction: column;
    gap: 8px;
    margin-bottom: 16px;
  }
  .resize-inputs label {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: var(--text-sm);
    color: var(--color-fg-muted);
  }
  .resize-inputs label span {
    width: 60px;
  }
  .resize-inputs input[type="number"] {
    flex: 1;
    height: 26px;
    padding: 0 8px;
    border: 1px solid var(--color-border-subtle);
    background: var(--color-surface-1);
    color: var(--color-fg);
    border-radius: var(--radius-xs);
    font-size: var(--text-sm);
    font-family: var(--font-mono);
    font-variant-numeric: tabular-nums;
  }
  .resize-inputs input[type="number"]:focus {
    outline: none;
    border-color: var(--color-border-accent);
  }
  .resize-lock {
    display: flex;
    align-items: center;
    gap: 10px;
    font-size: var(--text-sm);
    color: var(--color-fg-muted);
    user-select: none;
    margin-top: 2px;
  }
  .resize-modal footer {
    display: flex;
    justify-content: flex-end;
    gap: 6px;
  }

  /* ─── BACKGROUND POPOVER ────────────────────────────────────────
   * Toolbar trigger mirrors the .tools button "on" treatment so the
   * active state reads identically to the tool buttons next to it.
   * Panel hangs below the trigger, left-aligned to its left edge so
   * it opens into the canvas (the trigger sits mid-toolbar). */
  .bg-wrap {
    position: relative;
    display: inline-flex;
  }
  /* Background trigger lives in the `.tools` group as an icon-only
   * peer of the drawing tools (Layers icon is universal enough that
   * the toolbar caption read worse than the symbol did). Same 30×28
   * cell + accent-on style as every other tool button. */
  .bg-trigger {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 30px;
    height: 28px;
    border: none;
    background: transparent;
    color: var(--color-fg-muted);
    border-radius: var(--radius-sm);
    cursor: pointer;
    padding: 0;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
  }
  .bg-trigger:hover:not(:disabled) {
    background: var(--color-surface-2);
    color: var(--color-fg);
  }
  .bg-trigger:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
  .bg-trigger.on {
    background: var(--color-accent-bg-subtle);
    color: var(--color-accent-fg);
  }

  /* Faint vertical rule between the drawing tools and the Background
   * trigger — categorical separation ("draw on the image" vs.
   * "style the image") without making the toolbar feel sectioned. */
  .tools-divider {
    width: 1px;
    height: 18px;
    background: var(--color-border-subtle);
    margin: 0 4px;
    flex-shrink: 0;
  }

  .bg-panel {
    position: absolute;
    top: calc(100% + 8px);
    left: 0;
    z-index: 30;
    /* 290 px gives the 5-chip segmented rows (Padding "0/16/32/64/96")
     * ~25 px of slack each instead of the 1-2 px they had at 264 — that
     * gap was reading as "cramped numbers" in review. */
    width: 290px;
    /* Surface-0 + shadow = the elevated-layer convention; the inner
     * SegmentedControl sits on surface-1 and now reads as a distinct
     * tablet around the chips (at surface-1 popover, both shared the
     * same fill and the pill outline disappeared). */
    background: var(--color-surface-0);
    border: 1px solid var(--color-border-subtle);
    border-radius: var(--radius-md);
    box-shadow: var(--shadow-lg);
    padding: 12px;
    animation: bg-pop var(--duration-quick) var(--ease-out-snappy);
  }

  /* 5-column swatch grid: 8 curated presets + a custom-color tile.
   * 5 columns at 264-px popover width keeps the tiles compact (~42 px)
   * so a 9-tile grid doesn't feel oversized — picks are still
   * fully visual, just denser. Each tile shows its actual fill. */
  .bg-grid {
    display: grid;
    grid-template-columns: repeat(5, 1fr);
    gap: 6px;
  }
  .bg-swatch {
    aspect-ratio: 1;
    border-radius: var(--radius-sm);
    background: var(--swatch);
    border: 1.5px solid rgba(255, 255, 255, 0.14);
    cursor: pointer;
    padding: 0;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    color: var(--color-fg-muted);
    position: relative;
    transition:
      transform var(--duration-quick) var(--ease-in-out-soft),
      box-shadow var(--duration-quick) var(--ease-in-out-soft);
  }
  .bg-swatch:hover {
    transform: scale(1.1);
  }
  .bg-swatch.current {
    /* First ring fills the gap to the surface; must match `.bg-panel`'s
     * background so the accent ring reads as a true halo. */
    box-shadow:
      0 0 0 2px var(--color-surface-0),
      0 0 0 4px var(--color-accent);
  }
  /* None + Blur tiles get a hatched / overlay treatment so they read
   * as "not a solid colour" at a glance. */
  .bg-swatch[data-kind="none"] {
    background:
      repeating-conic-gradient(
        var(--color-surface-0) 0% 25%,
        var(--color-surface-2) 0% 50%
      )
      50% / 6px 6px;
  }
  .bg-swatch[data-kind="blur"] {
    background: linear-gradient(135deg, #475569, #94a3b8, #475569);
    color: #fff;
  }
  /* Custom-colour tile: <label> hosting an invisible native <input
   * type="color">. The "+" sits centred with a soft shadow so it
   * reads on any picked colour without re-tinting per fill. */
  .bg-swatch-custom {
    overflow: hidden;
    color: rgba(255, 255, 255, 0.92);
  }
  .bg-swatch-custom input[type="color"] {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
    opacity: 0;
    cursor: pointer;
    padding: 0;
    border: none;
  }
  .bg-swatch-plus {
    font-size: var(--text-lg);
    font-weight: 500;
    line-height: 1;
    text-shadow: 0 1px 2px rgba(0, 0, 0, 0.45);
    pointer-events: none;
  }

  .bg-divider {
    height: 1px;
    background: var(--color-border-subtle);
    margin: 12px -12px;
  }

  .bg-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    margin-bottom: 10px;
  }
  .bg-row:last-of-type {
    margin-bottom: 0;
  }
  /* Stacked variant for rows whose control is too wide to share the
   * line with the label (e.g. the 5-chip aspect-ratio segmented). */
  .bg-row-stacked {
    flex-direction: column;
    align-items: stretch;
    gap: 6px;
  }
  /* Stretch the segmented full-width and equalize chip widths so the
   * stacked rows read as a clean 5-column band. Without this the
   * `.chips` container stays inline-flex (content width), leaving a
   * dead gap to the right of short rows like Padding "0/16/32/64/96"
   * and making chip widths look arbitrary (each sized to its own
   * label length). */
  .bg-row-stacked :global(.chips) {
    display: flex;
    width: 100%;
  }
  .bg-row-stacked :global(.chip) {
    flex: 1;
    min-width: 0;
    padding: 0;
  }
  /* Slider row: the label keeps its column on the left but the
   * slider fills the rest of the line so the drag distance is the
   * full width of the panel — small sliders feel jittery, full-
   * width ones feel intentional. */
  .bg-row-slider {
    gap: 12px;
  }
  .bg-row-slider :global(.slider) {
    flex: 1;
    min-width: 0;
  }
  .bg-row-label {
    font-size: var(--text-sm);
    color: var(--color-fg-muted);
  }

  .bg-reset {
    margin-top: 12px;
    width: 100%;
    height: 26px;
    border: none;
    background: transparent;
    color: var(--color-fg-muted);
    font-family: inherit;
    font-size: var(--text-xs);
    border-radius: var(--radius-xs);
    cursor: pointer;
    transition:
      background var(--duration-quick) var(--ease-in-out-soft),
      color var(--duration-quick) var(--ease-in-out-soft);
  }
  .bg-reset:hover {
    background: var(--color-surface-2);
    color: var(--color-fg);
  }

  @keyframes bg-pop {
    from {
      opacity: 0;
      transform: translateY(-4px);
    }
    to {
      opacity: 1;
      transform: translateY(0);
    }
  }
</style>
