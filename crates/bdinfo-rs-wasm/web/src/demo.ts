// The vanilla (no-framework) demo driving the package's public API: pick or drop
// a BDMV folder, inspect it (structural scan), let the user select some
// playlists, run the measured scan in a Worker, and show the rendered report
// with copy + download. No upload — everything stays in the browser. The
// playlist table is the master of a master-detail flow: the active (highlighted)
// row populates the two detail panes below it — the playlist's stream files and
// its codecs — the same lower panes the bdinfo-rs desktop app and the classic
// BDInfo window show. The settings dialog mirrors the desktop app's: the two
// playlist-filter opt-outs, the size format and the chapter-count suffix are
// pure re-projections of the model the page already holds; only the
// short-playlist threshold (a fresh `inspect`) and the two report sections (a
// `renderReport` over the held disc) reach the WebAssembly module, and nothing
// in the dialog ever repeats a measured scan. The retention switch sends
// nothing at all: it travels with the next measured scan the user starts.
import {
  type BdmvFile,
  type Clip,
  type Disc,
  type HiddenRule,
  inspect,
  type Playlist,
  renderReport,
  reportFileName,
  type Stream,
  scan,
} from "./analyze.js";
import { sizeCell as formatSize } from "./format.js";

/**
 * One selection-table row — the CLI columns this page draws, distilled from a
 * {@link Playlist}. The disc model carries the numbers; the table needs a
 * formatted length cell and the two derived flags, so it computes them once
 * here rather than in every cell.
 */
interface PlaylistRow {
  position: number;
  group: number;
  name: string;
  /** `hh:mm:ss`, truncated to the tick exactly as the CLI table truncates it. */
  length: string;
  /** The raw ticks behind `length` — the Length column's sort key. */
  lengthTicks: number;
  /** Interleaved `*.ssif` size, else `*.m2ts` size, else null (the `—` cell). */
  estimatedBytes: number | null;
  /** Whether the playlist hides any stream (the CLI's `(*)` note). */
  hasHidden: boolean;
  hiddenBy: HiddenRule[];
  /** Chapter count behind the optional ` [NN Chapters]` name suffix. */
  chapterCount: number;
}

/** The ticks in one second — the unit `totalLengthTicks` counts. */
const TICKS_PER_SECOND = 10_000_000;

/** A row per playlist, in the table order `position` records. */
function playlistRows(playlists: Playlist[]): PlaylistRow[] {
  return playlists
    .map((playlist) => ({
      position: playlist.position,
      group: playlist.group,
      name: playlist.name,
      length: tableLength(playlist.totalLengthTicks),
      lengthTicks: playlist.totalLengthTicks,
      estimatedBytes: playlist.interleavedFileSizeBytes || playlist.fileSizeBytes || null,
      hasHidden: playlist.streams.some((stream) => stream.isHidden),
      hiddenBy: playlist.hiddenBy,
      chapterCount: playlist.chapterCount,
    }))
    .sort((a, b) => a.position - b.position);
}

/** `hh:mm:ss` from playlist ticks, truncated like the CLI table (hours wrap at 24). */
function tableLength(ticks: number): string {
  const total = Math.max(0, Math.trunc(ticks / TICKS_PER_SECOND));
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${pad(Math.trunc(total / 3600) % 24)}:${pad(Math.trunc(total / 60) % 60)}:${pad(total % 60)}`;
}

function el<T extends HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  if (node === null) {
    throw new Error(`missing #${id}`);
  }
  return node as T;
}

