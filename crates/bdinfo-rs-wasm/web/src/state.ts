// The page state the feature modules share, in one object, plus the DOM
// plumbing they all reach for. One object rather than a module of exported
// `let`s: an imported binding is read-only at the import site, so every module
// that writes one would otherwise need a setter beside it.
//
// This module imports no other page module at run time, and that is load-order
// safety, not tidiness: the feature modules import each other in cycles (the
// table draws the panes, the panes write the table's cells), and its state and
// its `el` both run while those modules are still evaluating. A module in a
// cycle is entered mid-evaluation, so anything it reached for here could be an
// uninitialized `const` — which is why the stored settings are read here rather
// than by a call back into the settings module.
import type { BdmvFile, Disc, MeasuredPlaylist, Playlist } from "./analyze.js";
import type { PaneRow } from "./panes.js";
import type { PlaylistRow, Sort } from "./table.js";

/** The picked disc — a `webkitdirectory` BDMV folder, or a single `.iso`. */
export type Source =
  | { kind: "folder"; files: BdmvFile[]; label: string }
  | { kind: "iso"; file: File; label: string };

/** The dialog's settings, mirroring the desktop app's browser-relevant ones. */
export interface Settings {
  /** The page palette: follow the OS (`"auto"`), or force one side. */
  theme: "auto" | "light" | "dark";
  showShortPlaylists: boolean;
  showLoopingPlaylists: boolean;
  /** What "short" means, in whole seconds — the one setting that re-runs `inspect`. */
  shortPlaylistSeconds: number;
  /** Size cells as `83.62 GB` instead of thousands-grouped bytes. */
  humanReadableSizes: boolean;
  /** Append ` [NN Chapters]` to a playlist name when the count exceeds one. */
  displayChapterCount: boolean;
  /** Render the report's `STREAM DIAGNOSTICS:` section. */
  reportStreamDiagnostics: boolean;
  /** Render the report's `QUICK SUMMARY:` section. */
  reportQuickSummary: boolean;
  /**
   * Keep what a scan measured of a stream file whose read failed partway. The
   * one setting that reaches only the NEXT scan: it changes what a scan
   * collects, so flipping it re-renders nothing the page already holds.
   */
  keepPartialScans: boolean;
}

/**
 * Where the settings persist between visits. index.html's inline theme-boot
 * script reads the same key as a literal — an inline script can import
 * nothing — so renaming it means changing both (and regenerating the boot
 * script's CSP hash).
 */
const SETTINGS_KEY = "bdinfo-rs.settings";

/** The default short-playlist threshold, matching the CLI flag's default. */
const DEFAULT_SHORT_SECONDS = 20;

/**
 * The threshold domain's ceiling (0 = the short rule off), mirroring the
 * `min`/`max` attributes on the `#opt-short-seconds` input and, through them,
 * the library's shared threshold contract. A stored or typed value outside
 * the domain reverts here rather than reaching the module, which throws on it.
 */
export const MAX_SHORT_SECONDS = 86_400;

/**
 * Reads the stored settings, defaulting to the standard filtered table with
 * every report section on. Sizes default to human-readable — the desktop app
 * ships thousands-grouped bytes instead, and this page has always shown
 * human-readable sizes, so it keeps its own default. `localStorage` throws
 * outright when the page is sandboxed or site data is blocked, so every access
 * is guarded — the demo then runs with the defaults and the choice simply does
 * not survive a reload.
 */
function loadSettings(): Settings {
  let stored: Partial<Settings> = {};
  try {
    const raw = window.localStorage.getItem(SETTINGS_KEY);
    if (raw !== null) {
      stored = JSON.parse(raw);
    }
  } catch {
    stored = {};
  }
  const seconds = stored.shortPlaylistSeconds;
  const theme = stored.theme;
  return {
    theme: theme === "light" || theme === "dark" ? theme : "auto",
    showShortPlaylists: stored.showShortPlaylists === true,
    showLoopingPlaylists: stored.showLoopingPlaylists === true,
    shortPlaylistSeconds:
      typeof seconds === "number" &&
      Number.isInteger(seconds) &&
      seconds >= 0 &&
      seconds <= MAX_SHORT_SECONDS
        ? seconds
        : DEFAULT_SHORT_SECONDS,
    humanReadableSizes: stored.humanReadableSizes !== false,
    displayChapterCount: stored.displayChapterCount !== false,
    reportStreamDiagnostics: stored.reportStreamDiagnostics !== false,
    reportQuickSummary: stored.reportQuickSummary !== false,
    keepPartialScans: stored.keepPartialScans !== false,
  };
}

