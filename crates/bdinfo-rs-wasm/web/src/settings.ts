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
  type Settings,
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
const themeChips = Array.from(
  el("theme-chips").querySelectorAll<HTMLButtonElement>("button[data-theme-choice]"),
);

/** What each palette paints the page ground, mirroring --bg in index.html. */
const THEME_COLORS = { light: "#f4f6f9", dark: "#0b0d10" };

/**
 * Puts the stored theme choice into effect: the data-theme attribute for a
 * manual choice (Auto removes it, and the prefers-color-scheme blocks in
 * index.html decide), the pressed state on the dialog's chips, and the
 * theme-color metas. The metas get the EFFECTIVE palette's color written into
 * both of them: their media form tracks only the OS, so it cannot follow a
 * manual override — and once overwritten, an Auto page needs the
 * matchMedia listener below to keep them in step with an OS flip.
 */
function applyTheme(): void {
  const choice = state.settings.theme;
  if (choice === "auto") {
    delete document.documentElement.dataset.theme;
  } else {
    document.documentElement.dataset.theme = choice;
  }
  for (const chip of themeChips) {
    chip.setAttribute("aria-pressed", String(chip.dataset.themeChoice === choice));
  }
  const effective =
    choice === "auto"
      ? window.matchMedia("(prefers-color-scheme: light)").matches
        ? "light"
        : "dark"
      : choice;
  for (const meta of document.querySelectorAll<HTMLMetaElement>('meta[name="theme-color"]')) {
    meta.content = THEME_COLORS[effective];
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
  // The boot script in index.html already set the manual attribute before
  // first paint; this pass adds what it could not — the chips and the metas.
  applyTheme();
  for (const chip of themeChips) {
    chip.addEventListener("click", () => {
      state.settings.theme = chip.dataset.themeChoice as Settings["theme"];
      saveSettings();
      applyTheme();
    });
  }
  window.matchMedia("(prefers-color-scheme: light)").addEventListener("change", () => {
    if (state.settings.theme === "auto") {
      applyTheme();
    }
  });
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