const dropzone = el<HTMLLabelElement>("dropzone");
const picker = el<HTMLInputElement>("picker");
const isoPicker = el<HTMLInputElement>("iso-picker");
const pickedBox = el("picked");
const pickedName = el("picked-name");
const pickedCount = el("picked-count");
const playlistsCard = el("playlists-card");
const discLabel = el("disc-label");
const playlistBody = el<HTMLTableSectionElement>("playlist-body");
const panesBox = el("detail-panes");
const paneLabel = el("pane-playlist");
const filesBody = el<HTMLTableSectionElement>("files-body");
const codecsBody = el<HTMLTableSectionElement>("codecs-body");
const selectAllBtn = el<HTMLButtonElement>("select-all");
const clearBtn = el<HTMLButtonElement>("clear-sel");
const selCount = el("sel-count");
const scanBtn = el<HTMLButtonElement>("scan-btn");
const progressCard = el("progress-card");
const bar = el<HTMLProgressElement>("bar");
const pctLabel = el("pct");
const progressText = el("progress-text");
const cancelBtn = el<HTMLButtonElement>("cancel-btn");
const reportCard = el("report-card");
const reportPre = el("report");
const copyBtn = el<HTMLButtonElement>("copy-btn");
const copyLabel = el("copy-label");
const downloadBtn = el<HTMLButtonElement>("download-btn");
const errorBox = el("error");
const errorText = el("error-text");
const mainEl = el("main");
const listingBox = el("listing");
const settingsBtn = el<HTMLButtonElement>("settings-btn");
const settingsDialog = el<HTMLDialogElement>("settings-dialog");
const settingsClose = el<HTMLButtonElement>("settings-close");
const optShort = el<HTMLInputElement>("opt-short");
const optShortSeconds = el<HTMLInputElement>("opt-short-seconds");
const discardNote = el("discard-note");
const optLooping = el<HTMLInputElement>("opt-looping");
const optHumanSizes = el<HTMLInputElement>("opt-human-sizes");
const optChapters = el<HTMLInputElement>("opt-chapters");
const optDiagnostics = el<HTMLInputElement>("opt-diagnostics");
const optSummary = el<HTMLInputElement>("opt-summary");
const optKeepPartial = el<HTMLInputElement>("opt-keep-partial");
const hiddenHint = el("hidden-hint");
const encryptedNote = el("encrypted-note");

/** The picked disc — a `webkitdirectory` BDMV folder, or a single `.iso`. */
type Source =
  | { kind: "folder"; files: BdmvFile[]; label: string }
  | { kind: "iso"; file: File; label: string };

let source: Source | null = null;
let reportText = "";
let discName = "disc";
/** Aborts the in-progress measured scan; null when no scan is running. */
let scanController: AbortController | null = null;
/**
 * The ONE disc the page holds — never two at a time, never fields blended
 * between an old disc and a new one. The structural `inspect` on pick fills it;
 * a finished `scan` replaces it with the measured twin (same playlists, same
 * classification, measured values filled in); a threshold change replaces it
 * with a re-inspected one and discards everything measured, because a measured
 * disc classified under a stale threshold would disagree with the table.
 */
let disc: Disc | null = null;
/** The short-playlist threshold `disc` was classified under, in seconds. */
let discThreshold = 0;
/**
 * The report-section switches `reportText` was rendered with; null while no
 * report is held. Re-applying the same pair is a no-op — no render request.
 */
let renderedWith: { streamDiagnostics: boolean; quickSummary: boolean } | null = null;
/**
 * Stamps every WebAssembly request so a slow completion cannot overwrite the
 * state a faster later action produced: each request captures the counter,
 * every newer request bumps it, and a completion whose stamp is stale is
 * dropped on the floor.
 */
let generation = 0;
/** `disc.playlists`, kept unwrapped for the table and pane renderers. */
let playlists: Playlist[] = [];
/**
 * The table rows over `playlists`: a scan returns every playlist, tagged with
 * the rules that withhold it, and the settings are applied to these rows in the
 * page. So toggling a setting redraws the table instantly instead of rescanning.
 */
let allRows: PlaylistRow[] = [];
/** The rows as last drawn — filter, numbering and sort applied, top to bottom. */
let displayed: PlaylistRow[] = [];
/** The playlists the user has unticked, by name — persists across a redraw. */
const unchecked = new Set<string>();
/** The playlist whose details the panes show; null before the first draw. */
let activeName: string | null = null;

// ── settings ─────────────────────────────────────────────────────────────────

/** The dialog's settings, mirroring the desktop app's eight browser-relevant ones. */
interface Settings {
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

/** Where the settings persist between visits. */
const SETTINGS_KEY = "bdinfo-rs.settings";

/** The default short-playlist threshold, matching the CLI flag's default. */
const DEFAULT_SHORT_SECONDS = 20;

/**
 * The threshold domain's ceiling (0 = the short rule off), mirroring the
 * `min`/`max` attributes on the `#opt-short-seconds` input and, through them,
 * the library's shared threshold contract. A stored or typed value outside
 * the domain reverts here rather than reaching the module, which throws on it.
 */
const MAX_SHORT_SECONDS = 86_400;

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
  return {
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

const settings = loadSettings();

function saveSettings(): void {
  try {
    window.localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings));
  } catch {
    return;
  }
}

// ── helpers ──────────────────────────────────────────────────────────────────

function show(node: HTMLElement): void {
  node.hidden = false;
}
function hide(node: HTMLElement): void {
  node.hidden = true;
}
function showError(message: string): void {
  errorText.textContent = message;
  show(errorBox);
}
function errMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