export function saveSettings(): void {
  try {
    window.localStorage.setItem(SETTINGS_KEY, JSON.stringify(state.settings));
  } catch {
    return;
  }
}

export const state = {
  source: null as Source | null,
  reportText: "",
  discName: "disc",
  /** Aborts the in-progress measured scan; null when no scan is running. */
  scanController: null as AbortController | null,
  /**
   * The ONE disc the page holds — never two at a time, never fields blended
   * between an old disc and a new one. The structural `inspect` on pick fills it;
   * a finished `scan` replaces it with the measured twin (same playlists, same
   * classification, measured values filled in); a threshold change replaces it
   * with a re-inspected one and discards everything measured, because a measured
   * disc classified under a stale threshold would disagree with the table.
   */
  disc: null as Disc | null,
  /** The short-playlist threshold `disc` was classified under, in seconds. */
  discThreshold: 0,
  /**
   * The report-section switches `reportText` was rendered with; null while no
   * report is held. Re-applying the same pair is a no-op — no render request.
   */
  renderedWith: null as { streamDiagnostics: boolean; quickSummary: boolean } | null,
  /**
   * Stamps every WebAssembly request so a slow completion cannot overwrite the
   * state a faster later action produced: each request captures the counter,
   * every newer request bumps it, and a completion whose stamp is stale is
   * dropped on the floor.
   */
  generation: 0,
  /** `disc.playlists`, kept unwrapped for the table and pane renderers. */
  playlists: [] as Playlist[],
  /**
   * The table rows over `playlists`: a scan returns every playlist, tagged with
   * the rules that withhold it, and the settings are applied to these rows in the
   * page. So toggling a setting redraws the table instantly instead of rescanning.
   */
  allRows: [] as PlaylistRow[],
  /** The rows as last drawn — filter, numbering and sort applied, top to bottom. */
  displayed: [] as PlaylistRow[],
  /** The playlists the user has unticked, by name — persists across a redraw. */
  unchecked: new Set<string>(),
  /**
   * What the RUNNING scan has measured so far, by playlist name: the overlay the
   * table and the panes prefer over the held disc while a scan is in flight, and
   * the only place a mid-scan number lives.
   *
   * It is an overlay rather than a patch of the held disc for the reason the
   * desktop app keeps one too: the disc the page holds is what the report, the
   * sort keys and the row set are derived from, so writing half-measured numbers
   * into it would make those disagree with the report beside them.
   *
   * It outlives the scan that filled it: a cancelled or failed scan leaves its
   * partial numbers on the cells, and the NEXT scan clears them as it starts.
   * A finished scan clears it too, but by adopting the measured disc — whose
   * numbers are the final form of the very cells the overlay was raising.
   */
  live: new Map<string, MeasuredPlaylist>(),
  /** The playlist whose details the panes show; null before the first draw. */
  activeName: null as string | null,
  /**
   * The highlighted detail-pane row — Ctrl+C's copy target while it stands, and
   * null when no pane row has been clicked. Cleared whenever the panes are
   * redrawn: a row index means nothing once the rows under it change.
   */
  paneRow: null as PaneRow | null,
  /**
   * Whether the transient reveal is on: the playlists the filters withhold sit
   * in the table as ordinary rows. Session-only and deliberately outside
   * {@link Settings} — it is never stored, and any settings change drops it,
   * which is what keeps it from reading as a fourth filter switch.
   */
  revealing: false,
  /**
   * Whether the shown report is the PRE-SCAN structural render rather than a
   * measured scan's. It is re-rendered whenever the selection or the settings
   * behind it move, so what is on screen always describes the current
   * selection; a measured scan replaces it with the report it produced.
   */
  previewing: false,
  /**
   * The playlist selection the shown pre-scan report was rendered over, by
   * name; null while none is shown. It is what tells a selection change (a
   * re-render) from a redraw that left the scan set exactly as it was — a
   * display setting, say, which the report is not derived from at all.
   */
  previewOrder: null as string[] | null,
  /** The active sort; null draws the CLI's table order (by `position`). */
  sort: null as Sort | null,
  settings: loadSettings(),
};

export function el<T extends HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  if (node === null) {
    throw new Error(`missing #${id}`);
  }
  return node as T;
}

export const errorBox = el("error");
const errorText = el("error-text");

export function show(node: HTMLElement): void {
  node.hidden = false;
}
export function hide(node: HTMLElement): void {
  node.hidden = true;
}
export function showError(message: string): void {
  errorText.textContent = message;
  show(errorBox);
}
export function errMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
