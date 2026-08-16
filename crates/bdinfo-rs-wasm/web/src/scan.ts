// Everything that drives the WebAssembly module: taking a picked folder or
// `.iso` through the structural `inspect`, adopting the disc that comes back,
// running the measured scan with its progress and cancel, and the report card's
// re-render, copy and download.
import {
  type BdmvFile,
  type Disc,
  inspect,
  type MeasuredSnapshot,
  renderReport,
  reportFileName,
  type ScanError,
  scan,
} from "./analyze.js";
import { counted, elapsedRemaining, errorLine, reportLabel } from "./format.js";
import { applyMeasured } from "./panes.js";
import { discardNote } from "./settings.js";
import { el, errMessage, errorBox, hide, type Source, show, showError, state } from "./state.js";
import { playlistRows, playlistsCard, renderRows, scanNames } from "./table.js";

const dropzone = el("dropzone");
const orIso = el("or-iso");
const privacy = el("privacy");
const pickedBox = el("picked");
const pickedName = el("picked-name");
const pickedCount = el("picked-count");
export const changeDiscBtn = el<HTMLButtonElement>("change-disc");
const discLabel = el("disc-label");
export const scanBtn = el<HTMLButtonElement>("scan-btn");
export const viewReportBtn = el<HTMLButtonElement>("view-report-btn");
const progressCard = el("progress-card");
const bar = el<HTMLProgressElement>("bar");
const progressSpinner = el("progress-spinner");
const pctLabel = el("pct");
const progressTimes = el("progress-times");
const progressText = el("progress-text");
export const reportCard = el("report-card");
const reportPre = el("report");
export const copyBtn = el<HTMLButtonElement>("copy-btn");
const copyLabel = el("copy-label");
export const downloadBtn = el<HTMLButtonElement>("download-btn");
const shortStreams = el("short-streams");
const shortStreamsList = el("short-streams-list");
const scanErrors = el("scan-errors");
const scanErrorsCount = el("scan-errors-count");
const scanErrorsList = el("scan-errors-list");
const mainEl = el("main");
const listingBox = el("listing");