/** {@link formatSize} under the page's current size-format setting. */
function sizeCell(bytes: number | null): string {
  return formatSize(bytes, settings.humanReadableSizes);
}

function fileListToBdmv(list: FileList): BdmvFile[] {
  return Array.from(list, (file) => ({ path: file.webkitRelativePath || file.name, file }));
}

/** One batch of a directory's entries (`readEntries` yields up to ~100 at a time). */
function readBatch(reader: FileSystemDirectoryReader): Promise<FileSystemEntry[]> {
  return new Promise((resolve, reject) => {
    reader.readEntries(resolve, reject);
  });
}

/** Walks a dropped file/dir entry into `(relativePath, File)` pairs. */
async function entryToFiles(
  entry: FileSystemEntry,
  prefix: string,
  out: BdmvFile[],
): Promise<void> {
  const path = prefix === "" ? entry.name : `${prefix}/${entry.name}`;
  if (entry.isFile) {
    const file = await new Promise<File>((resolve, reject) => {
      (entry as FileSystemFileEntry).file(resolve, reject);
    });
    out.push({ path, file });
    return;
  }
  const reader = (entry as FileSystemDirectoryEntry).createReader();
  let batch = await readBatch(reader);
  while (batch.length > 0) {
    for (const child of batch) {
      await entryToFiles(child, path, out);
    }
    batch = await readBatch(reader);
  }
}

async function collectAndLoad(roots: FileSystemEntry[]): Promise<void> {
  try {
    const out: BdmvFile[] = [];
    for (const root of roots) {
      await entryToFiles(root, "", out);
    }
    await loadFolder(out);
  } catch (error) {
    showError(errMessage(error));
  }
}

// ── flow ─────────────────────────────────────────────────────────────────────

async function loadFolder(files: BdmvFile[]): Promise<void> {
  if (files.length === 0) {
    return;
  }
  const label = files[0].path.split(/[/\\]/)[0] || "disc";
  await loadSource({ kind: "folder", files, label });
}

async function loadIso(file: File): Promise<void> {
  const label = file.name.replace(/\.iso$/i, "") || "disc";
  await loadSource({ kind: "iso", file, label });
}

async function loadSource(src: Source): Promise<void> {
  source = src;
  discName = src.label;
  pickedName.textContent = discName;
  pickedCount.textContent =
    src.kind === "folder" ? `${src.files.length} files` : "disc image (.iso)";
  mainEl.classList.remove("landing");
  show(pickedBox);
  hide(errorBox);
  hide(reportCard);
  hide(progressCard);
  hide(playlistsCard);
  hide(discardNote);
  hide(encryptedNote);
  show(listingBox);
  // A fresh pick discards everything held for the previous one.
  scanController?.abort();
  disc = null;
  reportText = "";
  renderedWith = null;
  const gen = ++generation;
  const threshold = settings.shortPlaylistSeconds;
  try {
    // The whole disc: the settings are applied to `allRows` in the page.
    const next = await inspect(src.kind === "folder" ? src.files : src.file, {
      shortPlaylistSeconds: threshold,
    });
    if (gen !== generation) {
      return;
    }
    if (next.playlists.length === 0) {
      showError(
        src.kind === "folder"
          ? "No Blu-ray playlists found. Point at a disc's BDMV folder (or the disc root)."
          : "No Blu-ray playlists found. Is this a Blu-ray .iso?",
      );
      return;
    }
    sort = null;
    activeName = null;
    unchecked.clear();
    adoptDisc(next, threshold);
    discLabel.textContent = discName;
    show(playlistsCard);
    playlistsCard.scrollIntoView({ behavior: "smooth", block: "nearest" });
  } catch (error) {
    showError(errMessage(error));
  } finally {
    hide(listingBox);
  }
}

/**
 * Takes `next` as THE held disc (classified under `threshold`) and redraws
 * everything derived from it, keeping the view state — ticks, active row,
 * sort — so a measured scan can fill in the panes' `—` cells without resetting
 * what the user set up.
 */
function adoptDisc(next: Disc, threshold: number): void {
  disc = next;
  discThreshold = threshold;
  playlists = next.playlists;
  allRows = playlistRows(next.playlists);
  encryptedNote.hidden = !next.isAacsEncrypted;
  renderRows();
}

/**
 * Whether the measured scan is offered: never for an AACS-encrypted disc, whose
 * stream data is ciphertext, so every value the scan would measure is
 * meaningless. The page still lists it — the structure comes from cleartext
 * metadata and is correct. The library imposes no such policy; this is the
 * demo's, and the desktop app's.
 */
