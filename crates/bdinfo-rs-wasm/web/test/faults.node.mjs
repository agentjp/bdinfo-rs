// Node read-failure characterization — what the built wasm exports produce when
// stream files cannot be read.
//
// The golden-parity harness next door proves the healthy scan; this one drives
// the SAME production export (`scan_files`) over the synthetic `MultiPlaylist`
// disc through reads that throw, and pins what comes out. A browser scanning a
// physically damaged disc is the case being modelled: `FileReaderSync` throws on
// the bad region, the wasm side records a scan error and carries on, and the
// report the user sees is assembled from whatever was readable.
//
// These are CHARACTERIZATION assertions. Every number below is what the code
// does today, not what it ought to do — a row that looks wrong is a pinned
// observation, and changing it is a deliberate decision, not a test fix.
//
// The run at the end is the cross-surface one: the same disc damaged at the
// same byte, compared against a golden the library renders from the native
// build too. The rows above characterize the browser alone and would not
// notice a core-side change that moved damaged-disc output on both sides.
//
// The disc is three playlists over three clips, one of them shared:
//
//   00000.MPLS  1640 s  00011.M2TS                marks at 0, 600, 1200, 1500
//   00001.MPLS    25 s  00022.M2TS                marks at 0, 8, 16
//   00002.MPLS  1660 s  00033.M2TS + 00011.M2TS   marks at 0, 10, 20, 620, 1220, 1520
//
// so a failure in 00011 moves two playlists, a failure in 00022 moves one, and
// 00002 is the only playlist that can lose one clip and keep another.
//
// 00011.M2TS is the one clip bigger than a read chunk (6,297,600 bytes against
// `WASM_CHUNK`), which is what lets a fault TRUNCATE a file here instead of
// voiding it: fail a read inside its second chunk and everything the first
// chunk carried survives. The other two clips are read whole in one call, so a
// fault anywhere in them takes the whole file.
//
// Prereq: `npm run build` (emits pkg/). Run with `npm run test:node`.

import { readdir, readFile } from "node:fs/promises";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { errorLine } from "../dist/format.js";
import { installShims, readLog, ShimFile, setFaults } from "./shims.mjs";

installShims();

const here = dirname(fileURLToPath(import.meta.url));
const discRoot = resolve(here, "../../../bdinfo-rs/tests/fixtures/MultiPlaylist");
const wasmPath = resolve(here, "../pkg/bdinfo_rs_wasm_bg.wasm");
// The damaged-disc golden, committed beside the healthy ones and pinned from
// BOTH sides: the library test
// `the_damaged_disc_report_matches_the_golden_the_browser_pins_too` renders it
// from the native build inside the root gate, this harness renders it from the
// built wasm inside the wasm gate. The file is the coupling point — neither
// side can move damaged-disc output without reddening the other's gate too.
const goldenPath = resolve(
  here,
  "../../../bdinfo-rs/tests/fixtures/golden/damaged-multiplaylist.txt",
);

/** The clip byte lengths, so a fault offset can be placed inside or past one. */
const CLIP_BYTES = { "00011.m2ts": 6_297_600, "00022.m2ts": 96_000, "00033.m2ts": 76_800 };

/**
 * The read-chunk size this build asks for — `m2ts::DATA_SIZE` under
 * `target_arch = "wasm32"`, which the browser build uses for BOTH the bounded
 * codec pass and the full measured pass (the native build's smaller quick chunk
 * does not apply here).
 *
 * Every read below is one chunk of it, clamped to the end of the file, so it is
 * also where a clip's chunk boundaries are: byte 5,242,880 of 00011.M2TS is
 * clip second 1365.33, between that clip's 1,200-second and 1,500-second marks.
 */
const WASM_CHUNK = 5_242_880;