export function fileListToBdmv(list: FileList): BdmvFile[] {
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

export async function collectAndLoad(roots: FileSystemEntry[]): Promise<void> {
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

export async function loadFolder(files: BdmvFile[]): Promise<void> {
  if (files.length === 0) {
    return;
  }
  const label = files[0].path.split(/[/\\]/)[0] || "disc";
  await loadSource({ kind: "folder", files, label });
}

export async function loadIso(file: File): Promise<void> {
  const label = file.name.replace(/\.iso$/i, "") || "disc";
  await loadSource({ kind: "iso", file, label });
}

/**
 * Swaps the source card between its two shapes: the dropzone that lands the
 * user, and the slim bar naming what is loaded. The dropzone itself is
 * unchanged either way — {@link changeDisc} simply brings it back.
 */
function showSourceBar(loaded: boolean): void {
  dropzone.hidden = loaded;
  orIso.hidden = loaded;
  privacy.hidden = loaded;
  pickedBox.hidden = !loaded;
}

/**
 * Puts the dropzone back. The loaded disc is left exactly as it stands — the
 * page keeps listing it until a new pick replaces it, so a user who opens the
 * picker and changes their mind loses nothing.
 */
export function changeDisc(): void {
  showSourceBar(false);
}

async function loadSource(src: Source): Promise<void> {
  state.source = src;
  state.discName = src.label;
  pickedName.textContent = state.discName;
  pickedCount.textContent =
    src.kind === "folder" ? counted(src.files.length, "file") : "disc image (.iso)";
  mainEl.classList.remove("landing");
  showSourceBar(true);
  hide(errorBox);
  hide(reportCard);
  hide(progressCard);
  hide(playlistsCard);
  hide(discardNote);
  show(listingBox);
  // A fresh pick discards everything held for the previous one.
  state.scanController?.abort();
  state.disc = null;
  state.reportText = "";
  state.renderedWith = null;
  state.previewing = false;
  state.paneRow = null;
  const gen = ++state.generation;
  const threshold = state.settings.shortPlaylistSeconds;
  try {
    // The whole disc: the settings are applied to `allRows` in the page.
    const next = await inspect(src.kind === "folder" ? src.files : src.file, {
      shortPlaylistSeconds: threshold,
    });
    if (gen !== state.generation) {
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
    state.sort = null;
    state.activeName = null;
    state.unchecked.clear();
    adoptDisc(next, threshold);
    discLabel.textContent = state.discName;
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
export function adoptDisc(next: Disc, threshold: number): void {
  // A new disc supersedes whatever a scan was ticking: its numbers are the
  // final form of the very cells the overlay was raising. A shown pre-scan
  // report was rendered from the disc being replaced, so it is re-rendered even
  // when the selection names come out identical.
  state.live.clear();
  state.previewOrder = null;
  state.disc = next;
  state.discThreshold = threshold;
  state.playlists = next.playlists;
  state.allRows = playlistRows(next.playlists);
  // Only a measured scan can carry short-stream notices, so a re-inspected
  // (structural) disc clears the strip along with everything else measured.
  const notices = next.shortStreamNotices ?? [];
  shortStreamsList.replaceChildren(
    ...notices.map((notice) => {
      const item = document.createElement("li");
      item.textContent = notice;
      return item;
    }),
  );
  shortStreams.hidden = notices.length === 0;
  renderScanErrors(next.errors);
  renderRows();
}

/**
 * Fills the failure strip from the held disc: one line per file the scan could
 * not read or parse, in the wording the report's `WARNING:` block and the
 * command line use.
 *
 * A structural listing records these as a measured scan does, and it renders no
 * report — so this strip, not the report, is what tells a browser user their
 * disc is damaged. A disc that recorded none hides it.
 */
function renderScanErrors(errors: ScanError[]): void {
  scanErrorsCount.textContent = `Recorded ${counted(errors.length, "error")} — the readable rest is shown.`;
  scanErrorsList.replaceChildren(
    ...errors.map((error) => {
      const item = document.createElement("li");
      item.textContent = errorLine(error);
      return item;
    }),
  );
  scanErrors.hidden = errors.length === 0;
}

/**
 * Whether the measured scan is offered: never for an AACS-encrypted disc, whose
 * stream data is ciphertext, so every value the scan would measure is
 * meaningless. The page still lists it — the structure comes from cleartext
 * metadata and is correct. The library imposes no such policy; this is the
 * demo's, and the desktop app's.
 */
export function scanOffered(): boolean {
  return state.disc !== null && !state.disc.isAacsEncrypted;
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

export async function runScan(): Promise<void> {
  if (state.source === null || !scanOffered()) {
    return;
  }
  const selection = scanNames();
  // An empty scan set means an empty table, not an empty selection — the
  // button is disabled there, and a scan started any other way is refused on
  // the same terms.
  if (selection.length === 0) {
    return;
  }
  const src = state.source;
  const controller = new AbortController();
  state.scanController = controller;
  const gen = ++state.generation;
  hide(errorBox);
  hide(reportCard);
  hide(discardNote);
  // Whatever this scan reports supersedes a pre-scan report of the same disc.
  state.previewing = false;
  show(progressCard);
  setProgress(0, "Preparing…");
  // This pass starts its tallies from zero, so whatever a cancelled or failed
  // scan left ticked goes now rather than being overwritten row by row. The
  // redraw is also what freezes the controls that name the scan set.
  state.live.clear();
  renderRows();
  const started = performance.now();
  // The last progress event's byte counts, retained so a wall-clock tick can
  // re-derive the estimate from them. Null until the first event — the blind
  // window the readout spends blank.
  let counts: { done: number; total: number } | null = null;
  // The whole blind window hangs off that one value: no estimate, no
  // percentage, and a sweeping bar instead of a flat one. On a defective disc
  // the first read can stall for the better part of a minute, and a `0%` beside
  // a motionless bar claims a measurement that has not been made.
  const showTimes = () => {
    // Read once into a local: the counts are written from another closure, so
    // only a snapshot narrows for the estimate below.
    const measured = counts;
    progressSpinner.hidden = measured !== null;
    pctLabel.hidden = measured === null;
    bar.classList.toggle("indeterminate", measured === null);
    progressTimes.textContent =
      measured === null
        ? ""
        : elapsedRemaining(measured.done, measured.total, performance.now() - started);
  };
  showTimes();
  const onProgress = ({ file, done, total }: { file: string; done: number; total: number }) => {
    const percent = total > 0 ? Math.floor((done / total) * 100) : 0;
    setProgress(percent, `Scanning ${file}`);
    counts = { done, total };
    showTimes();
  };
  // The estimate is re-derived from the retained counts on every tick, so a
  // read that stalls makes Remaining climb each second instead of holding the
  // value a now-stale event computed. Stamped like every other completion: a
  // tick belonging to a scan the page has moved on from writes nothing.
  const ticker = window.setInterval(() => {
    if (gen === state.generation) {
      showTimes();
    }
  }, 1000);
  const threshold = state.settings.shortPlaylistSeconds;
  const sections = reportSections();
  // The live cells: guarded by the same stamp the completion is, so a snapshot
  // from a scan the page has already moved on from writes nothing.
  const onMeasured = (snapshot: MeasuredSnapshot) => {
    if (gen === state.generation) {
      applyMeasured(snapshot);
    }
  };
  try {
    const result = await scan(src.kind === "folder" ? src.files : src.file, onProgress, {
      selection,
      signal: controller.signal,
      shortPlaylistSeconds: threshold,
      keepPartial: state.settings.keepPartialScans,
      onMeasured,
      ...sections,
    });
    if (gen !== state.generation) {
      return;
    }
    state.reportText = result.report;
    state.renderedWith = sections;
    setProgress(100, "Done");
    // The measured disc replaces the structural one, so the panes' measured
    // sizes and bit rates fill in for the playlists this scan measured.
    adoptDisc(result.disc, threshold);
    showReport(state.reportText);
  } catch (error) {
    // A cancel is a user action, not a failure — reset quietly, no error shown.
    const cancelled =
      controller.signal.aborted || (error instanceof Error && error.name === "AbortError");
    if (!cancelled) {
      showError(errMessage(error));
    }
  } finally {
    window.clearInterval(ticker);
    state.scanController = null;
    hide(progressCard);
    // A finished scan already dropped the overlay, adopting the measured disc.
    // A cancelled or failed one KEEPS it: what the scan did measure before it
    // stopped is real, and the desktop app leaves the same cells standing. The
    // report is not made from it — a cancel still renders none — so the redraw
    // only releases the controls and puts the cells back as they stand.
    renderRows();
  }
  // A report switch flipped while the scan ran was deferred (the disc it would
  // have rendered was about to be replaced); one cheap re-render catches up,
  // and is a no-op when nothing was flipped.
  void applyReportSections();
}

/** The report sections the settings currently ask for. */
function reportSections(): { streamDiagnostics: boolean; quickSummary: boolean } {
  return {
    streamDiagnostics: state.settings.reportStreamDiagnostics,
    quickSummary: state.settings.reportQuickSummary,
  };
}

/**
 * Renders the report over the held disc BEFORE any measured scan: the same
 * render the desktop app serves from the moment a disc is listed, over a disc
 * whose measured values are all zero because nothing has measured them yet.
 *
 * It prints the scan set the button would measure — the ticked rows, or every
 * listed row — by handing the module that selection as the disc's
 * `reportOrder`, which is the documented way to name what a render prints. The
 * disc's own order would print the library's default whole-disc set instead:
 * not the set this page is showing, and not the one a scan from here would
 * measure.
 */
async function renderPreview(): Promise<void> {
  const held = state.disc;
  // Mid-scan the held disc is about to be replaced by the measured one, whose
  // completion renders the real report over it.
  if (held === null || state.scanController !== null) {
    return;
  }
  const sections = reportSections();
  const selection = scanNames();
  // The settings can withhold every playlist the disc has. There is then
  // nothing to report on — the button that offers the report is disabled on the
  // same terms — so the shown one is withdrawn rather than re-rendered into a
  // report with no playlist in it.
  if (selection.length === 0) {
    state.previewing = false;
    state.previewOrder = null;
    state.reportText = "";
    hide(reportCard);
    return;
  }
  // Recorded as the render is REQUESTED, not when it lands: it says which
  // selection the page is rendering, so a change made during the round trip is
  // judged against that one rather than against the older order still on
  // screen — which would ask again for the render already on its way.
  state.previewOrder = selection;
  const gen = ++state.generation;
  try {
    const text = await renderReport({ ...held, reportOrder: selection }, sections);
    if (gen !== state.generation) {
      return;
    }
    state.reportText = text;
    state.renderedWith = sections;
    reportPre.textContent = text;
    show(reportCard);
  } catch (error) {
    showError(errMessage(error));
  }
}

/** The View report button: renders the pre-scan report and keeps it shown. */
export async function viewReport(): Promise<void> {
  state.previewing = true;
  await renderPreview();
  reportCard.scrollIntoView({ behavior: "smooth", block: "nearest" });
}

/**
 * Re-renders a shown pre-scan report when the scan set it prints has moved.
 *
 * A no-op unless one is shown — a measured report is not re-derived from a
 * selection — and a no-op while the set is the one already on screen: a redraw
 * for a display setting changes what the TABLE shows, and the report is not
 * rendered from that.
 */
export function refreshPreview(): void {
  if (!state.previewing) {
    return;
  }
  const selection = scanNames();
  const shown = state.previewOrder;
  if (
    shown !== null &&
    shown.length === selection.length &&
    shown.every((n, i) => n === selection[i])
  ) {
    return;
  }
  void renderPreview();
}

/**
 * Applies the report-section switches: a `renderReport` over the held measured
 * disc, replacing the held report text only — never a rescan. Without a
 * measured disc there is nothing to re-render and the choice just waits for the
 * next scan; re-applying the pair the held report was rendered with sends
 * nothing at all.
 */
export async function applyReportSections(): Promise<void> {
  // A shown pre-scan report renders under the switches too — it is the same
  // render, over a disc nothing has measured.
  if (state.previewing) {
    await renderPreview();
    return;
  }
  if (state.disc === null || !state.disc.measured) {
    return;
  }
  // Mid-scan the held disc is about to be replaced: leave it alone and let the
  // scan's own completion re-run this once the new disc is in.
  if (state.scanController !== null) {
    return;
  }
  const wanted = reportSections();
  if (
    state.renderedWith !== null &&
    state.renderedWith.streamDiagnostics === wanted.streamDiagnostics &&
    state.renderedWith.quickSummary === wanted.quickSummary
  ) {
    return;
  }
  const held = state.disc;
  const gen = ++state.generation;
  try {
    const text = await renderReport(held, wanted);
    if (gen !== state.generation) {
      return;
    }
    state.reportText = text;
    state.renderedWith = wanted;
    // Replace the text in place — no scroll: the dialog is open over the page.
    reportPre.textContent = text;
    show(reportCard);
  } catch (error) {
    showError(errMessage(error));
  }
}

export async function copyReport(): Promise<void> {
  try {
    await navigator.clipboard.writeText(state.reportText);
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

export async function downloadReport(): Promise<void> {
  // The module's sanitizer names the file: the disc controls its own label
  // bytes, and this is the one place the demo turns that label into a path.
  // The label comes off the scanned disc, so an `.iso` saves under the volume
  // label recorded in its filesystem — the name every other surface writes —
  // rather than under whatever the image file is called here.
  let name: string;
  try {
    name = await reportFileName(reportLabel(state.disc?.volumeLabel, state.discName));
  } catch (error) {
    showError(errMessage(error));
    return;
  }
  const blob = new Blob([state.reportText], { type: "text/plain;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = name;
  link.click();
  URL.revokeObjectURL(url);
}