function scanOffered(): boolean {
  return disc !== null && !disc.isAacsEncrypted;
}

/**
 * Whether the settings list `row`: every rule that classifies it as withheld
 * must be switched on.
 */
function isShown(row: PlaylistRow): boolean {
  return row.hiddenBy.every((rule) =>
    rule === "short" ? settings.showShortPlaylists : settings.showLoopingPlaylists,
  );
}

// ── sorting ──────────────────────────────────────────────────────────────────

/** A sortable playlist-table column; the sort key is the row's raw value. */
type SortColumn = "position" | "name" | "group" | "length" | "size";

/** The playlist table's active sort — a column and a direction. */
interface Sort {
  column: SortColumn;
  ascending: boolean;
}

/** The active sort; null draws the CLI's table order (by `position`). */
let sort: Sort | null = null;

/**
 * Compares two rows under `by`, on the raw values, never the cell strings. The
 * absent estimated size (the `—` cell) orders after every present value in
 * BOTH directions, so the dashes sit at the bottom whichever way the column
 * sorts — the desktop app's rule.
 */
function compareRows(a: PlaylistRow, b: PlaylistRow, by: Sort): number {
  const dir = (ordering: number): number => (by.ascending ? ordering : -ordering);
  switch (by.column) {
    case "position":
      return dir(a.position - b.position);
    case "name":
      return dir(a.name < b.name ? -1 : a.name > b.name ? 1 : 0);
    case "group":
      return dir(a.group - b.group);
    case "length":
      return dir(a.lengthTicks - b.lengthTicks);
    case "size":
      if (a.estimatedBytes === null || b.estimatedBytes === null) {
        return (a.estimatedBytes === null ? 1 : 0) - (b.estimatedBytes === null ? 1 : 0);
      }
      return dir(a.estimatedBytes - b.estimatedBytes);
  }
}

/** Whether `rows` already read ascending by `column` (ties allowed). */
function isAscending(rows: PlaylistRow[], column: SortColumn): boolean {
  const probe: Sort = { column, ascending: true };
  return rows.every((row, index) => index === 0 || compareRows(rows[index - 1], row, probe) <= 0);
}

/**
 * The desktop app's header-click rule: a click on the current sort column flips
 * its direction; a click on any other column starts it ascending — unless the
 * rows already read ascending by that column, in which case it starts
 * descending. That keeps every click a visible re-order.
 */
function clickSort(column: SortColumn): void {
  const ascending = sort?.column === column ? !sort.ascending : !isAscending(displayed, column);
  sort = { column, ascending };
  renderRows();
}

/** Points the header arrows (and `aria-sort`) at the active sort column. */
function renderSortIndicators(): void {
  for (const th of playlistsCard.querySelectorAll<HTMLTableCellElement>("th[data-sort]")) {
    const current = sort !== null && th.dataset.sort === sort.column ? sort : null;
    const arrow = th.querySelector(".arrow");
    if (arrow !== null) {
      arrow.textContent = current === null ? "" : current.ascending ? "▲" : "▼";
    }
    if (current === null) {
      th.removeAttribute("aria-sort");
    } else {
      th.setAttribute("aria-sort", current.ascending ? "ascending" : "descending");
    }
  }
}

// ── the playlist table ───────────────────────────────────────────────────────

/** Draws the rows the settings show, and the hint for the ones they do not. */
function renderRows(): void {
  const shown = allRows.filter(isShown);
  // Both numbered columns are renumbered over the shown rows, so the table
  // reads like a scan made with these settings rather than like the widened
  // listing it is sliced from. Group numbers keep their identity because the
  // rows arrive grouped: the distinct values, in order, are the new 1, 2, 3.
  // Numbering happens before sorting, so a sorted table shuffles the numbers
  // with their rows instead of re-counting the new order.
  const groups = [...new Set(shown.map((row) => row.group))];
  const numbered = shown.map((row, index) => ({
    row,
    position: index + 1,
    group: groups.indexOf(row.group) + 1,
  }));
  const by = sort;
  if (by !== null) {
    numbered.sort((a, b) => compareRows(a.row, b.row, by));
  }
  displayed = numbered.map((entry) => entry.row);
  playlistBody.replaceChildren(
    ...numbered.map((entry) => playlistRow(entry.row, entry.position, entry.group)),
  );
  renderSortIndicators();
  renderHint();
  ensureActive();
  updateSelection();
}

/** How many playlist names a hint line spells out before it counts the rest. */
const HINT_NAMES = 3;

