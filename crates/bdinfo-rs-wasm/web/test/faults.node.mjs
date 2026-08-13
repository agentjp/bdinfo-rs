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
// The disc is three playlists over three clips, one of them shared:
//
//   00000.MPLS  30 s  00011.M2TS                marks at 0, 10, 20
//   00001.MPLS  25 s  00022.M2TS                marks at 0, 8, 16
//   00002.MPLS  50 s  00033.M2TS + 00011.M2TS   marks at 0, 10, 20, 35
//
// so a failure in 00011 moves two playlists, a failure in 00022 moves one, and
// 00002 is the only playlist that can lose one clip and keep another.
//
// Prereq: `npm run build` (emits pkg/). Run with `npm run test:node`.

import { readdir, readFile } from "node:fs/promises";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { installShims, readLog, ShimFile, setFaults } from "./shims.mjs";

installShims();

const here = dirname(fileURLToPath(import.meta.url));
const discRoot = resolve(here, "../../../bdinfo-rs/tests/fixtures/MultiPlaylist");
const wasmPath = resolve(here, "../pkg/bdinfo_rs_wasm_bg.wasm");

/** The clip byte lengths, so a fault offset can be placed inside or past one. */
const CLIP_BYTES = { "00011.m2ts": 115_200, "00022.m2ts": 96_000, "00033.m2ts": 76_800 };

