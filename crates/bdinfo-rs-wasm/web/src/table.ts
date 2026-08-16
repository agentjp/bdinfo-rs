// The playlist table — the master of the master-detail flow: the rows over the
// held disc, the settings filter and the renumbering, the sort, the selection
// ticks, and the hint naming what the filters withheld.
import type { HiddenRule, Playlist } from "./analyze.js";
import { applyActive, ensureActive } from "./panes.js";
import { scanBtn, scanOffered } from "./scan.js";
import { sizeCell } from "./settings.js";
import { el, state } from "./state.js";

/**
 * One selection-table row — the CLI columns this page draws, distilled from a
 * {@link Playlist}. The disc model carries the numbers; the table needs a
 * formatted length cell and the two derived flags, so it computes them once
 * here rather than in every cell.
 */
export interface PlaylistRow {
  position: number;
  group: number;
  name: string;
  /** `hh:mm:ss`, truncated to the tick exactly as the CLI table truncates it. */
  length: string;
  /** The raw ticks behind `length` — the Length column's sort key. */
  lengthTicks: number;
  /** Interleaved `*.ssif` size, else `*.m2ts` size, else null (the `—` cell). */
  estimatedBytes: number | null;
  /**
   * Packet-derived bytes over the playlist's clips — zero until a scan measures
   * them, and the cell a running scan ticks (see `state.live`).
   */
  measuredBytes: number;
  /** Whether the playlist hides any stream (the CLI's `(*)` note). */
  hasHidden: boolean;
  hiddenBy: HiddenRule[];
  /** Chapter count behind the optional ` [NN Chapters]` name suffix. */
  chapterCount: number;
}

/** The ticks in one second — the unit `totalLengthTicks` counts. */
export const TICKS_PER_SECOND = 10_000_000;

/** Bytes per transport packet — what turns a packet count into a size cell. */
export const PACKET_BYTES = 192;

export const playlistsCard = el("playlists-card");
export const playlistBody = el<HTMLTableSectionElement>("playlist-body");
const selCount = el("sel-count");
const hiddenHint = el("hidden-hint");

/** A row per playlist, in the table order `position` records. */
export function playlistRows(playlists: Playlist[]): PlaylistRow[] {
  return playlists
    .map((playlist) => ({
      position: playlist.position,
      group: playlist.group,
      name: playlist.name,
      length: tableLength(playlist.totalLengthTicks),
      lengthTicks: playlist.totalLengthTicks,
      estimatedBytes: playlist.interleavedFileSizeBytes || playlist.fileSizeBytes || null,
      measuredBytes: playlist.clips.reduce(
        (bytes, clip) => bytes + clip.packetCount * PACKET_BYTES,
        0,
      ),
      hasHidden: playlist.streams.some((stream) => stream.isHidden),
      hiddenBy: playlist.hiddenBy,
      chapterCount: playlist.chapterCount,
    }))
    .sort((a, b) => a.position - b.position);
}