/**
 * The hint under the table: one line per filter rule that is on *and* withheld
 * a playlist, looping first. Each rule is judged on its own against the whole
 * disc, mirroring the CLI's `Hidden by filters (…)` block — a playlist that is
 * both short and looping is named on both lines and takes both settings to
 * reveal.
 */
function renderHint(): void {
  const lines: string[] = [];
  if (!settings.showLoopingPlaylists) {
    lines.push(...hintLine("looping"));
  }
  if (!settings.showShortPlaylists) {
    lines.push(...hintLine("short"));
  }
  hiddenHint.textContent = lines.join("\n");
  hiddenHint.hidden = lines.length === 0;
}

/** The hint line for `rule`, or nothing when the rule withheld no playlist. */
function hintLine(rule: "short" | "looping"): string[] {
  const names = allRows.filter((row) => row.hiddenBy.includes(rule)).map((row) => row.name);
  if (names.length === 0) {
    return [];
  }
  const rest = names.length - HINT_NAMES;
  const more = rest > 0 ? ` and ${rest} more` : "";
  return [
    `Hidden by filters (${rule}): ${names.slice(0, HINT_NAMES).join(", ")}${more} - enable in settings`,
  ];
}

function cell(className?: string): HTMLTableCellElement {
  const td = document.createElement("td");
  if (className !== undefined) {
    td.className = className;
  }
  return td;
}
function textCell(text: string, className?: string): HTMLTableCellElement {
  const td = cell(className);
  td.textContent = text;
  return td;
}

/** A `*` marker span — hidden tracks in the master table, hidden streams below. */
function star(title: string): HTMLSpanElement {
  const marker = document.createElement("span");
  marker.className = "star";
  marker.textContent = "*";
  marker.title = title;
  return marker;
}

function playlistRow(row: PlaylistRow, position: number, group: number): HTMLTableRowElement {
  const tr = document.createElement("tr");
  tr.dataset.name = row.name;

  const check = document.createElement("input");
  check.type = "checkbox";
  check.checked = !unchecked.has(row.name);
  const checkCell = cell("col-check");
  checkCell.appendChild(check);
  tr.appendChild(checkCell);

  tr.appendChild(textCell(String(position)));

  const nameCell = cell("name");
  // The desktop app's suffix rule verbatim: two-digit count, only past one.
  nameCell.textContent =
    settings.displayChapterCount && row.chapterCount > 1
      ? `${row.name} [${String(row.chapterCount).padStart(2, "0")} Chapters]`
      : row.name;
  if (row.hasHidden) {
    nameCell.appendChild(star("Has hidden tracks"));
  }
  tr.appendChild(nameCell);

  tr.appendChild(textCell(String(group)));
  tr.appendChild(textCell(row.length));
  tr.appendChild(textCell(sizeCell(row.estimatedBytes), "num"));

  // The check cell toggles the tick; the rest of the row activates the
  // playlist and fills the detail panes, like the desktop table.
  tr.addEventListener("click", (event) => {
    if (event.target === check || event.target === checkCell) {
      if (event.target === checkCell) {
        check.checked = !check.checked;
      }
      updateSelection();
      return;
    }
    activeName = row.name;
    applyActive();
  });
  check.addEventListener("change", updateSelection);
  return tr;
}

function rowBoxes(): HTMLInputElement[] {
  return Array.from(playlistBody.querySelectorAll<HTMLInputElement>("input[type=checkbox]"));
}

function updateSelection(): void {
  let count = 0;
  for (const box of rowBoxes()) {
    const tr = box.closest("tr");
    tr?.classList.toggle("sel", box.checked);
    const name = tr?.dataset.name;
    if (name !== undefined) {
      // Remembered by name, so a row the settings hide and later show again
      // comes back with the tick the user left it with.
      if (box.checked) {
        unchecked.delete(name);
      } else {
        unchecked.add(name);
      }
    }
    if (box.checked) {
      count += 1;
    }
  }
  selCount.textContent = `${count} selected`;
  scanBtn.disabled = count === 0 || !scanOffered();
}

function selectedNames(): string[] {
  const names: string[] = [];
  for (const box of rowBoxes()) {
    const name = box.closest("tr")?.dataset.name;
    if (box.checked && name !== undefined) {
      names.push(name);
    }
  }
  return names;
}

function setAll(checked: boolean): void {
  for (const box of rowBoxes()) {
    box.checked = checked;
  }
  updateSelection();
}

// ── the detail panes ─────────────────────────────────────────────────────────

/**
 * Keeps a row active and the panes filled: when the settings hide the active
 * row (or nothing is active yet), the first shown row takes over — so the
 * panes always describe a row that is on screen.
 */