// The fault modes. Offsets are inside their file unless the name says otherwise.
// 00022 and 00033 are smaller than one chunk, so a read covers either whole in
// one call and any in-file offset voids all of it; 00011 spans two chunks, so
// where in it the fault sits decides whether the clip is voided or truncated —
// the distinction a real multi-GB clip makes on a damaged disc.
const MODES = {
  // No fault at all: the reference the fault runs are read against.
  healthy: {},
  // The shared clip dies in its FIRST chunk, so nothing of it survives and both
  // passes record an error. Two playlists reference it; the third does not.
  sharedClip: { files: { "00011.m2ts": 40_000 } },
  // The shared clip dies deep — inside its SECOND chunk, past where the bounded
  // codec pass stops reading. Only the full measured pass reaches the damage, so
  // the WARNING block names the file ONCE (the field's line-per-damaged-file
  // shape), and the playlists keep every chapter the first chunk covered.
  deepFault: { files: { "00011.m2ts": WASM_CHUNK + 307_200 } },
  // Two clips die independently, which is the shape a failing drive was
  // observed to produce: a WARNING block naming more than one file.
  twoClips: { files: { "00011.m2ts": 40_000, "00022.m2ts": 10_000 } },
  // The EARLIER clip of the two-clip playlist dies while the later one reads —
  // the case where a playlist keeps measured bytes for the wrong span of itself.
  earlierClip: { files: { "00033.m2ts": 30_000 } },
  // A whole failing volume: once one read has thrown, every later read throws.
  poisonedVolume: { files: { "00033.m2ts": 30_000 }, poisonVolume: true },
  // A fault past the end of its file, so no read ever reaches it. Proves the
  // injector gates on the offset instead of failing whatever it is pointed at.
  unreachedFault: { files: { "00011.m2ts": 8_000_000 } },
};

const SELECTIONS = {
  // Every playlist on the disc.
  all: ["00000.MPLS", "00001.MPLS", "00002.MPLS"],
  // Only the playlist that loses a clip.
  defectiveOnly: ["00002.MPLS"],
  // That playlist plus one other, with a third deselected.
  defectivePlusOne: ["00002.MPLS", "00001.MPLS"],
};

