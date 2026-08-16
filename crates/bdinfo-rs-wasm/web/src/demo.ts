// The vanilla (no-framework) demo driving the package's public API: pick or drop
// a BDMV folder, inspect it (structural scan), let the user select some
// playlists, run the measured scan in a Worker, and show the rendered report
// with copy + download. No upload — everything stays in the browser. The
// playlist table is the master of a master-detail flow: the active (highlighted)
// row populates the two detail panes below it — the playlist's stream files and
// its codecs — the same lower panes the bdinfo-rs desktop app and the classic
// BDInfo window show. The measured cells of all three tables tick WHILE a scan
// runs, from the snapshots the scan reports (`applyMeasured`): cells only, so
// the row set and the sort the user set up never move under a running scan.
// The settings dialog mirrors the desktop app's: the two
// playlist-filter opt-outs, the size format and the chapter-count suffix are
// pure re-projections of the model the page already holds; only the
// short-playlist threshold (a fresh `inspect`) and the two report sections (a
// `renderReport` over the held disc) reach the WebAssembly module, and nothing
// in the dialog ever repeats a measured scan. The retention switch sends
// nothing at all: it travels with the next measured scan the user starts.
//
// This module is the page's composition root: it owns the pick controls and
// wires every listener the feature modules beside it export.
import {
  collectAndLoad,
  copyBtn,
  copyReport,
  downloadBtn,
  downloadReport,
  fileListToBdmv,
  loadFolder,
  loadIso,
  runScan,
  scanBtn,
} from "./scan.js";
import { initSettings } from "./settings.js";
import { el, errMessage, showError, state } from "./state.js";
import { clickSort, playlistsCard, type SortColumn, setAll } from "./table.js";

const dropzone = el<HTMLLabelElement>("dropzone");
const picker = el<HTMLInputElement>("picker");
const isoPicker = el<HTMLInputElement>("iso-picker");
const selectAllBtn = el<HTMLButtonElement>("select-all");
const clearBtn = el<HTMLButtonElement>("clear-sel");
const cancelBtn = el<HTMLButtonElement>("cancel-btn");

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

initSettings();

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
  state.scanController?.abort();
});
copyBtn.addEventListener("click", () => {
  void copyReport();
});
downloadBtn.addEventListener("click", () => {
  void downloadReport();
});