// The fault modes. Offsets are inside their file unless the name says otherwise;
// every clip here is far smaller than the 5 MiB chunk the demux asks for, so a
// read covers a whole file in one call and any in-file offset voids all of it.
// A real multi-GB clip fails one chunk and keeps the chunks before it.
const MODES = {
  // No fault at all: the reference the fault runs are read against.
  healthy: {},
  // The shared clip dies. Two playlists reference it; the third does not.
  sharedClip: { files: { "00011.m2ts": 40_000 } },
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
  unreachedFault: { files: { "00011.m2ts": 200_000 } },
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
//   reads   the clip reads in order, `!` marking one that threw. The scan makes
//           two passes over the selected clips — a bounded codec pass then the
//           full measured pass — so a clip that fails in both is named twice.
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
    "reads=00011,00022,00033,00011,00022,00033 warn= blocks=00000,00001,00002 " +
    "00000=19088,19088,18956/19044 00001=19088,19088,18941/19035 " +
    "00002=19088,19677,18607,19000/19035",
  "healthy/defectiveOnly":
    "reads=00011,00033,00011,00033 warn= blocks=00002 " +
    "00000=19088,19088,18956/19044 00001=0,0,0/0 00002=19088,19677,18607,19000/19035",
  "healthy/defectivePlusOne":
    "reads=00011,00022,00033,00011,00022,00033 warn= blocks=00002,00001 " +
    "00000=19088,19088,18956/19044 00001=19088,19088,18941/19035 " +
    "00002=19088,19677,18607,19000/19035",

  "sharedClip/all":
    "reads=00011!,00022,00033,00011!,00022,00033 warn=00011,00011 blocks=00000,00001,00002 " +
    "00000=0,0,0/0 00001=19088,19088,18941/19035 00002=19088,18956,0,0/0",
  "sharedClip/defectiveOnly":
    "reads=00011!,00033,00011!,00033 warn=00011,00011 blocks=00002 " +
    "00000=0,0,0/0 00001=0,0,0/0 00002=19088,18956,0,0/0",
  "sharedClip/defectivePlusOne":
    "reads=00011!,00022,00033,00011!,00022,00033 warn=00011,00011 blocks=00002,00001 " +
    "00000=0,0,0/0 00001=19088,19088,18941/19035 00002=19088,18956,0,0/0",

  "twoClips/all":
    "reads=00011!,00022!,00033,00011!,00022!,00033 warn=00011,00022,00011,00022 " +
    "blocks=00000,00001,00002 00000=0,0,0/0 00001=0,0,0/0 00002=19088,18956,0,0/0",
  "twoClips/defectiveOnly":
    "reads=00011!,00033,00011!,00033 warn=00011,00011 blocks=00002 " +
    "00000=0,0,0/0 00001=0,0,0/0 00002=19088,18956,0,0/0",
  "twoClips/defectivePlusOne":
    "reads=00011!,00022!,00033,00011!,00022!,00033 warn=00011,00022,00011,00022 " +
    "blocks=00002,00001 00000=0,0,0/0 00001=0,0,0/0 00002=19088,18956,0,0/0",

  "earlierClip/all":
    "reads=00011,00022,00033!,00011,00022,00033! warn=00033,00033 blocks=00000,00001,00002 " +
    "00000=19088,19088,18956/19044 00001=19088,19088,18941/19035 " +
    "00002=721,0,18607,19000/19044",
  "earlierClip/defectiveOnly":
    "reads=00011,00033!,00011,00033! warn=00033,00033 blocks=00002 " +
    "00000=19088,19088,18956/19044 00001=0,0,0/0 00002=721,0,18607,19000/19044",
  "earlierClip/defectivePlusOne":
    "reads=00011,00022,00033!,00011,00022,00033! warn=00033,00033 blocks=00002,00001 " +
    "00000=19088,19088,18956/19044 00001=19088,19088,18941/19035 " +
    "00002=721,0,18607,19000/19044",

  "poisonedVolume/all":
    "reads=00011,00022,00033!,00011!,00022!,00033! warn=00033,00011,00022,00033 " +
    "blocks=00000,00001,00002 00000=0,0,0/0 00001=0,0,0/0 00002=0,0,0,0/0",
  "poisonedVolume/defectiveOnly":
    "reads=00011,00033!,00011!,00033! warn=00033,00011,00033 blocks=00002 " +
    "00000=0,0,0/0 00001=0,0,0/0 00002=0,0,0,0/0",
  "poisonedVolume/defectivePlusOne":
    "reads=00011,00022,00033!,00011!,00022!,00033! warn=00033,00011,00022,00033 " +
    "blocks=00002,00001 00000=0,0,0/0 00001=0,0,0/0 00002=0,0,0,0/0",

  "unreachedFault/all":
    "reads=00011,00022,00033,00011,00022,00033 warn= blocks=00000,00001,00002 " +
    "00000=19088,19088,18956/19044 00001=19088,19088,18941/19035 " +
    "00002=19088,19677,18607,19000/19035",
  "unreachedFault/defectiveOnly":
    "reads=00011,00033,00011,00033 warn= blocks=00002 " +
    "00000=19088,19088,18956/19044 00001=0,0,0/0 00002=19088,19677,18607,19000/19035",
  "unreachedFault/defectivePlusOne":
    "reads=00011,00022,00033,00011,00022,00033 warn= blocks=00002,00001 " +
    "00000=19088,19088,18956/19044 00001=19088,19088,18941/19035 " +
    "00002=19088,19677,18607,19000/19035",
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
  const wholeFileReads = [];
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

      const clipReads = readLog().filter((read) => read.name.endsWith(".m2ts"));
      wholeFileReads.push(
        ...clipReads.filter(
          (read) => read.start !== 0 || read.end !== CLIP_BYTES[read.name.toLowerCase()],
        ),
      );
      const reads = clipReads.map((read) => `${stem(read.name)}${read.failed ? "!" : ""}`);
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

  // Every clip is read whole, in one call, on both passes — the property that
  // makes a fault here void a file rather than truncate it, and the reason this
  // disc cannot exercise the keep-the-completed-chunks path a multi-GB clip has.
  if (wholeFileReads.length > 0) {
    failures.push(`clip reads were not whole-file: ${JSON.stringify(wholeFileReads)}`);
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

  // Re-rendering the disc model of a damaged subset scan is NOT the scan's own
  // report. The scan renders the selection the caller named; the re-render walks
  // every playlist the disc declares, in the presentation order of the selection
  // table (longest first, 00002 before 00000 here) — so the two playlists the
  // caller never selected come back as blocks of zeros beside the one it did.
  const rerendered = reportBlocks(render_report(subsetDisc));
  if (rerendered.join(",") !== "00002,00000,00001") {
    failures.push(
      `re-render blocks were ${rerendered.join(",")}, not the whole disc in table order`,
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
      `× ${Object.keys(SELECTIONS).length} selections) match their pinned shapes.`,
  );
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