// What each (mode, selection) produces today, one line per run:
//
//   reads   the clip reads in order, `@n` marking a read of chunk n (chunk 0
//           carries no marker) and `!` one that threw. The scan makes two passes
//           over the selected clips — a bounded codec pass then the full
//           measured pass — so a clip that fails in both is named twice. The
//           bounded pass stops as soon as every stream is identified, which is
//           inside the first chunk, so it never reads a `@1`.
//   warn    the files the report's `WARNING:` block names, in printed order.
//   blocks  the `PLAYLIST:` blocks the report carries, in printed order — the
//           scanned selection, in the order the caller named it, never the whole
//           disc. Re-rendering the disc model afterwards is a different matter,
//           pinned separately below.
//   then    one entry per playlist of the returned disc model:
//           `name=<per-chapter average video rate in bit/s>/<video stream rate>`.
//           Every playlist is present whichever ones were scanned, and a
//           deselected playlist sharing a scanned clip carries its tallies.
const EXPECTED = {
  "healthy/all":
    "reads=00011,00022,00033,00011,00011@1,00022,00033 warn= blocks=00000,00001,00002 " +
    "00000=19088,19088,19088,19079/19087 00001=19088,19088,18941/19035 " +
    "00002=19088,19677,19076,19088,19088,19079/19086",
  "healthy/defectiveOnly":
    "reads=00011,00033,00011,00011@1,00033 warn= blocks=00002 " +
    "00000=19088,19088,19088,19079/19087 00001=0,0,0/0 " +
    "00002=19088,19677,19076,19088,19088,19079/19086",
  "healthy/defectivePlusOne":
    "reads=00011,00022,00033,00011,00011@1,00022,00033 warn= blocks=00002,00001 " +
    "00000=19088,19088,19088,19079/19087 00001=19088,19088,18941/19035 " +
    "00002=19088,19677,19076,19088,19088,19079/19086",

  "sharedClip/all":
    "reads=00011!,00022,00033,00011!,00022,00033 warn=00011,00011 blocks=00000,00001,00002 " +
    "00000=0,0,0,0/0 00001=19088,19088,18941/19035 00002=19088,18956,0,0,0,0/0",
  "sharedClip/defectiveOnly":
    "reads=00011!,00033,00011!,00033 warn=00011,00011 blocks=00002 " +
    "00000=0,0,0,0/0 00001=0,0,0/0 00002=19088,18956,0,0,0,0/0",
  "sharedClip/defectivePlusOne":
    "reads=00011!,00022,00033,00011!,00022,00033 warn=00011,00011 blocks=00002,00001 " +
    "00000=0,0,0,0/0 00001=19088,19088,18941/19035 00002=19088,18956,0,0,0,0/0",

  // The deep fault's three tells, in one line each: ONE warning line where
  // `sharedClip` has two, a `@1!` read where the bounded pass has no `@1` at
  // all, and chapters that stop mid-table instead of going zero from the start.
  "deepFault/all":
    "reads=00011,00022,00033,00011,00011@1!,00022,00033 warn=00011 blocks=00000,00001,00002 " +
    "00000=19088,19088,10498,0/19087 00001=19088,19088,18941/19035 " +
    "00002=19088,19677,19076,19088,10498,0/19086",
  "deepFault/defectiveOnly":
    "reads=00011,00033,00011,00011@1!,00033 warn=00011 blocks=00002 " +
    "00000=19088,19088,10498,0/19087 00001=0,0,0/0 " +
    "00002=19088,19677,19076,19088,10498,0/19086",
  "deepFault/defectivePlusOne":
    "reads=00011,00022,00033,00011,00011@1!,00022,00033 warn=00011 blocks=00002,00001 " +
    "00000=19088,19088,10498,0/19087 00001=19088,19088,18941/19035 " +
    "00002=19088,19677,19076,19088,10498,0/19086",

  "twoClips/all":
    "reads=00011!,00022!,00033,00011!,00022!,00033 warn=00011,00022,00011,00022 " +
    "blocks=00000,00001,00002 00000=0,0,0,0/0 00001=0,0,0/0 00002=19088,18956,0,0,0,0/0",
  "twoClips/defectiveOnly":
    "reads=00011!,00033,00011!,00033 warn=00011,00011 blocks=00002 " +
    "00000=0,0,0,0/0 00001=0,0,0/0 00002=19088,18956,0,0,0,0/0",
  "twoClips/defectivePlusOne":
    "reads=00011!,00022!,00033,00011!,00022!,00033 warn=00011,00022,00011,00022 " +
    "blocks=00002,00001 00000=0,0,0,0/0 00001=0,0,0/0 00002=19088,18956,0,0,0,0/0",

  "earlierClip/all":
    "reads=00011,00022,00033!,00011,00011@1,00022,00033! warn=00033,00033 " +
    "blocks=00000,00001,00002 00000=19088,19088,19088,19079/19087 " +
    "00001=19088,19088,18941/19035 00002=721,0,19076,19088,19088,19079/19087",
  "earlierClip/defectiveOnly":
    "reads=00011,00033!,00011,00011@1,00033! warn=00033,00033 blocks=00002 " +
    "00000=19088,19088,19088,19079/19087 00001=0,0,0/0 " +
    "00002=721,0,19076,19088,19088,19079/19087",
  "earlierClip/defectivePlusOne":
    "reads=00011,00022,00033!,00011,00011@1,00022,00033! warn=00033,00033 blocks=00002,00001 " +
    "00000=19088,19088,19088,19079/19087 00001=19088,19088,18941/19035 " +
    "00002=721,0,19076,19088,19088,19079/19087",

  "poisonedVolume/all":
    "reads=00011,00022,00033!,00011!,00022!,00033! warn=00033,00011,00022,00033 " +
    "blocks=00000,00001,00002 00000=0,0,0,0/0 00001=0,0,0/0 00002=0,0,0,0,0,0/0",
  "poisonedVolume/defectiveOnly":
    "reads=00011,00033!,00011!,00033! warn=00033,00011,00033 blocks=00002 " +
    "00000=0,0,0,0/0 00001=0,0,0/0 00002=0,0,0,0,0,0/0",
  "poisonedVolume/defectivePlusOne":
    "reads=00011,00022,00033!,00011!,00022!,00033! warn=00033,00011,00022,00033 " +
    "blocks=00002,00001 00000=0,0,0,0/0 00001=0,0,0/0 00002=0,0,0,0,0,0/0",

  "unreachedFault/all":
    "reads=00011,00022,00033,00011,00011@1,00022,00033 warn= blocks=00000,00001,00002 " +
    "00000=19088,19088,19088,19079/19087 00001=19088,19088,18941/19035 " +
    "00002=19088,19677,19076,19088,19088,19079/19086",
  "unreachedFault/defectiveOnly":
    "reads=00011,00033,00011,00011@1,00033 warn= blocks=00002 " +
    "00000=19088,19088,19088,19079/19087 00001=0,0,0/0 " +
    "00002=19088,19677,19076,19088,19088,19079/19086",
  "unreachedFault/defectivePlusOne":
    "reads=00011,00022,00033,00011,00011@1,00022,00033 warn= blocks=00002,00001 " +
    "00000=19088,19088,19088,19079/19087 00001=19088,19088,18941/19035 " +
    "00002=19088,19677,19076,19088,19088,19079/19086",
};

