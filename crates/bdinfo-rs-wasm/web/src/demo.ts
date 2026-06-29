// The vanilla (no-framework) demo driving the package's public API: pick or drop
// a BDMV folder, list its playlists (structural scan), let the user select some,
// run the measured scan in a Worker, and show the rendered report with copy +
// download. No upload — everything stays in the browser.
import {
  analyze,
  analyzeIso,
  type BdmvFile,
  listPlaylists,
  listPlaylistsIso,
  type PlaylistRow,
} from "./analyze.js";

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

/** The picked disc — a `webkitdirectory` BDMV folder, or a single `.iso`. */
type Source =
  | { kind: "folder"; files: BdmvFile[]; label: string }
  | { kind: "iso"; file: File; label: string };

let source: Source | null = null;
let reportText = "";
let discName = "disc";
/** Aborts the in-progress measured scan; null when no scan is running. */
let scanController: AbortController | null = null;

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

/** A byte count as `83.62 GB` / `335.37 MB` (1024-based, like BDInfo), or `—`. */
function humanBytes(bytes: number | null): string {
  if (bytes === null || bytes <= 0) {
    return "—";
  }
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(unit === 0 ? 0 : 2)} ${units[unit]}`;
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
  show(listingBox);
  try {
    const rows =
      src.kind === "folder" ? await listPlaylists(src.files) : await listPlaylistsIso(src.file);
    if (rows.length === 0) {
      showError(
        src.kind === "folder"
          ? "No Blu-ray playlists found. Point at a disc's BDMV folder (or the disc root)."
          : "No Blu-ray playlists found. Is this a Blu-ray .iso?",
      );
      return;
    }
    renderPlaylists(rows);
  } catch (error) {
    showError(errMessage(error));
  } finally {
    hide(listingBox);
  }
}

function renderPlaylists(rows: PlaylistRow[]): void {
  playlistBody.replaceChildren();
  for (const row of rows) {
    playlistBody.appendChild(playlistRow(row));
  }
  discLabel.textContent = discName;
  updateSelection();
  show(playlistsCard);
  playlistsCard.scrollIntoView({ behavior: "smooth", block: "nearest" });
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

function playlistRow(row: PlaylistRow): HTMLTableRowElement {
  const tr = document.createElement("tr");
  tr.dataset.name = row.name;

  const check = document.createElement("input");
  check.type = "checkbox";
  check.checked = true;
  const checkCell = cell("col-check");
  checkCell.appendChild(check);
  tr.appendChild(checkCell);

  tr.appendChild(textCell(String(row.position)));

  const nameCell = cell("name");
  nameCell.textContent = row.name;
  if (row.hasHidden) {
    const star = document.createElement("span");
    star.className = "star";
    star.textContent = "*";
    star.title = "Has hidden tracks";
    nameCell.appendChild(star);
  }
  tr.appendChild(nameCell);

  tr.appendChild(textCell(String(row.group)));
  tr.appendChild(textCell(row.length));
  tr.appendChild(textCell(humanBytes(row.estimatedBytes), "num"));

  // Clicking anywhere on the row toggles its checkbox.
  tr.addEventListener("click", (event) => {
    if (event.target !== check) {
      check.checked = !check.checked;
    }
    updateSelection();
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
    if (box.checked) {
      count += 1;
    }
  }
  selCount.textContent = `${count} selected`;
  scanBtn.disabled = count === 0;
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
  if (source === null) {
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
  show(progressCard);
  setProgress(0, "Preparing…");
  scanBtn.disabled = true;
  const onProgress = ({ file, done, total }: { file: string; done: number; total: number }) => {
    const percent = total > 0 ? Math.floor((done / total) * 100) : 0;
    setProgress(percent, `Scanning ${file}`);
  };
  try {
    const options = { selection, signal: controller.signal };
    reportText =
      src.kind === "folder"
        ? await analyze(src.files, onProgress, options)
        : await analyzeIso(src.file, onProgress, options);
    setProgress(100, "Done");
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
    scanBtn.disabled = selectedNames().length === 0;
  }
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

function downloadReport(): void {
  const blob = new Blob([reportText], { type: "text/plain;charset=utf-8" });
  const url = URL.createObjectURL(blob);
  const link = document.createElement("a");
  link.href = url;
  link.download = `BDINFO.${discName}.txt`;
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
downloadBtn.addEventListener("click", downloadReport);