function ensureActive(): void {
  if (activeName === null || !displayed.some((row) => row.name === activeName)) {
    activeName = displayed[0]?.name ?? null;
  }
  applyActive();
}

/** Highlights the active row and redraws both panes from its playlist. */
function applyActive(): void {
  for (const tr of playlistBody.querySelectorAll("tr")) {
    tr.classList.toggle("active", tr.dataset.name === activeName);
  }
  renderPanes();
}

/** Fills both panes from the active playlist, or hides them without one. */
function renderPanes(): void {
  const playlist = playlists.find((entry) => entry.name === activeName);
  if (playlist === undefined) {
    hide(panesBox);
    return;
  }
  show(panesBox);
  paneLabel.textContent = playlist.name;
  filesBody.replaceChildren(...streamFileRows(playlist.clips));
  codecsBody.replaceChildren(...playlist.streams.map(codecRow));
}

/**
 * The "Stream Files" rows — one per clip, formatted like the desktop pane: the
 * index counts the main (angle-0) clips, so an extra-angle clip shares its
 * main clip's index and gets a ` (N)` angle suffix on the file name; the
 * estimated size prefers the interleaved `*.ssif` over the plain `*.m2ts`; a
 * size that is not yet known (no file on disk / nothing demuxed) shows as `—`.
 */
function streamFileRows(clips: Clip[]): HTMLTableRowElement[] {
  let index = 0;
  return clips.map((clip) => {
    if (clip.angleIndex === 0) {
      index += 1;
    }
    const file =
      clip.angleIndex > 0 ? `${clip.displayName} (${clip.angleIndex})` : clip.displayName;
    const estimated =
      clip.interleavedFileSizeBytes > 0 ? clip.interleavedFileSizeBytes : clip.fileSizeBytes;
    const tr = document.createElement("tr");
    tr.appendChild(textCell(file, "name"));
    tr.appendChild(textCell(String(index)));
    // The clip carries seconds only; truncating them to ticks first is the
    // table-time rule every `hh:mm:ss` cell follows.
    tr.appendChild(textCell(tableLength(Math.trunc(clip.lengthSeconds * TICKS_PER_SECOND))));
    tr.appendChild(textCell(sizeCell(estimated > 0 ? estimated : null), "num"));
    // The packet-derived size: 192 bytes per transport packet.
    tr.appendChild(textCell(sizeCell(clip.packetCount > 0 ? clip.packetCount * 192 : null), "num"));
    return tr;
  });
}

/**
 * One "Streams" (codec) row, formatted like the desktop pane. The description
 * is `fullDescription` — the same string the locked report prints — so the
 * pane matches the report; a hidden stream's codec name is marked with `*`.
 */
function codecRow(stream: Stream): HTMLTableRowElement {
  const tr = document.createElement("tr");
  const codecCell = cell();
  codecCell.textContent = stream.codecName;
  if (stream.isHidden) {
    codecCell.appendChild(star("Hidden stream"));
  }
  tr.appendChild(codecCell);
  tr.appendChild(textCell(stream.languageName));
  tr.appendChild(textCell(bitrateCell(stream.bitrateBps), "num"));
  tr.appendChild(textCell(stream.fullDescription));
  return tr;
}

/** The bit-rate cell: `N kbps` (thousands-grouped), or `—` while unmeasured. */
function bitrateCell(bitsPerSecond: number): string {
  const kbps = Math.trunc(bitsPerSecond / 1000);
  return kbps > 0 ? `${kbps.toLocaleString("en-US")} kbps` : "—";
}

// ── scan + report ────────────────────────────────────────────────────────────

function setProgress(percent: number, text: string): void {
  bar.value = percent;
  pctLabel.textContent = `${percent}%`;
  progressText.textContent = text;
}

function showReport(text: string): void {
  reportPre.textContent = text;
  show(reportCard);
  reportCard.scrollIntoView({ behavior: "smooth", block: "nearest" });
}

