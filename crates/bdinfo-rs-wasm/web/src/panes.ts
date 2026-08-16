// The two detail panes under the playlist table — the active playlist's stream
// files and its codecs — and the in-place cell writes that let all three tables
// tick while a scan runs.
import type { Clip, MeasuredPlaylist, MeasuredSnapshot, Stream } from "./analyze.js";
import { sizeCell } from "./settings.js";
import { el, hide, show, state } from "./state.js";
import {
  cell,
  PACKET_BYTES,
  playlistBody,
  star,
  TICKS_PER_SECOND,
  tableLength,
  textCell,
} from "./table.js";

const panesBox = el("detail-panes");
const paneLabel = el("pane-playlist");
const filesBody = el<HTMLTableSectionElement>("files-body");
const codecsBody = el<HTMLTableSectionElement>("codecs-body");

/**
 * Keeps a row active and the panes filled: when the settings hide the active
 * row (or nothing is active yet), the first shown row takes over — so the
 * panes always describe a row that is on screen.
 */
export function ensureActive(): void {
  if (state.activeName === null || !state.displayed.some((row) => row.name === state.activeName)) {
    state.activeName = state.displayed[0]?.name ?? null;
  }
  applyActive();
}

/** Highlights the active row and redraws both panes from its playlist. */
export function applyActive(): void {
  for (const tr of playlistBody.querySelectorAll("tr")) {
    tr.classList.toggle("active", tr.dataset.name === state.activeName);
  }
  renderPanes();
}

/** Fills both panes from the active playlist, or hides them without one. */
function renderPanes(): void {
  const playlist = state.playlists.find((entry) => entry.name === state.activeName);
  if (playlist === undefined) {
    hide(panesBox);
    return;
  }
  show(panesBox);
  paneLabel.textContent = playlist.name;
  const measured = state.live.get(playlist.name);
  filesBody.replaceChildren(...streamFileRows(playlist.clips, measured));
  codecsBody.replaceChildren(...playlist.streams.map((stream) => codecRow(stream, measured)));
}

/**
 * The "Stream Files" rows — one per clip, formatted like the desktop pane: the
 * index counts the main (angle-0) clips, so an extra-angle clip shares its
 * main clip's index and gets a ` (N)` angle suffix on the file name; the
 * estimated size prefers the interleaved `*.ssif` over the plain `*.m2ts`; a
 * size that is not yet known (no file on disk / nothing demuxed) shows as `—`.
 *
 * `measured` is the running scan's tallies for this playlist, whose per-clip
 * rows line up with `clips` one for one; without it the cells come from the
 * held disc.
 */
function streamFileRows(clips: Clip[], measured?: MeasuredPlaylist): HTMLTableRowElement[] {
  let index = 0;
  return clips.map((clip, position) => {
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
    // The packet-derived size: 192 bytes per transport packet. Tagged like the
    // table's measured cell, so a snapshot can patch it where it stands.
    const bytes = measured?.clips.at(position)?.measuredBytes ?? clip.packetCount * PACKET_BYTES;
    const packets = textCell(sizeCell(bytes > 0 ? bytes : null), "num");
    packets.dataset.cell = "clip-measured";
    tr.appendChild(packets);
    return tr;
  });
}

/**
 * One "Streams" (codec) row, formatted like the desktop pane. The description
 * is `fullDescription` — the same string the locked report prints — so the
 * pane matches the report; a hidden stream's codec name is marked with `*`.
 *
 * The rate comes from `measured` — the running scan's tallies for this
 * playlist — when it carries this row, and from the held disc otherwise. The
 * row records the pair that names it, since the snapshot orders its streams
 * differently and can only be matched by `(pid, angleIndex)`.
 */
function codecRow(stream: Stream, measured?: MeasuredPlaylist): HTMLTableRowElement {
  const tr = document.createElement("tr");
  tr.dataset.pid = String(stream.pid);
  tr.dataset.angle = String(stream.angleIndex);
  const codecCell = cell();
  codecCell.textContent = stream.codecName;
  if (stream.isHidden) {
    codecCell.appendChild(star("Hidden stream"));
  }
  tr.appendChild(codecCell);
  tr.appendChild(textCell(stream.languageName));
  const rate = textCell(bitrateCell(liveRate(measured, stream) ?? stream.bitrateBps), "num");
  rate.dataset.cell = "bitrate";
  tr.appendChild(rate);
  tr.appendChild(textCell(stream.fullDescription));
  return tr;
}

/** `measured`'s rate for the row `(pid, angleIndex)` names, when it has one. */
function liveRate(measured: MeasuredPlaylist | undefined, stream: Stream): number | undefined {
  return measured?.streams.find(
    (row) => row.pid === stream.pid && row.angleIndex === stream.angleIndex,
  )?.bitrateBps;
}

/** The bit-rate cell: `N kbps` (thousands-grouped), or `—` while unmeasured. */
function bitrateCell(bitsPerSecond: number): string {
  const kbps = Math.trunc(bitsPerSecond / 1000);
  return kbps > 0 ? `${kbps.toLocaleString("en-US")} kbps` : "—";
}

// ── live measured cells ──────────────────────────────────────────────────────

/**
 * Takes one snapshot of the running scan and writes the cells it moved, in
 * place: the playlist table's measured column, and the two panes when the
 * snapshot covers the active playlist.
 *
 * Cells only — no row is added, removed, re-sorted or re-numbered, and the
 * report is not re-rendered. That is what makes ticking safe during a scan: the
 * table the user set up (its sort, its ticks, its active row) is the same table
 * a second later, with different numbers in it. A snapshot covers only the
 * playlists that play the stream file it was taken over, so every other row
 * keeps the number it already shows.
 */
export function applyMeasured(snapshot: MeasuredSnapshot): void {
  for (const playlist of snapshot.playlists) {
    state.live.set(playlist.name, playlist);
  }
  for (const tr of playlistBody.querySelectorAll("tr")) {
    const measured = state.live.get(tr.dataset.name ?? "");
    const cell = tr.querySelector('[data-cell="measured"]');
    if (measured !== undefined && cell !== null) {
      cell.textContent = sizeCell(measured.measuredBytes || null);
    }
  }
  const active = state.activeName === null ? undefined : state.live.get(state.activeName);
  if (active !== undefined) {
    patchPanes(active);
  }
}

/** Writes `measured` into the open panes' cells, leaving their rows alone. */
function patchPanes(measured: MeasuredPlaylist): void {
  Array.from(filesBody.rows).forEach((tr, position) => {
    const clip = measured.clips.at(position);
    const cell = tr.querySelector('[data-cell="clip-measured"]');
    if (clip !== undefined && cell !== null) {
      cell.textContent = sizeCell(clip.measuredBytes || null);
    }
  });
  for (const tr of codecsBody.rows) {
    const rate = measured.streams.find(
      (row) => String(row.pid) === tr.dataset.pid && String(row.angleIndex) === tr.dataset.angle,
    );
    const cell = tr.querySelector('[data-cell="bitrate"]');
    if (rate !== undefined && cell !== null) {
      cell.textContent = bitrateCell(rate.bitrateBps);
    }
  }
}
