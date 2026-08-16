// The settings dialog: what it persists, what a change re-projects from the
// held disc, and the one control (the short-playlist threshold) that costs a
// fresh `inspect`.
import { inspect } from "./analyze.js";
import { sizeCell as formatSize } from "./format.js";
import { adoptDisc, applyReportSections, reportCard } from "./scan.js";
import {
  el,
  errMessage,
  hide,
  MAX_SHORT_SECONDS,
  saveSettings,
  showError,
  state,
} from "./state.js";
import { renderRows } from "./table.js";

/** {@link formatSize} under the page's current size-format setting. */
export function sizeCell(bytes: number | null): string {
  return formatSize(bytes, state.settings.humanReadableSizes);
}

const settingsBtn = el<HTMLButtonElement>("settings-btn");
const settingsDialog = el<HTMLDialogElement>("settings-dialog");
const settingsClose = el<HTMLButtonElement>("settings-close");
const optShort = el<HTMLInputElement>("opt-short");
const optShortSeconds = el<HTMLInputElement>("opt-short-seconds");
export const discardNote = el("discard-note");
const optLooping = el<HTMLInputElement>("opt-looping");
const optHumanSizes = el<HTMLInputElement>("opt-human-sizes");
const optChapters = el<HTMLInputElement>("opt-chapters");
const optDiagnostics = el<HTMLInputElement>("opt-diagnostics");
const optSummary = el<HTMLInputElement>("opt-summary");
const optKeepPartial = el<HTMLInputElement>("opt-keep-partial");

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
    optShortSeconds.value = String(state.settings.shortPlaylistSeconds);
    return;
  }
  state.settings.shortPlaylistSeconds = parsed;
  saveSettings();
  if (state.source === null || state.disc === null || parsed === state.discThreshold) {
    return;
  }
  const src = state.source;
  const discarding = state.disc.measured;
  // A scan still running would land measured results classified under the old
  // threshold — the exact stale completion the generation stamp exists to drop.
  state.scanController?.abort();
  const gen = ++state.generation;
  try {
    const next = await inspect(src.kind === "folder" ? src.files : src.file, {
      shortPlaylistSeconds: parsed,
    });
    if (gen !== state.generation) {
      return;
    }
    state.reportText = "";
    state.renderedWith = null;
    hide(reportCard);
    adoptDisc(next, parsed);
    discardNote.hidden = !discarding;
  } catch (error) {
    showError(errMessage(error));
  }
}

/** Puts the held settings in the dialog's controls and wires their changes. */
export function initSettings(): void {
  optShort.checked = state.settings.showShortPlaylists;
  optLooping.checked = state.settings.showLoopingPlaylists;
  optShortSeconds.value = String(state.settings.shortPlaylistSeconds);
  optHumanSizes.checked = state.settings.humanReadableSizes;
  optChapters.checked = state.settings.displayChapterCount;
  optDiagnostics.checked = state.settings.reportStreamDiagnostics;
  optSummary.checked = state.settings.reportQuickSummary;
  optKeepPartial.checked = state.settings.keepPartialScans;

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
      state.settings.showShortPlaylists = optShort.checked;
      state.settings.showLoopingPlaylists = optLooping.checked;
      state.settings.humanReadableSizes = optHumanSizes.checked;
      state.settings.displayChapterCount = optChapters.checked;
      saveSettings();
      renderRows();
    });
  }
  for (const box of [optDiagnostics, optSummary]) {
    box.addEventListener("change", () => {
      state.settings.reportStreamDiagnostics = optDiagnostics.checked;
      state.settings.reportQuickSummary = optSummary.checked;
      saveSettings();
      void applyReportSections();
    });
  }
  // Retention decides what a scan collects, not how the page shows it: the held
  // disc keeps whatever the scan that produced it collected, so the new value is
  // remembered and reaches the next scan.
  optKeepPartial.addEventListener("change", () => {
    state.settings.keepPartialScans = optKeepPartial.checked;
    saveSettings();
  });
  optShortSeconds.addEventListener("change", () => {
    void applyThreshold();
  });
}