async function runScan(): Promise<void> {
  if (source === null || !scanOffered()) {
    return;
  }
  const selection = selectedNames();
  if (selection.length === 0) {
    return;
  }
  const src = source;
  const controller = new AbortController();
  scanController = controller;
  hide(errorBox);
  hide(reportCard);
  hide(discardNote);
  show(progressCard);
  setProgress(0, "Preparing…");
  scanBtn.disabled = true;
  const onProgress = ({ file, done, total }: { file: string; done: number; total: number }) => {
    const percent = total > 0 ? Math.floor((done / total) * 100) : 0;
    setProgress(percent, `Scanning ${file}`);
  };
  const gen = ++generation;
  const threshold = settings.shortPlaylistSeconds;
  const sections = {
    streamDiagnostics: settings.reportStreamDiagnostics,
    quickSummary: settings.reportQuickSummary,
  };
  try {
    const result = await scan(src.kind === "folder" ? src.files : src.file, onProgress, {
      selection,
      signal: controller.signal,
      shortPlaylistSeconds: threshold,
      keepPartial: settings.keepPartialScans,
      ...sections,
    });
    if (gen !== generation) {
      return;
    }
    reportText = result.report;
    renderedWith = sections;
    setProgress(100, "Done");
    // The measured disc replaces the structural one, so the panes' measured
    // sizes and bit rates fill in for the playlists this scan measured.
    adoptDisc(result.disc, threshold);
    showReport(reportText);
  } catch (error) {
    // A cancel is a user action, not a failure — reset quietly, no error shown.
    const cancelled =
      controller.signal.aborted || (error instanceof Error && error.name === "AbortError");
    if (!cancelled) {
      showError(errMessage(error));
    }
  } finally {
    scanController = null;
    hide(progressCard);
    scanBtn.disabled = selectedNames().length === 0 || !scanOffered();
  }
  // A report switch flipped while the scan ran was deferred (the disc it would
  // have rendered was about to be replaced); one cheap re-render catches up,
  // and is a no-op when nothing was flipped.
  void applyReportSections();
}

async function copyReport(): Promise<void> {
  try {
    await navigator.clipboard.writeText(reportText);
    copyLabel.textContent = "Copied!";
    copyBtn.classList.add("copied");
    window.setTimeout(() => {
      copyLabel.textContent = "Copy";
      copyBtn.classList.remove("copied");
    }, 1500);
  } catch {
    showError("Could not copy to the clipboard.");
  }
}

