// Node golden-parity test — no browser, no driver.
//
// Loads the BUILT, wasm-opt'd module (pkg/) straight into Node via `initSync`
// (the synchronous byte-init path, no fetch), installs the browser globals the
// streaming export touches (`shims.mjs`: `File`, `FileReaderSync`, plus
// `.size`/`.slice`), and drives the SAME production export the Worker uses —
// `scan_files` over a `(relativePath, File)` list built from the committed Big
// Buck Bunny BD-ROM fixture.
//
// It then asserts the rendered report is BYTE-IDENTICAL to the pinned
// golden (`tests/golden_report.txt`) — the crate's own golden, rendered from the
// same Big Buck Bunny fixture the native CLI e2e test scans and pinned by the
// native and in-browser parity tests alike. So this ties the wasm channel to the
// locked-output contract on every gate run, with only Node + the built wasm.
//
// The same golden also pins the round trip through the structured disc model:
// the `disc` that `scan_files` returns beside the report, handed straight back
// to `render_report`, must render those bytes again — so the model reaches
// JavaScript and comes back carrying every value the report prints.
//
// It also asserts the demo's size-cell formatter (src/format.ts) against the
// shared vector table the desktop app asserts in Rust — the one place where a
// hand-written formatter exists twice, once per language.
//
// Prereq: `npm run build` (emits pkg/ and dist/). Run with `npm run test:node`.

import { readFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { installShims, ShimFile } from "./shims.mjs";

// --- browser-global shims (synchronous, Worker-equivalent) -------------------

// No fault policy is installed, so every read below serves its bytes.
// `faults.node.mjs` drives the same shims through reads that throw.
installShims();

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
// The byte-size vectors the desktop app asserts too — the two size formatters
// are hand-written once per language and cannot share code, so this table is
// what keeps them from drifting apart. Its own header documents the columns.
const sizeVectorsPath = resolve(here, "../../../bdinfo-rs-gui/tests/size-vectors.tsv");

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
    scan_iso,
    inspect_files,
    inspect_iso,
    render_report,
    report_file_name,
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

  const full = scan_files(paths, files, []);
  const got = Buffer.from(full.report, "utf8");

  // A by-name selective scan. On this single-playlist fixture, selecting the
  // only playlist measures the same bytes as `--whole`, so its report must
  // equal the golden too.
  const selReport = Buffer.from(scan_files(paths, files, ["00000.MPLS"]).report, "utf8");
  const selOk = selReport.equals(golden);
  if (!selOk) {
    console.error(
      `FAIL — selective scan (${selReport.length} bytes) diverged from golden (${golden.length} bytes).`,
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

  // The four selection-table fields on the model. The fixture disc is one
  // ~30 s playlist, so it is group 1, position 1, withheld by nothing, and its
  // tick count divides exactly into the `00:00:30` the table cell prints.
  const tableOk =
    inspectedPlaylist.group === 1 &&
    inspectedPlaylist.position === 1 &&
    Array.isArray(inspectedPlaylist.hiddenBy) &&
    inspectedPlaylist.hiddenBy.length === 0 &&
    inspectedPlaylist.totalLengthTicks ===
      Math.trunc(inspectedPlaylist.totalLengthSeconds * 10_000_000) &&
    Math.trunc(inspectedPlaylist.totalLengthTicks / 10_000_000) === 30;
  if (!tableOk) {
    console.error(
      `FAIL — selection-table fields unexpected: group ${inspectedPlaylist.group}, position ${inspectedPlaylist.position}, hiddenBy ${JSON.stringify(inspectedPlaylist.hiddenBy)}, ticks ${inspectedPlaylist.totalLengthTicks}.`,
    );
  }

  // The short-playlist threshold and the classification it drives. The disc
  // gains a second playlist over the same clip, patched to ~10 s: every export
  // returns BOTH playlists whatever the threshold, and only `hiddenBy` moves.
  // Sharing the clip also puts both in group 1, longest first.
  const mpls = new Uint8Array(await readFile(join(fixtures, "PLAYLIST/00000.mpls")));
  const shortPaths = [...paths, "WASMDISC/BDMV/PLAYLIST/00001.mpls"];
  const shortFiles = [...files, new ShimFile(shortPlaylist(mpls), "00001.mpls")];
  const classify = (playlists) =>
    JSON.stringify(playlists.map((playlist) => `${playlist.name}:${playlist.hiddenBy}`));
  const numbering = (playlists) =>
    JSON.stringify(playlists.map((playlist) => [playlist.group, playlist.position]));
  const standard = inspect_files(shortPaths, shortFiles).playlists;
  const lowered = inspect_files(shortPaths, shortFiles, { shortPlaylistSeconds: 5 }).playlists;
  const measuredShort = scan_files(shortPaths, shortFiles, [], undefined, {
    streamDiagnostics: true,
    quickSummary: true,
    shortPlaylistSeconds: 5,
  }).disc;
  const thresholdOk =
    classify(standard) === '["00000.MPLS:","00001.MPLS:short"]' &&
    // A zero threshold switches the short rule off: nothing is shorter than
    // zero seconds, so neither playlist is withheld.
    classify(inspect_files(shortPaths, shortFiles, { shortPlaylistSeconds: 0 }).playlists) ===
      '["00000.MPLS:","00001.MPLS:"]' &&
    classify(lowered) === '["00000.MPLS:","00001.MPLS:"]' &&
    numbering(standard) === "[[1,1],[1,2]]" &&
    numbering(lowered) === "[[1,1],[1,2]]" &&
    // The measured export applies the same threshold, so a `Disc` means the
    // same thing whichever call produced it.
    classify(measuredShort.playlists) === '["00000.MPLS:","00001.MPLS:"]';
  if (!thresholdOk) {
    console.error(
      `FAIL — classification unexpected: standard ${classify(standard)}, lowered ${classify(lowered)}, numbering ${numbering(standard)}, measured ${classify(measuredShort.playlists)}.`,
    );
  }

  // An out-of-domain threshold throws — from the structural and the measured
  // export alike — instead of silently scanning with the 20 s default.
  const rejects = (run) => {
    try {
      run();
      return false;
    } catch (error) {
      return String(error.message ?? error).includes("shortPlaylistSeconds");
    }
  };
  const rejectionOk =
    rejects(() => inspect_files(paths, files, { shortPlaylistSeconds: -1 })) &&
    rejects(() => inspect_files(paths, files, { shortPlaylistSeconds: 86401 })) &&
    rejects(() => inspect_files(paths, files, { shortPlaylistSeconds: Number.NaN })) &&
    rejects(() => scan_files(paths, files, [], undefined, { shortPlaylistSeconds: -1 }));
  if (!rejectionOk) {
    console.error("FAIL — an out-of-domain shortPlaylistSeconds did not throw.");
  }

  // The codecs depth: an inspect that reads each stream file's head. The video
  // stream's description gains its profile/level while the disc stays
  // unmeasured (the LPCM rate is parameter-declared, so it may fill in).
  const codecsDisc = inspect_files(paths, files, { codecs: true });
  const codecsVideo = codecsDisc.playlists[0].streams[0];
  const plainVideo = inspected.playlists[0].streams[0];
  const codecsOk =
    codecsDisc.measured === false &&
    codecsVideo.bitrateBps === 0 &&
    codecsVideo.fullDescription.includes("High Profile 4.1") &&
    !plainVideo.fullDescription.includes("Profile");
  if (!codecsOk) {
    console.error(
      `FAIL — codecs inspect unexpected: measured ${codecsDisc.measured}, video "${codecsVideo.fullDescription}", plain "${plainVideo.fullDescription}".`,
    );
  }

  // The demo's size cells against the shared vector table, columns 4 and 5 (the
  // desktop app asserts columns 2 and 3 of the same rows). A row count is
  // asserted too: a badly parsed table would check nothing and still pass.
  const { sizeCell } = await import("../dist/format.js");
  const vectors = (await readFile(sizeVectorsPath, "utf8"))
    .split(/\r?\n/)
    .filter((line) => line.length > 0 && !line.startsWith("#"))
    .map((line) => line.split("\t"));
  const sizeMismatches = vectors.flatMap(([bytes, , , exact, human]) => {
    const value = Number(bytes);
    const got = [sizeCell(value, false), sizeCell(value, true)];
    return got[0] === exact && got[1] === human
      ? []
      : [`${bytes}: got ${JSON.stringify(got)}, want ${JSON.stringify([exact, human])}`];
  });
  const sizeOk = vectors.length === 14 && sizeMismatches.length === 0;
  if (!sizeOk) {
    console.error(
      `FAIL — size cells diverged from the shared vector table (${vectors.length} vectors): ${sizeMismatches.join("; ")}`,
    );
  }

  // The report save-file name, sanitized by the core rule.
  const fileNameOk =
    report_file_name("WASMDISC") === "BDINFO.WASMDISC.txt" &&
    report_file_name("a/b:c") === "BDINFO.a_b_c.txt";
  if (!fileNameOk) {
    console.error(
      `FAIL — report_file_name unexpected: ${report_file_name("WASMDISC")}, ${report_file_name("a/b:c")}.`,
    );
  }

  // The measured scan that returns both outputs, and the round trip back. One
  // scan produces the report and the disc; feeding that disc straight back to
  // `render_report` must reproduce the same bytes — which is what proves the
  // model crossed to JavaScript and back without losing a value the report
  // prints. Switching a section off must then drop it and nothing else.
  const reRendered = Buffer.from(render_report(full.disc), "utf8");
  const noDiagnostics = render_report(full.disc, { streamDiagnostics: false });
  const noSummary = render_report(full.disc, { quickSummary: false });
  const fullOk =
    full.disc.measured === true &&
    full.disc.playlists[0].streams[0].bitrateBps > 0 &&
    reRendered.equals(golden) &&
    !noDiagnostics.includes("STREAM DIAGNOSTICS:") &&
    noDiagnostics.includes("QUICK SUMMARY:") &&
    noSummary.includes("STREAM DIAGNOSTICS:") &&
    !noSummary.includes("QUICK SUMMARY:");

  // Retention is about a stream file that fails to read, which this healthy
  // fixture never does — so switching it off must leave the locked bytes alone.
  // What the switch does when a read DOES fail is pinned in the library.
  const dropped = Buffer.from(
    scan_files(paths, files, [], undefined, { keepPartial: false }).report,
    "utf8",
  );
  const keepPartialOk = dropped.equals(golden);
  if (!keepPartialOk) {
    console.error(
      `FAIL — keepPartial: false changed a healthy scan (${dropped.length} bytes vs golden ${golden.length}).`,
    );
  }
  if (!fullOk) {
    console.error(
      `FAIL — scan_files/render_report round trip: report ${full.report.length} B, re-rendered ${reRendered.length} B, golden ${golden.length} B, measured ${full.disc.measured}.`,
    );
  }

  // The streaming `.iso` path: the same disc opened through the UDF reader as one
  // `File`. scan_iso (whole + by-name) must match the native `.iso` golden,
  // exercising WebIso's FileReaderSync windowed reads through the same shims
  // scan_files uses.
  const isoGolden = await readFile(isoGoldenPath);
  const isoFile = new ShimFile(new Uint8Array(await readFile(isoPath)), "BigBuckBunny.iso");
  const isoFull = scan_iso(isoFile, []);
  const isoReport = Buffer.from(isoFull.report, "utf8");
  const isoSelReport = Buffer.from(scan_iso(isoFile, ["00000.MPLS"]).report, "utf8");
  const isoOk = isoReport.equals(isoGolden);
  const isoSelOk = isoSelReport.equals(isoGolden);
  // The disc model over the same image: the UDF volume label is the genuine
  // one recorded in the filesystem, where the folder pick above can only use
  // the picked folder name.
  const isoInspected = inspect_iso(isoFile);
  const isoInspectOk =
    isoInspected.measured === false &&
    isoInspected.volumeLabel === "Blu-Ray" &&
    isoInspected.playlists.length === 1 &&
    isoInspected.playlists[0].name === "00000.MPLS" &&
    isoInspected.playlists[0].position === 1 &&
    isoInspected.playlists[0].hiddenBy.length === 0 &&
    isoInspected.playlists[0].clips.length === 1;
  if (!isoInspectOk) {
    console.error(`FAIL — inspect_iso disc unexpected: ${JSON.stringify(isoInspected)}`);
  }
  // The both-outputs scan over the image, round-tripped the same way.
  const isoFullOk =
    isoOk &&
    isoFull.disc.measured === true &&
    isoFull.disc.volumeLabel === "Blu-Ray" &&
    Buffer.from(render_report(isoFull.disc), "utf8").equals(isoGolden);
  if (!isoFullOk) {
    console.error(
      `FAIL — scan_iso/render_report round trip: report ${isoFull.report.length} B, iso golden ${isoGolden.length} B.`,
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
    selOk &&
    inspectOk &&
    tableOk &&
    thresholdOk &&
    rejectionOk &&
    codecsOk &&
    sizeOk &&
    fileNameOk &&
    fullOk &&
    keepPartialOk &&
    isoOk &&
    isoSelOk &&
    isoInspectOk &&
    isoFullOk
  ) {
    console.log(
      `PASS — Node measured scan matches the golden (${golden.length} bytes); inspect + table fields + classification + rejection + codecs + size vectors + file name + selection + round trip + retention + .iso OK.`,
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
