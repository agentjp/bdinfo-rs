// Node golden-parity test — no browser, no driver.
//
// Loads the BUILT, wasm-opt'd module (pkg/) straight into Node via `initSync`
// (the synchronous byte-init path, no fetch), shims the three browser globals the
// streaming export touches (`File`, `FileReaderSync`, plus `.size`/`.slice`), and
// drives the SAME production export the Worker uses — `scan_files` over a
// `(relativePath, File)` list built from the committed Big Buck Bunny BD-ROM
// fixture. It then asserts the rendered report is BYTE-IDENTICAL to the pinned
// golden (`tests/golden_report.txt`) — the crate's own golden, rendered from the
// same Big Buck Bunny fixture the native CLI e2e test scans and pinned by the
// native and in-browser parity tests alike. So this ties the wasm channel to the
// locked-output contract on every gate run, with only Node + the built wasm.
//
// The same golden also pins the round trip through the structured disc model:
// the `disc` that `scan_files_full` returns beside the report, handed straight
// back to `render_report`, must render those bytes again — so the model reaches
// JavaScript and comes back carrying every value the report prints.
//
// Prereq: `npm run build` (emits pkg/). Run with `npm run test:node`.

import { readFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

// --- browser-global shims (synchronous, Worker-equivalent) -------------------

/** A minimal synchronous `Blob`: a byte window with `size` and `slice`. */
class ShimBlob {
  constructor(bytes) {
    this._bytes = bytes;
  }
  get size() {
    return this._bytes.length;
  }
  slice(start, end) {
    return new ShimBlob(this._bytes.subarray(start, end));
  }
}

/** A `File` over a byte buffer — what the wasm `instanceof File` check sees. */
class ShimFile extends ShimBlob {
  constructor(bytes, name) {
    super(bytes);
    this.name = name;
  }
}

/** `FileReaderSync.readAsArrayBuffer` — the synchronous byte read the seam needs. */
class ShimFileReaderSync {
  readAsArrayBuffer(blob) {
    const b = blob._bytes;
    return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
  }
}

globalThis.File = ShimFile;
globalThis.FileReaderSync = ShimFileReaderSync;

// --- paths -------------------------------------------------------------------

const here = dirname(fileURLToPath(import.meta.url));
const fixtures = resolve(here, "../../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV");
const goldenPath = resolve(here, "../../tests/golden_report.txt");
const wasmPath = resolve(here, "../pkg/bdinfo_rs_wasm_bg.wasm");
// The same disc as a UDF `.iso`, and its native `.iso` golden (the report
// `bdinfo-rs <disc>.iso` writes). The only line that differs from the folder
// golden is `Disc Label:` — an `.iso` reads the real UDF volume label `Blu-Ray`.
const isoPath = resolve(here, "../../../bdinfo-rs/tests/fixtures/BigBuckBunny.iso");
const isoGoldenPath = resolve(here, "../../../bdinfo-rs/tests/fixtures/golden/iso.txt");

// The fixture's six files at the synthetic disc paths the golden was built from:
// root `WASMDISC` → disc label `WASMDISC`. `bdmt_eng.xml` is empty, mirroring the
// in-memory parity blob and the headless-browser test's layout.
const LAYOUT = [
  { path: "WASMDISC/BDMV/index.bdmv", file: join(fixtures, "index.bdmv") },
  { path: "WASMDISC/BDMV/MovieObject.bdmv", file: join(fixtures, "MovieObject.bdmv") },
  { path: "WASMDISC/BDMV/PLAYLIST/00000.mpls", file: join(fixtures, "PLAYLIST/00000.mpls") },
  { path: "WASMDISC/BDMV/CLIPINF/00000.clpi", file: join(fixtures, "CLIPINF/00000.clpi") },
  { path: "WASMDISC/BDMV/STREAM/00000.m2ts", file: join(fixtures, "STREAM/00000.m2ts") },
  { path: "WASMDISC/BDMV/META/DL/bdmt_eng.xml", file: null },
];

/**
 * The fixture playlist patched to a ~10 s duration, so the short-playlist filter
 * withholds it. The first PlayItem's OUT_time is a u32-BE at file offset 86
 * (IN_time is 27_000_000 at 82) and a second is 45_000 ticks.
 */
function shortPlaylist(mpls) {
  const bytes = new Uint8Array(mpls);
  new DataView(bytes.buffer).setUint32(86, 27_000_000 + 45_000 * 10);
  return bytes;
}

async function main() {
  const golden = await readFile(goldenPath);

  const {
    initSync,
    scan_files,
    list_playlists,
    scan_iso,
    list_iso_playlists,
    inspect_files,
    inspect_iso,
    scan_files_full,
    scan_iso_full,
    render_report,
  } = await import("../pkg/bdinfo_rs_wasm.js");
  initSync({ module: await readFile(wasmPath) });

  const paths = [];
  const files = [];
  for (const item of LAYOUT) {
    const bytes =
      item.file === null ? new Uint8Array(0) : new Uint8Array(await readFile(item.file));
    const name = item.path.split("/").pop();
    paths.push(item.path);
    files.push(new ShimFile(bytes, name));
  }

  const report = scan_files(paths, files, []);
  const got = Buffer.from(report, "utf8");

  // The new CLI-parity exports: the structural list, and a by-name selective
  // scan. On this single-playlist fixture, selecting the only playlist measures
  // the same bytes as `--whole`, so its report must equal the golden too.
  const rows = JSON.parse(list_playlists(paths, files));
  const selReport = Buffer.from(scan_files(paths, files, ["00000.MPLS"]), "utf8");
  const listOk =
    Array.isArray(rows) &&
    rows.length === 1 &&
    rows[0].name === "00000.MPLS" &&
    rows[0].position === 1 &&
    Array.isArray(rows[0].hiddenBy) &&
    rows[0].hiddenBy.length === 0;
  const selOk = selReport.equals(golden);
  if (!listOk) {
    console.error(`FAIL — list_playlists rows unexpected: ${JSON.stringify(rows)}`);
  }

  // The filter options: the same disc plus a second playlist over the same clip,
  // patched to ~10 s so the short rule withholds it. Omitting the options (and
  // passing `false`) must list only the feature; passing `show_short_playlists`
  // must add the short playlist, tagged `hiddenBy: ["short"]`.
  const mpls = new Uint8Array(await readFile(join(fixtures, "PLAYLIST/00000.mpls")));
  const shortPaths = [...paths, "WASMDISC/BDMV/PLAYLIST/00001.mpls"];
  const shortFiles = [...files, new ShimFile(shortPlaylist(mpls), "00001.mpls")];
  const listNames = (...options) =>
    JSON.parse(list_playlists(shortPaths, shortFiles, ...options)).map((row) => row.name);
  const widened = JSON.parse(list_playlists(shortPaths, shortFiles, true));
  const optionsOk =
    JSON.stringify(listNames()) === '["00000.MPLS"]' &&
    JSON.stringify(listNames(false, false)) === '["00000.MPLS"]' &&
    JSON.stringify(listNames(false, true)) === '["00000.MPLS"]' &&
    JSON.stringify(widened.map((row) => row.name)) === '["00000.MPLS","00001.MPLS"]' &&
    JSON.stringify(widened.map((row) => row.hiddenBy)) === '[[],["short"]]';
  if (!optionsOk) {
    console.error(
      `FAIL — filter options unexpected: ${JSON.stringify(listNames())} / ${JSON.stringify(widened)}`,
    );
  }

  // The structural disc model. `inspect_files` returns the whole scanned model
  // as a real JavaScript object — properties read directly, no JSON.parse — so
  // this is also what proves the mirror crosses the boundary at all. `measured`
  // is false because no packet demux ran, and every value that only a demux
  // could fill is therefore zero.
  const inspected = inspect_files(paths, files);
  const inspectedPlaylist = inspected.playlists[0];
  const inspectOk =
    inspected.measured === false &&
    inspected.volumeLabel === "WASMDISC" &&
    inspected.sizeBytes === 11146324 &&
    inspected.errors.length === 0 &&
    inspected.playlists.length === 1 &&
    inspectedPlaylist.name === "00000.MPLS" &&
    inspectedPlaylist.streams.length === 2 &&
    inspectedPlaylist.streams[0].codecName === "MPEG-4 AVC Video" &&
    inspectedPlaylist.clips.length === 1 &&
    inspectedPlaylist.clips[0].name === "00000.M2TS" &&
    inspectedPlaylist.streams.every((stream) => stream.bitrateBps === 0);
  if (!inspectOk) {
    console.error(`FAIL — inspect_files disc unexpected: ${JSON.stringify(inspected)}`);
  }

  // The short-playlist threshold, which only the inspect exports take: the
  // ~10 s playlist the default 20 s rule withholds is listed once the threshold
  // drops below its length, and a zero threshold means the default.
  const inspectNames = (...options) =>
    inspect_files(shortPaths, shortFiles, ...options).playlists.map((playlist) => playlist.name);
  const thresholdOk =
    JSON.stringify(inspectNames()) === '["00000.MPLS"]' &&
    JSON.stringify(inspectNames(false, false, 0)) === '["00000.MPLS"]' &&
    JSON.stringify(inspectNames(false, false, 5)) === '["00000.MPLS","00001.MPLS"]';
  if (!thresholdOk) {
    console.error(
      `FAIL — short-playlist threshold unexpected: ${JSON.stringify(inspectNames(false, false, 5))}`,
    );
  }
  if (!selOk) {
    console.error(
      `FAIL — selective scan (${selReport.length} bytes) diverged from golden (${golden.length} bytes).`,
    );
  }

  // The measured scan that returns both outputs, and the round trip back. One
  // scan produces the report and the disc; feeding that disc straight back to
  // `render_report` must reproduce the same bytes — which is what proves the
  // model crossed to JavaScript and back without losing a value the report
  // prints. Switching a section off must then drop it and nothing else.
  const full = scan_files_full(paths, files, []);
  const reRendered = Buffer.from(render_report(full.disc), "utf8");
  const noDiagnostics = render_report(full.disc, false);
  const noSummary = render_report(full.disc, true, false);
  const fullOk =
    Buffer.from(full.report, "utf8").equals(golden) &&
    full.disc.measured === true &&
    full.disc.playlists[0].streams[0].bitrateBps > 0 &&
    reRendered.equals(golden) &&
    !noDiagnostics.includes("STREAM DIAGNOSTICS:") &&
    noDiagnostics.includes("QUICK SUMMARY:") &&
    noSummary.includes("STREAM DIAGNOSTICS:") &&
    !noSummary.includes("QUICK SUMMARY:");
  if (!fullOk) {
    console.error(
      `FAIL — scan_files_full/render_report round trip: report ${full.report.length} B, re-rendered ${reRendered.length} B, golden ${golden.length} B, measured ${full.disc.measured}.`,
    );
  }

  // The streaming `.iso` path: the same disc opened through the UDF reader as one
  // `File`. scan_iso (whole + by-name) and list_iso_playlists must match the
  // native `.iso` golden / table, exercising WebIso's FileReaderSync windowed
  // reads through the same shims scan_files uses.
  const isoGolden = await readFile(isoGoldenPath);
  const isoFile = new ShimFile(new Uint8Array(await readFile(isoPath)), "BigBuckBunny.iso");
  const isoReport = Buffer.from(scan_iso(isoFile, []), "utf8");
  const isoRows = JSON.parse(list_iso_playlists(isoFile));
  const isoSelReport = Buffer.from(scan_iso(isoFile, ["00000.MPLS"]), "utf8");
  const isoOk = isoReport.equals(isoGolden);
  const isoListOk =
    Array.isArray(isoRows) &&
    isoRows.length === 1 &&
    isoRows[0].name === "00000.MPLS" &&
    isoRows[0].position === 1 &&
    Array.isArray(isoRows[0].hiddenBy) &&
    isoRows[0].hiddenBy.length === 0;
  const isoSelOk = isoSelReport.equals(isoGolden);
  if (!isoListOk) {
    console.error(`FAIL — list_iso_playlists rows unexpected: ${JSON.stringify(isoRows)}`);
  }
  // The disc model over the same image: the UDF volume label is the genuine
  // one recorded in the filesystem, where the folder pick above can only use
  // the picked folder name.
  const isoInspected = inspect_iso(isoFile);
  const isoInspectOk =
    isoInspected.measured === false &&
    isoInspected.volumeLabel === "Blu-Ray" &&
    isoInspected.playlists.length === 1 &&
    isoInspected.playlists[0].name === "00000.MPLS" &&
    isoInspected.playlists[0].clips.length === 1;
  if (!isoInspectOk) {
    console.error(`FAIL — inspect_iso disc unexpected: ${JSON.stringify(isoInspected)}`);
  }
  // The same both-outputs scan over the image, round-tripped the same way.
  const isoFull = scan_iso_full(isoFile, []);
  const isoFullOk =
    Buffer.from(isoFull.report, "utf8").equals(isoGolden) &&
    isoFull.disc.measured === true &&
    isoFull.disc.volumeLabel === "Blu-Ray" &&
    Buffer.from(render_report(isoFull.disc), "utf8").equals(isoGolden);
  if (!isoFullOk) {
    console.error(
      `FAIL — scan_iso_full/render_report round trip: report ${isoFull.report.length} B, iso golden ${isoGolden.length} B.`,
    );
  }
  if (!isoSelOk) {
    console.error(
      `FAIL — selective .iso scan (${isoSelReport.length} bytes) diverged from the iso golden (${isoGolden.length} bytes).`,
    );
  }
  if (!isoOk) {
    console.error(
      `FAIL — .iso scan (${isoReport.length} bytes) diverged from the iso golden (${isoGolden.length} bytes).`,
    );
    const lim = Math.min(isoReport.length, isoGolden.length);
    for (let i = 0; i < lim; i++) {
      if (isoReport[i] !== isoGolden[i]) {
        const ctx = (buf) =>
          JSON.stringify(buf.slice(Math.max(0, i - 30), i + 30).toString("utf8"));
        console.error(`  first .iso diff at byte ${i}:`);
        console.error(`    golden: ${ctx(isoGolden)}`);
        console.error(`    got:    ${ctx(isoReport)}`);
        break;
      }
    }
  }

  if (
    got.equals(golden) &&
    listOk &&
    selOk &&
    optionsOk &&
    inspectOk &&
    thresholdOk &&
    fullOk &&
    isoOk &&
    isoListOk &&
    isoSelOk &&
    isoInspectOk &&
    isoFullOk
  ) {
    console.log(
      `PASS — Node measured scan matches the golden (${golden.length} bytes); list + options + inspect + selection + round trip + .iso OK.`,
    );
    process.exit(0);
  }

  console.error(
    `FAIL — report (${got.length} bytes) diverged from golden (${golden.length} bytes).`,
  );
  const limit = Math.min(got.length, golden.length);
  for (let i = 0; i < limit; i++) {
    if (got[i] !== golden[i]) {
      const ctx = (buf) => JSON.stringify(buf.slice(Math.max(0, i - 30), i + 30).toString("utf8"));
      console.error(`  first diff at byte ${i}:`);
      console.error(`    golden: ${ctx(golden)}`);
      console.error(`    got:    ${ctx(got)}`);
      break;
    }
  }
  process.exit(1);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