async function downloadReport(): Promise<void> {
  // The module's sanitizer names the file: the disc controls its own label
  // bytes, and this is the one place the demo turns that label into a path.
  let name: string;
  try {
    name = await reportFileName(discName);
  } catch (error) {
    showError(errMessage(error));
    return;
  }
  const blob = new Blob([reportText], { type: "text/plain;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  link.click();
  URL.revokeObjectURL(url);
}

// ── wiring ───────────────────────────────────────────────────────────────────

picker.addEventListener("change", () => {
  const list = picker.files;
  if (list !== null && list.length > 0) {
    void loadFolder(fileListToBdmv(list));
  }
});

isoPicker.addEventListener("change", () => {
  const file = isoPicker.files?.[0];
  if (file !== undefined) {
    void loadIso(file);
  }
});

dropzone.addEventListener("dragover", (event) => {
  event.preventDefault();
  dropzone.classList.add("drag");
});
dropzone.addEventListener("dragleave", () => {
  dropzone.classList.remove("drag");
});
dropzone.addEventListener("drop", (event) => {
  event.preventDefault();
  dropzone.classList.remove("drag");
  const items = event.dataTransfer?.items;
  if (items === undefined || items.length === 0) {
    return;
  }
  // Capture the entries synchronously — the DataTransfer is neutered after the event.
  const roots: FileSystemEntry[] = [];
  for (const item of Array.from(items)) {
    const entry = item.webkitGetAsEntry?.();
    if (entry !== null && entry !== undefined) {
      roots.push(entry);
    }
  }
  // A single dropped `.iso` → the image path (the folder walk would reject a
  // bare file with no wrapping directory).
  const only = roots[0];
  if (roots.length === 1 && only.isFile && /\.iso$/i.test(only.name)) {
    (only as FileSystemFileEntry).file(
      (file) => void loadIso(file),
      (error) => showError(errMessage(error)),
    );
    return;
  }
  void collectAndLoad(roots);
});

/**
 * Applies the report-section switches: a `renderReport` over the held measured
 * disc, replacing the held report text only — never a rescan. Without a
 * measured disc there is nothing to re-render and the choice just waits for the
 * next scan; re-applying the pair the held report was rendered with sends
 * nothing at all.
 */
async function applyReportSections(): Promise<void> {
  if (disc === null || !disc.measured) {
    return;
  }
  // Mid-scan the held disc is about to be replaced: leave it alone and let the
  // scan's own completion re-run this once the new disc is in.
  if (scanController !== null) {
    return;
  }
  const wanted = {
    streamDiagnostics: settings.reportStreamDiagnostics,
    quickSummary: settings.reportQuickSummary,
  };
  if (
    renderedWith !== null &&
    renderedWith.streamDiagnostics === wanted.streamDiagnostics &&
    renderedWith.quickSummary === wanted.quickSummary
  ) {
    return;
  }
  const held = disc;
  const gen = ++generation;
  try {
    const text = await renderReport(held, wanted);
    if (gen !== generation) {
      return;
    }
    reportText = text;
    renderedWith = wanted;
    // Replace the text in place — no scroll: the dialog is open over the page.
    reportPre.textContent = text;
    show(reportCard);
  } catch (error) {
    showError(errMessage(error));
  }
}

/**
 * Applies a committed threshold value — the one setting that still costs a
 * scan, because `hiddenBy` is classified against the threshold in force. The
 * held disc is replaced by a fresh `inspect` under the new value, and anything
 * measured is DISCARDED with a visible note beside the control: a measured disc
 * classified under the old threshold would disagree with the re-classified
 * table. A value equal to the one in force changes nothing and sends nothing.
 */
async function applyThreshold(): Promise<void> {
  // 0 is a committed value like any other: it turns the short rule off.
  const parsed = Number(optShortSeconds.value);
  if (!Number.isInteger(parsed) || parsed < 0 || parsed > MAX_SHORT_SECONDS) {
    optShortSeconds.value = String(settings.shortPlaylistSeconds);
    return;
  }
  settings.shortPlaylistSeconds = parsed;
  saveSettings();
  if (source === null || disc === null || parsed === discThreshold) {
    return;
  }
  const src = source;
  const discarding = disc.measured;
  // A scan still running would land measured results classified under the old
  // threshold — the exact stale completion the generation stamp exists to drop.
  scanController?.abort();
  const gen = ++generation;
  try {
    const next = await inspect(src.kind === "folder" ? src.files : src.file, {
      shortPlaylistSeconds: parsed,
    });
    if (gen !== generation) {
      return;
    }
    reportText = "";
    renderedWith = null;
    hide(reportCard);
    adoptDisc(next, parsed);
    discardNote.hidden = !discarding;
  } catch (error) {
    showError(errMessage(error));
  }
}

optShort.checked = settings.showShortPlaylists;
optLooping.checked = settings.showLoopingPlaylists;
optShortSeconds.value = String(settings.shortPlaylistSeconds);
optHumanSizes.checked = settings.humanReadableSizes;
optChapters.checked = settings.displayChapterCount;
optDiagnostics.checked = settings.reportStreamDiagnostics;
optSummary.checked = settings.reportQuickSummary;
optKeepPartial.checked = settings.keepPartialScans;

settingsBtn.addEventListener("click", () => {
  settingsDialog.showModal();
});
settingsClose.addEventListener("click", () => {
  settingsDialog.close();
});
// The four display settings are pure re-projections of the held disc: redraw
// the table and panes from what the page holds, no WebAssembly call.
for (const box of [optShort, optLooping, optHumanSizes, optChapters]) {
  box.addEventListener("change", () => {
    settings.showShortPlaylists = optShort.checked;
    settings.showLoopingPlaylists = optLooping.checked;
    settings.humanReadableSizes = optHumanSizes.checked;
    settings.displayChapterCount = optChapters.checked;
    saveSettings();
    renderRows();
  });
}
for (const box of [optDiagnostics, optSummary]) {
  box.addEventListener("change", () => {
    settings.reportStreamDiagnostics = optDiagnostics.checked;
    settings.reportQuickSummary = optSummary.checked;
    saveSettings();
    void applyReportSections();
  });
}
// Retention decides what a scan collects, not how the page shows it: the held
// disc keeps whatever the scan that produced it collected, so the new value is
// remembered and reaches the next scan.
optKeepPartial.addEventListener("change", () => {
  settings.keepPartialScans = optKeepPartial.checked;
  saveSettings();
});
optShortSeconds.addEventListener("change", () => {
  void applyThreshold();
});

for (const th of playlistsCard.querySelectorAll<HTMLTableCellElement>("th[data-sort]")) {
  th.addEventListener("click", () => {
    clickSort(th.dataset.sort as SortColumn);
  });
}

selectAllBtn.addEventListener("click", () => {
  setAll(true);
});
clearBtn.addEventListener("click", () => {
  setAll(false);
});
scanBtn.addEventListener("click", () => {
  void runScan();
});
cancelBtn.addEventListener("click", () => {
  scanController?.abort();
});
copyBtn.addEventListener("click", () => {
  void copyReport();
});
downloadBtn.addEventListener("click", () => {
  void downloadReport();
});