/** `hh:mm:ss` from playlist ticks, truncated like the CLI table (hours wrap at 24). */
export function tableLength(ticks: number): string {
  const total = Math.max(0, Math.trunc(ticks / TICKS_PER_SECOND));
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${pad(Math.trunc(total / 3600) % 24)}:${pad(Math.trunc(total / 60) % 60)}:${pad(total % 60)}`;
}

/**
 * Whether the settings list `row`: every rule that classifies it as withheld
 * must be switched on.
 */
function isShown(row: PlaylistRow): boolean {
  return row.hiddenBy.every((rule) =>
    rule === "short" ? state.settings.showShortPlaylists : state.settings.showLoopingPlaylists,
  );
}

// ── sorting ──────────────────────────────────────────────────────────────────

/** A sortable playlist-table column; the sort key is the row's raw value. */
export type SortColumn = "position" | "name" | "group" | "length" | "size" | "measured";

/** The playlist table's active sort — a column and a direction. */
export interface Sort {
  column: SortColumn;
  ascending: boolean;
}

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
    case "measured":
      // Unmeasured is a zero, not an absent value like the estimated size
      // above: a scan measured nothing there yet, so it sorts as the smallest
      // number rather than being pushed to the bottom in both directions.
      return dir(measuredBytes(a) - measuredBytes(b));
  }
}

/** The measured bytes a row shows: the running scan's, else the held disc's. */
function measuredBytes(row: PlaylistRow): number {
  return state.live.get(row.name)?.measuredBytes ?? row.measuredBytes;
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
export function clickSort(column: SortColumn): void {
  const ascending =
    state.sort?.column === column ? !state.sort.ascending : !isAscending(state.displayed, column);
  state.sort = { column, ascending };
  renderRows();
}

/** Points the header arrows (and `aria-sort`) at the active sort column. */
function renderSortIndicators(): void {
  for (const th of playlistsCard.querySelectorAll<HTMLTableCellElement>("th[data-sort]")) {
    const current =
      state.sort !== null && th.dataset.sort === state.sort.column ? state.sort : null;
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
export function renderRows(): void {
  const shown = state.allRows.filter(isShown);
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
  const by = state.sort;
  if (by !== null) {
    numbered.sort((a, b) => compareRows(a.row, b.row, by));
  }
  state.displayed = numbered.map((entry) => entry.row);
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
  if (!state.settings.showLoopingPlaylists) {
    lines.push(...hintLine("looping"));
  }
  if (!state.settings.showShortPlaylists) {
    lines.push(...hintLine("short"));
  }
  hiddenHint.textContent = lines.join("\n");
  hiddenHint.hidden = lines.length === 0;
}

/** The hint line for `rule`, or nothing when the rule withheld no playlist. */
function hintLine(rule: "short" | "looping"): string[] {
  const names = state.allRows.filter((row) => row.hiddenBy.includes(rule)).map((row) => row.name);
  if (names.length === 0) {
    return [];
  }
  const rest = names.length - HINT_NAMES;
  const more = rest > 0 ? ` and ${rest} more` : "";
  return [
    `Hidden by filters (${rule}): ${names.slice(0, HINT_NAMES).join(", ")}${more} - enable in settings`,
  ];
}

export function cell(className?: string): HTMLTableCellElement {
  const td = document.createElement("td");
  if (className !== undefined) {
    td.className = className;
  }
  return td;
}
export function textCell(text: string, className?: string): HTMLTableCellElement {
  const td = cell(className);
  td.textContent = text;
  return td;
}

/** A `*` marker span — hidden tracks in the master table, hidden streams below. */
export function star(title: string): HTMLSpanElement {
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
  check.checked = !state.unchecked.has(row.name);
  const checkCell = cell("col-check");
  checkCell.appendChild(check);
  tr.appendChild(checkCell);

  tr.appendChild(textCell(String(position)));

  const nameCell = cell("name");
  // The desktop app's suffix rule verbatim: two-digit count, only past one.
  nameCell.textContent =
    state.settings.displayChapterCount && row.chapterCount > 1
      ? `${row.name} [${String(row.chapterCount).padStart(2, "0")} Chapters]`
      : row.name;
  if (row.hasHidden) {
    nameCell.appendChild(star("Has hidden tracks"));
  }
  tr.appendChild(nameCell);

  tr.appendChild(textCell(String(group)));
  tr.appendChild(textCell(row.length));
  tr.appendChild(textCell(sizeCell(row.estimatedBytes), "num"));

  // The measured cell a running scan ticks: tagged so a snapshot can find it
  // without knowing the column order (see `applyMeasured`).
  const measured = textCell(sizeCell(measuredBytes(row) || null), "num");
  measured.dataset.cell = "measured";
  tr.appendChild(measured);

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
    state.activeName = row.name;
    applyActive();
  });
  check.addEventListener("change", updateSelection);
  return tr;
}

function rowBoxes(): HTMLInputElement[] {
  return Array.from(playlistBody.querySelectorAll<HTMLInputElement>("input[type=checkbox]"));
}

export function updateSelection(): void {
  let count = 0;
  for (const box of rowBoxes()) {
    const tr = box.closest("tr");
    tr?.classList.toggle("sel", box.checked);
    const name = tr?.dataset.name;
    if (name !== undefined) {
      // Remembered by name, so a row the settings hide and later show again
      // comes back with the tick the user left it with.
      if (box.checked) {
        state.unchecked.delete(name);
      } else {
        state.unchecked.add(name);
      }
    }
    if (box.checked) {
      count += 1;
    }
  }
  selCount.textContent = `${count} selected`;
  scanBtn.disabled = count === 0 || !scanOffered();
}

export function selectedNames(): string[] {
  const names: string[] = [];
  for (const box of rowBoxes()) {
    const name = box.closest("tr")?.dataset.name;
    if (box.checked && name !== undefined) {
      names.push(name);
    }
  }
  return names;
}

export function setAll(checked: boolean): void {
  for (const box of rowBoxes()) {
    box.checked = checked;
  }
  updateSelection();
}
