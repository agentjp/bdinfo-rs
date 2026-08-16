// The demo's pure formatters, in a module of its own so a test can reach them:
// `demo.ts` reads the page's DOM at import time and cannot be loaded outside a
// browser. Nothing in the published package imports this — the demo is the
// site, not the API.
//
// The desktop app formats the same cell in Rust (`model::byte_cell`), which no
// amount of arranging can share with TypeScript. What holds the two together is
// the vector table `crates/bdinfo-rs-gui/tests/size-vectors.tsv`: both sides
// assert every row of it, so a change here that is not mirrored there fails a
// test on both.

import type { Disc, ScanError, ScanErrorReason } from "./analyze.js";

/**
 * The count with its noun's number agreed — `"1 error"`, `"0 errors"`,
 * `"3 files"`. The noun is the singular; the plural is `s`-appended, which
 * every noun the demo counts inflects regularly.
 */
export function counted(n: number, noun: string): string {
  return n === 1 ? `1 ${noun}` : `${n} ${noun}s`;
}

/**
 * A size cell under the size-format setting: `83.62 GB` / `335.37 MB`
 * (1024-based, like BDInfo) when `humanReadable`, the thousands-grouped exact
 * byte count (`11,145,216`) when not, and `—` for a size nothing knows yet.
 */
export function sizeCell(bytes: number | null, humanReadable: boolean): string {
  if (bytes === null || bytes <= 0) {
    return "—";
  }
  if (!humanReadable) {
    return bytes.toLocaleString("en-US");
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

/**
 * The disc's detected-feature labels, in presentation order — the `Extras:`
 * line the report prints, and the disc-info strip's `Detected Features`.
 *
 * The labels and their order are the library's `BdRom::extra_features`
 * (`crates/bdinfo-rs-core/src/bdrom/disc.rs`), written a second time here
 * because a browser page cannot call it: the mirror carries the six flags, not
 * the strings they stand for. `test/parity.node.mjs` renders a disc with every
 * flag set and compares this list against the report's own `Extras:` line, so a
 * label or an order that moves on the Rust side fails there rather than
 * drifting quietly.
 */
export function featureLabels(disc: Disc): string[] {
  const flags: [boolean, string][] = [
    [disc.isUhd, "Ultra HD"],
    [disc.isBdJava, "BD-Java"],
    [disc.is50hz, "50Hz Content"],
    [disc.is3d, "Blu-ray 3D"],
    [disc.isDbox, "D-BOX Motion Code"],
    [disc.isPsp, "PSP Digital Copy"],
  ];
  return flags.filter(([set]) => set).map(([, label]) => label);
}

/**
 * One recorded scan failure as one line — `{stage} {file}: {reason}`, the
 * sentence the `bdinfo-rs` command line prints on stderr and the desktop app
 * banners.
 *
 * The wording is the library's `ScanError`/`BdError` display, which crosses to
 * the browser as structured data rather than as text, so the mapping below is
 * the second copy of it. `faults.node.mjs` renders the errors of a real damaged
 * scan through this function, so a reason whose wording moves in the library
 * fails that harness rather than drifting quietly.
 */
export function errorLine(error: ScanError): string {
  return `${error.stage} ${error.file}: ${errorReason(error.reason)}`;
}

/** The reason half of {@link errorLine}. */
export function errorReason(reason: ScanErrorReason): string {
  switch (reason.kind) {
    case "unknownFileType":
      return `unknown file type: ${reason.magic}`;
    case "unexpectedEof":
      return "unexpected end of input";
    case "structureNotFound":
      return "unable to locate BD structure";
    case "missingClipFile":
      return `referenced missing clip file: ${reason.file}`;
    case "io":
      return `io error: ${reason.message}`;
    case "metadataTooLarge":
      return `metadata file too large: ${reason.file} exceeds ${reason.limitBytes} bytes`;
    // The open kinds — a cancelled scan today, anything the library adds
    // tomorrow — carry their own message and nothing to prefix it with.
    case "other":
      return reason.message;
  }
}

/** What the remaining field reads while nothing has been measured to extrapolate from. */
const NO_ESTIMATE = "--:--:--";

/**
 * `hh:mm:ss` from whole seconds, hours ACCUMULATING — a scan past a day reads
 * `25:01:01`. Deliberately unlike the table's `tableLength`, which wraps at 24
 * because a playlist runtime never legitimately exceeds a day and a wall-clock
 * estimate can.
 */
function hms(seconds: number): string {
  const pad = (value: number) => String(value).padStart(2, "0");
  return `${pad(Math.trunc(seconds / 3600))}:${pad(Math.trunc(seconds / 60) % 60)}:${pad(seconds % 60)}`;
}

/**
 * The progress card's `Elapsed hh:mm:ss · Remaining hh:mm:ss`, from the byte
 * counts the last progress event reported and the wall time since the scan
 * started.
 *
 * The estimate scales the elapsed time by the bytes still to read, so
 * re-deriving it from RETAINED counts at a later `elapsedMs` makes it climb —
 * which is what a caller ticking this on a wall clock wants a stalled read to
 * look like. Before the first byte is measured there is nothing to extrapolate
 * from, and the field reads {@link NO_ESTIMATE} rather than `00:00:00`, which
 * would say "about to finish" for however long the stall lasts.
 *
 * The arithmetic is the library's `bdrom::progress::progress_stats` +
 * `remaining_hms` (Rust), hand-written a second time here because a browser
 * page cannot call it; the vectors in `test/parity.node.mjs` are the ones that
 * module's own unit tests assert, so a change to either side that is not
 * mirrored fails a test.
 */
export function elapsedRemaining(done: number, total: number, elapsedMs: number): string {
  const left = Math.max(0, total - done);
  const remaining = done === 0 ? NO_ESTIMATE : hms(Math.trunc((elapsedMs * left) / (done * 1000)));
  return `Elapsed ${hms(Math.trunc(elapsedMs / 1000))} · Remaining ${remaining}`;
}

/**
 * The disc label a saved report is named after: the disc's own volume label,
 * falling back to `picked` — the name of the folder or file the user chose —
 * when no disc is held or its label is empty.
 *
 * The two differ on an `.iso`, whose volume label is the one recorded in the
 * UDF filesystem while the file name is only what the image happens to be
 * called on this machine. Every other surface names the report after the
 * volume label, so the browser does too; the page still SHOWS the picked name,
 * which is what the user recognizes.
 */
export function reportLabel(volumeLabel: string | undefined, picked: string): string {
  return volumeLabel !== undefined && volumeLabel !== "" ? volumeLabel : picked;
}