/** Every file under `dir`, deepest-first within a directory, as absolute paths. */
async function walk(dir) {
  const found = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    if (entry.isDirectory()) {
      found.push(...(await walk(full)));
    } else {
      found.push(full);
    }
  }
  return found;
}

/** The clip stem of a logged read, `00011.m2ts` → `00011`. */
const stem = (name) => name.slice(0, name.lastIndexOf("."));

/** The files the report's `WARNING:` block names, in printed order. */
function warningFiles(report) {
  const [, block] = report.split("WARNING: File errors were encountered during scan:\r\n");
  if (block === undefined) {
    return [];
  }
  return block
    .split("\r\n")
    .filter((line) => line.includes("\tio error:"))
    .map((line) => stem(line.split("\t")[0]));
}

/**
 * The report with every `WARNING:` line's reason replaced by `<reason>` — the
 * library's `normalize_warning_reasons` in JavaScript.
 *
 * The block renders `{file}\t{reason}`, and the reason is each surface's own
 * error text (`io error: …` natively, the thrown JavaScript value here), so it
 * is the one field the two sides cannot share. Tabs appear nowhere else in the
 * report, which is what makes the per-line rule safe over the whole text.
 */
const normalizeWarningReasons = (report) =>
  report
    .split("\r\n")
    .map((line) => {
      const tab = line.indexOf("\t");
      return tab === -1 ? line : `${line.slice(0, tab)}\t<reason>`;
    })
    .join("\r\n");

/**
 * Whether a logged read is one whole chunk of the cadence: it starts on a chunk
 * boundary and ends one chunk later, or at the end of its file.
 */
function isChunkRead(read) {
  const size = CLIP_BYTES[read.name.toLowerCase()];
  return read.start % WASM_CHUNK === 0 && read.end === Math.min(read.start + WASM_CHUNK, size);
}

/** The `PLAYLIST:` block headings the report carries, in printed order. */
const reportBlocks = (report) =>
  [...report.matchAll(/^PLAYLIST: (\d+)\.MPLS\r$/gm)].map((match) => match[1]);

async function main() {
  const { initSync, render_report, scan_files } = await import("../pkg/bdinfo_rs_wasm.js");
  initSync({ module: await readFile(wasmPath) });

  // The disc as a folder pick: every file under it, at `MultiPlaylist/...`
  // relative paths, which is what makes the report's disc label `MultiPlaylist`.
  const paths = [];
  const files = [];
  for (const full of await walk(discRoot)) {
    const rel = relative(discRoot, full).split("\\").join("/");
    const name = rel.slice(rel.lastIndexOf("/") + 1);
    paths.push(`MultiPlaylist/${rel}`);
    files.push(new ShimFile(new Uint8Array(await readFile(full)), name));
  }

  const failures = [];
  const reports = new Map();
  const offCadenceReads = [];
  /** The healthy and deep-fault whole-disc models, for the truncation pins. */
  let healthyDisc = null;
  let deepDisc = null;
  /** The disc model of one damaged subset scan, kept for the re-render pin. */
  let subsetDisc = null;

  for (const [mode, policy] of Object.entries(MODES)) {
    for (const [label, selection] of Object.entries(SELECTIONS)) {
      const key = `${mode}/${label}`;
      setFaults(policy);
      const result = scan_files(paths, files, selection, undefined, {
        streamDiagnostics: true,
        quickSummary: true,
      });
      reports.set(key, result.report);
      if (key === "twoClips/defectiveOnly") {
        subsetDisc = result.disc;
      }
      if (key === "healthy/all") {
        healthyDisc = result.disc;
      }
      if (key === "deepFault/all") {
        deepDisc = result.disc;
      }

      const clipReads = readLog().filter((read) => read.name.endsWith(".m2ts"));
      offCadenceReads.push(...clipReads.filter((read) => !isChunkRead(read)));
      const reads = clipReads.map((read) => {
        const chunk = read.start / WASM_CHUNK;
        return `${stem(read.name)}${chunk > 0 ? `@${chunk}` : ""}${read.failed ? "!" : ""}`;
      });
      const playlists = result.disc.playlists.map((playlist) => {
        const rates = playlist.chapters.map((chapter) => Math.round(chapter.avgRateBps));
        const video = Math.round(playlist.streams[0].bitrateBps);
        return `${stem(playlist.name)}=${rates.join(",")}/${video}`;
      });

      const got = [
        `reads=${reads.join(",")}`,
        `warn=${warningFiles(result.report).join(",")}`,
        `blocks=${reportBlocks(result.report).join(",")}`,
        ...playlists,
      ].join(" ");
      if (got !== EXPECTED[key]) {
        failures.push(`${key}\n     got  ${got}\n     want ${EXPECTED[key]}`);
      }
    }
  }

  // Every clip read is one whole chunk of the cadence — start on a chunk
  // boundary, end a chunk later or at the end of the file, never anything
  // between. That is what makes the `@n` markers above readable as chunk
  // indices, and it is the cadence a live progress consumer sees: one snapshot
  // opportunity per chunk, so a one-chunk clip offers exactly one and 00011
  // offers two.
  if (offCadenceReads.length > 0) {
    failures.push(`clip reads were off the chunk cadence: ${JSON.stringify(offCadenceReads)}`);
  }

  // A fault no read reaches must change nothing at all — byte-identical reports.
  for (const label of Object.keys(SELECTIONS)) {
    if (reports.get(`unreachedFault/${label}`) !== reports.get(`healthy/${label}`)) {
      failures.push(`unreachedFault/${label} report differs from the healthy scan`);
    }
  }

  // The headline: narrowing the selection does not move the numbers of a
  // playlist that stays selected. For each fault mode, the playlist that loses a
  // clip renders identically whether the disc was scanned whole, alone, or
  // alongside one other playlist with a third deselected.
  for (const mode of Object.keys(MODES)) {
    const block = (label) => {
      const report = reports.get(`${mode}/${label}`);
      return report.slice(report.indexOf("PLAYLIST: 00002.MPLS"));
    };
    const whole = block("all").slice(0, block("defectiveOnly").length);
    if (whole !== block("defectiveOnly") || !block("defectivePlusOne").startsWith(whole)) {
      failures.push(`${mode}: 00002.MPLS renders differently across the three selections`);
    }
  }

  // Re-rendering the disc model of a damaged subset scan IS the scan's own
  // report: the model carries the playlists the scan printed (`reportOrder`),
  // so the re-render prints those and only those — byte for byte, the two
  // playlists the caller never selected absent rather than blocks of zeros.
  // Before this was fixed the re-render walked every playlist the disc
  // declares, in selection-table order (`00002,00000,00001` here).
  const rerendered = render_report(subsetDisc);
  if (reportBlocks(rerendered).join(",") !== "00002") {
    failures.push(
      `re-render blocks were ${reportBlocks(rerendered).join(",")}, not the scanned selection`,
    );
  }
  if (rerendered !== reports.get("twoClips/defectiveOnly")) {
    failures.push("re-rendering the subset scan model did not reproduce its own report");
  }

  // A read that throws a value with no message and no name still reports what
  // it threw: the seam renders the thrown payload itself rather than collapsing
  // every exotic throw to one literal, which is what a field report of a
  // damaged disc came back carrying.
  const thrown = reports
    .get("sharedClip/all")
    .split("\r\n")
    .filter((line) => line.includes("io error:"));
  if (!thrown.every((line) => line.includes("unreadable from byte 40000"))) {
    failures.push(`the WARNING lines lost the thrown value: ${JSON.stringify(thrown)}`);
  }
  if (thrown.some((line) => line.includes("JavaScript exception"))) {
    failures.push(`a WARNING line fell back to the placeholder: ${JSON.stringify(thrown)}`);
  }

  // What a browser user is shown for that same damaged scan: the demo page
  // renders `disc.errors` through its own formatter (src/format.ts), one line
  // per recorded failure. Each line must be the report's own `WARNING:` line
  // with the stage in front — the page and the report saying the same thing
  // about the same failure, which is what makes the strip trustworthy on a
  // listing that renders no report at all.
  const damagedLines = subsetDisc.errors.map(errorLine);
  const warningLines = reports
    .get("twoClips/defectiveOnly")
    .split("\r\n")
    .filter((line) => line.includes("\tio error:"));
  if (damagedLines.length !== 2) {
    failures.push(`the damaged subset scan recorded ${damagedLines.length} error(s), not 2`);
  }
  const rendered = damagedLines.every(
    (line, index) =>
      line === `${subsetDisc.errors[index].stage} ${warningLines[index]?.replace("\t", ": ")}`,
  );
  if (!rendered) {
    failures.push(
      `the page's error lines are not the report's: ${JSON.stringify(damagedLines)} against ` +
        `${JSON.stringify(warningLines)}`,
    );
  }
  if (
    !damagedLines.every(
      (line) =>
        // The stream stage, and the clip named as it is ON DISC (lower case
        // here) — where the report's own tables print it upper-cased.
        line.startsWith("stream 00011.m2ts: io error:") &&
        line.includes("unreadable from byte 40000"),
    )
  ) {
    failures.push(`the page's error lines lost the failure: ${JSON.stringify(damagedLines)}`);
  }

  // Damage deep inside a multi-chunk clip is named ONCE per file, not twice.
  // The bounded codec pass has every stream identified well inside chunk 1 and
  // stops there, so it never reaches byte 5,550,080 and records nothing; only
  // the full measured pass, reading to the end, fails. That is the WARNING shape
  // a damaged multi-GB clip produces in the field — one line per damaged file —
  // and no one-chunk clip on this disc can produce it.
  for (const label of Object.keys(SELECTIONS)) {
    const named = warningFiles(reports.get(`deepFault/${label}`));
    if (named.join(",") !== "00011") {
      failures.push(
        `deepFault/${label} WARNING named [${named.join(",")}], not 00011 exactly once`,
      );
    }
  }

  // And the data the failed read did not cost is still there. The marks are laid
  // so 00011's chunk boundary (byte 5,242,880 = clip second 1365.33) falls in
  // the SECOND-TO-LAST chapter of both playlists that play it: every chapter
  // before that one comes back carrying the healthy scan's rate, the one holding
  // the boundary comes back non-zero but short of it, and the one after it is a
  // row of zeros. Mid-file partial data, through the real exports.
  const chapterRates = (disc, name) =>
    disc.playlists
      .find((playlist) => playlist.name === name)
      .chapters.map((chapter) => Math.round(chapter.avgRateBps));
  for (const name of ["00000.MPLS", "00002.MPLS"]) {
    const healthy = chapterRates(healthyDisc, name);
    const damaged = chapterRates(deepDisc, name);
    const boundary = damaged.length - 2;
    const shape = `${name} healthy=${healthy.join(",")} damaged=${damaged.join(",")}`;
    if (damaged.slice(0, boundary).join(",") !== healthy.slice(0, boundary).join(",")) {
      failures.push(`deepFault: chapters before the boundary moved — ${shape}`);
    }
    if (!(damaged[boundary] > 0 && damaged[boundary] < healthy[boundary])) {
      failures.push(`deepFault: the chapter holding the boundary is not partial — ${shape}`);
    }
    if (damaged[boundary + 1] !== 0) {
      failures.push(`deepFault: the chapter past the boundary is not zero — ${shape}`);
    }
  }

  // The cross-surface pin. The same disc, damaged at the same byte of the same
  // stream file, scanned whole and compared against the committed golden the
  // library test renders from the native build.
  //
  // The fault sits exactly on a chunk boundary of BOTH builds — 5 MiB is one
  // browser chunk and twenty native 256 KiB ones — so each keeps the bytes
  // below it and loses every byte above, and the two reports are comparable
  // byte for byte. A fault anywhere else splits them: this side would void its
  // whole 5 MiB chunk while the native scan kept another 256 KiB of it.
  //
  // The scan is the whole-disc path (an empty selection), which renders in the
  // disc's own filtered presentation order rather than a caller's order — the
  // only shape a library render can produce, so the only one both sides share.
  setFaults({ files: { "00011.m2ts": WASM_CHUNK } });
  const pinned = normalizeWarningReasons(scan_files(paths, files, []).report);
  const golden = await readFile(goldenPath, "utf8");
  if (pinned !== golden) {
    const at = [...golden].findIndex((char, i) => pinned[i] !== char);
    const context = (text) => JSON.stringify(text.slice(Math.max(0, at - 40), at + 40));
    failures.push(
      `the damaged-disc golden moved (${pinned.length} chars against ${golden.length}), ` +
        `first difference at ${at}:\n     golden ${context(golden)}\n     got    ${context(pinned)}`,
    );
  }

  if (failures.length > 0) {
    console.error(`FAIL — ${failures.length} read-failure characterization(s) moved:`);
    for (const failure of failures) {
      console.error(`  ${failure}`);
    }
    process.exit(1);
  }

  console.log(
    `PASS — ${reports.size} read-failure runs (${Object.keys(MODES).length} fault modes ` +
      `× ${Object.keys(SELECTIONS).length} selections) match their pinned shapes, ` +
      `and the damaged-disc scan matches the shared golden (${golden.length} chars).`,
  );
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
