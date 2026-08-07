// Headless-Chrome Worker parity test.
//
// Serves the built package over HTTP, opens it in headless Chrome (the system
// Google Chrome, via Playwright's `channel: "chrome"`), hands the page the
// committed Big Buck Bunny BD-ROM fixture as in-browser `File` objects, runs the
// FULL measured scan in the Worker (the `FileReaderSync` byte-offset path), and
// asserts the returned report is BYTE-IDENTICAL to the pinned native golden
// (`tests/golden_report.txt`) — the same golden the native ⇄ wasm parity test pins.
//
// It drives the package's own entry (`dist/analyze.js`), so it is also what
// proves the TypeScript surface: the disc model reaches the page as a real
// object over `postMessage`, and handing it back to `renderReport` renders the
// golden bytes again — the round trip through structured clone in both
// directions, which the Node test (raw wasm exports, no Worker) cannot see.
//
// It then drives the demo page itself (`index.html` + `dist/demo.js`, served
// the same static way `dev.mjs` serves it): the fixture goes in through the
// real folder picker, and the master-detail behaviour — pane population from
// the active row, header-click sorting, the measured cells filling after a
// scan — is asserted in the page. That is the demo's only executable check:
// biome and tsc prove nothing about behaviour.
//
// Prereq: `npm run build` (emits `pkg/` + `dist/`). Run with `npm run test:chrome`.

import { readFile } from "node:fs/promises";
import { createServer } from "node:http";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright-core";

const here = dirname(fileURLToPath(import.meta.url));
const webRoot = resolve(here, ".."); // crates/bdinfo-rs-wasm/web
const fixtures = resolve(here, "../../../bdinfo-rs/tests/fixtures/BigBuckBunny/BDMV");
const goldenPath = resolve(here, "../../tests/golden_report.txt");
// The same disc as a UDF `.iso` + its native `.iso` golden. The image lives
// outside webRoot, so the server serves it from one fixed route (`/__fixture.iso`)
// that the page `fetch`es into a `File` — no multi-MB base64 round-trip.
const isoPath = resolve(here, "../../../bdinfo-rs/tests/fixtures/BigBuckBunny.iso");
const isoGoldenPath = resolve(here, "../../../bdinfo-rs/tests/fixtures/golden/iso.txt");

// The fixture's six files at the synthetic disc paths the in-memory golden was
// built from: root `WASMDISC` → disc label `WASMDISC`. `bdmt_eng.xml` is empty,
// mirroring the in-memory parity blob.
const LAYOUT = [
  { path: "WASMDISC/BDMV/index.bdmv", file: join(fixtures, "index.bdmv") },
  { path: "WASMDISC/BDMV/MovieObject.bdmv", file: join(fixtures, "MovieObject.bdmv") },
  { path: "WASMDISC/BDMV/PLAYLIST/00000.mpls", file: join(fixtures, "PLAYLIST/00000.mpls") },
  { path: "WASMDISC/BDMV/CLIPINF/00000.clpi", file: join(fixtures, "CLIPINF/00000.clpi") },
  { path: "WASMDISC/BDMV/STREAM/00000.m2ts", file: join(fixtures, "STREAM/00000.m2ts") },
  { path: "WASMDISC/BDMV/META/DL/bdmt_eng.xml", file: null },
];

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json; charset=utf-8",
};

function startServer() {
  const server = createServer(async (req, res) => {
    try {
      const urlPath = decodeURIComponent((req.url ?? "/").split("?")[0]);
      // The `.iso` fixture lives outside webRoot; serve it from one fixed route.
      if (urlPath === "/__fixture.iso") {
        res.writeHead(200, { "content-type": "application/octet-stream" });
        res.end(await readFile(isoPath));
        return;
      }
      const safe = join(webRoot, urlPath).replace(/[\\/]+$/, "");
      if (!safe.startsWith(webRoot)) {
        res.writeHead(403).end();
        return;
      }
      const body = await readFile(safe);
      res.writeHead(200, { "content-type": MIME[extname(safe)] ?? "application/octet-stream" });
      res.end(body);
    } catch {
      res.writeHead(404).end();
    }
  });
  return new Promise((ok) => {
    server.listen(0, "127.0.0.1", () => ok(server));
  });
}

async function main() {
  const golden = await readFile(goldenPath);
  const isoGolden = await readFile(isoGoldenPath);

  // Read fixture bytes and base64-frame them for the in-page File construction.
  const entries = [];
  for (const item of LAYOUT) {
    const bytes = item.file === null ? Buffer.alloc(0) : await readFile(item.file);
    entries.push({ path: item.path, b64: bytes.toString("base64") });
  }

  const server = await startServer();
  const { port } = server.address();
  const base = `http://127.0.0.1:${port}`;

  const browser = await chromium.launch({
    channel: "chrome",
    headless: true,
    args: ["--no-sandbox"],
  });

  let listings;
  let structured;
  let isoStructured;
  let demo;
  try {
    const page = await browser.newPage();
    page.on("console", (msg) => console.log(`  [page] ${msg.text()}`));
    page.on("pageerror", (err) => console.log(`  [pageerror] ${err.message}`));

    await page.goto(`${base}/test/harness.html`);
    await page.waitForFunction(() => window.__ready === true, { timeout: 30000 });

    // The in-page `(relativePath, File)` list the folder calls take.
    await page.evaluate((items) => {
      window.__files = items.map((item) => {
        const binary = atob(item.b64);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) {
          bytes[i] = binary.charCodeAt(i);
        }
        const name = item.path.split("/").pop();
        return { path: item.path, file: new File([bytes], name) };
      });
    }, entries);

    // The playlist classification, driven through the real Worker: the same
    // disc plus a second playlist over the same clip patched to ~10 s (the first
    // PlayItem's OUT_time is a u32-BE at file offset 86, 45_000 ticks a second),
    // which the short-playlist rule withholds until the threshold drops below
    // its length. Both playlists are always returned; only `hiddenBy` moves.
    // Sharing a clip also puts them in one group, longest first.
    listings = await page.evaluate(async () => {
      const source = window.__files.find((entry) => entry.path.endsWith("00000.mpls"));
      const bytes = new Uint8Array(await source.file.arrayBuffer());
      new DataView(bytes.buffer).setUint32(86, 27_000_000 + 45_000 * 10);
      const files = [
        ...window.__files,
        {
          path: "WASMDISC/BDMV/PLAYLIST/00001.mpls",
          file: new File([bytes], "00001.mpls"),
        },
      ];
      const rules = (playlists) =>
        playlists.map((playlist) => `${playlist.name}:${playlist.hiddenBy}`);
      const inspected = async (options) =>
        rules((await window.__inspect(files, options)).playlists);
      return {
        standard: await inspected(),
        lowered: await inspected({ shortPlaylistSeconds: 5 }),
        // The measured export takes the same threshold, so a disc means the
        // same thing whichever call produced it.
        measured: rules(
          (await window.__scan(files, undefined, { shortPlaylistSeconds: 5 })).disc.playlists,
        ),
        numbering: (await window.__inspect(files)).playlists.map(
          (playlist) => `${playlist.group}/${playlist.position}`,
        ),
      };
    });

    // The structured API over the folder pick: the model out of a structural
    // scan, then a measured scan that returns the report and the model together,
    // then that model handed back for a render with and without a section.
    structured = await page.evaluate(async () => {
      const disc = await window.__inspect(window.__files);
      const scanned = await window.__scan(window.__files);
      return {
        inspected: {
          measured: disc.measured,
          volumeLabel: disc.volumeLabel,
          playlists: disc.playlists.map((playlist) => playlist.name),
          streams: disc.playlists[0].streams.length,
          rates: disc.playlists[0].streams.map((stream) => stream.bitrateBps),
          group: disc.playlists[0].group,
          position: disc.playlists[0].position,
          hiddenBy: disc.playlists[0].hiddenBy,
          ticks: disc.playlists[0].totalLengthTicks,
        },
        measured: scanned.disc.measured,
        rate: scanned.disc.playlists[0].streams[0].bitrateBps,
        report: scanned.report,
        reRendered: await window.__renderReport(scanned.disc),
        trimmed: await window.__renderReport(scanned.disc, { streamDiagnostics: false }),
      };
    });

    // The `.iso` path: the same disc fetched as one `File` and opened through
    // the UDF reader — the real-browser Worker + FileReaderSync seam. One `File`
    // is what tells `inspect` and `scan` to open it as an `.iso`, so this is the
    // check that the two sources are told apart; the volume label is the genuine
    // one recorded in the filesystem, where a folder pick can only use the
    // picked folder's name.
    isoStructured = await page.evaluate(async () => {
      const buf = await (await fetch("/__fixture.iso")).arrayBuffer();
      const file = new File([new Uint8Array(buf)], "BigBuckBunny.iso");
      const disc = await window.__inspect(file);
      const scanned = await window.__scan(file);
      return {
        label: disc.volumeLabel,
        playlists: disc.playlists.map((playlist) => playlist.name),
        measured: scanned.disc.measured,
        report: scanned.report,
        reRendered: await window.__renderReport(scanned.disc),
      };
    });
    // The demo page, end to end. A second playlist (the same clip, OUT_time
    // patched to ~40 s the same way as above, plus a second chapter mark so the
    // chapter-count suffix has a playlist to appear on) makes the table two
    // rows, so ordering is observable; a third playlist patched to ~10 s is
    // withheld by the short rule at the default threshold, so the filter
    // toggles and the threshold have something to reveal. A constructed `File`
    // has an empty read-only `webkitRelativePath`; an own property carries the
    // folder path the demo's picker handler reads.
    const demoPage = await browser.newPage();
    demoPage.on("console", (msg) => console.log(`  [demo] ${msg.text()}`));
    demoPage.on("pageerror", (err) => console.log(`  [demo pageerror] ${err.message}`));
    await demoPage.goto(`${base}/index.html`);
    await demoPage.evaluate((items) => {
      // Record every request the page posts to a scan Worker, so the checks
      // below can assert which settings cost a wasm call and which cost none.
      const NativeWorker = window.Worker;
      window.__calls = [];
      window.Worker = class extends NativeWorker {
        postMessage(message, ...rest) {
          if (typeof message === "object" && message !== null && "kind" in message) {
            window.__calls.push(message.kind);
          }
          super.postMessage(message, ...rest);
        }
      };
      const decode = (b64) => {
        const binary = atob(b64);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) {
          bytes[i] = binary.charCodeAt(i);
        }
        return bytes;
      };
      const toFile = (path, bytes) => {
        const file = new File([bytes], path.split("/").pop());
        Object.defineProperty(file, "webkitRelativePath", { value: path });
        return file;
      };
      const withLength = (bytes, seconds) => {
        const out = bytes.slice();
        new DataView(out.buffer).setUint32(86, 27_000_000 + 45_000 * seconds);
        return out;
      };
      // Appends a copy of the playlist's single chapter mark, 10 s later: the
      // PlayListMark block is the file's last section (extension data start is
      // 0), so growing it needs only its length u32, its mark count u16, and
      // the appended 14-byte entry (timestamp at entry offset 4).
      const withSecondChapter = (bytes) => {
        const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
        const marks = view.getUint32(12);
        const out = new Uint8Array(bytes.length + 14);
        out.set(bytes);
        out.set(bytes.slice(marks + 6, marks + 6 + 14), bytes.length);
        const outView = new DataView(out.buffer);
        outView.setUint32(marks, view.getUint32(marks) + 14);
        outView.setUint16(marks + 4, view.getUint16(marks + 4) + 1);
        outView.setUint32(bytes.length + 4, view.getUint32(marks + 6 + 4) + 45_000 * 10);
        return out;
      };
      const files = items.map((item) => toFile(item.path, decode(item.b64)));
      const source = items.find((item) => item.path.endsWith("00000.mpls"));
      files.push(
        toFile(
          "WASMDISC/BDMV/PLAYLIST/00001.mpls",
          withSecondChapter(withLength(decode(source.b64), 40)),
        ),
      );
      files.push(toFile("WASMDISC/BDMV/PLAYLIST/00002.mpls", withLength(decode(source.b64), 10)));
      const transfer = new DataTransfer();
      for (const file of files) {
        transfer.items.add(file);
      }
      const picker = document.getElementById("picker");
      picker.files = transfer.files;
      picker.dispatchEvent(new Event("change"));
    }, entries);
    await demoPage.waitForFunction(
      () => document.querySelectorAll("#playlist-body tr").length === 2,
      { timeout: 30000 },
    );

    // One structural snapshot of the page: table order and numbers, the active
    // row, both panes' cell texts, the hint, the cards' visibility, and every
    // Worker request posted so far.
    const readDemo = () =>
      demoPage.evaluate(() => {
        const texts = (selector) =>
          [...document.querySelectorAll(selector)].map((node) => node.textContent);
        const grid = (selector) =>
          [...document.querySelectorAll(selector)].map((tr) =>
            [...tr.children].map((td) => td.textContent),
          );
        return {
          names: texts("#playlist-body td.name"),
          positions: texts("#playlist-body tr td:nth-child(2)"),
          sizes: texts("#playlist-body tr td:nth-child(6)"),
          active: document.querySelector("#playlist-body tr.active")?.dataset.name ?? null,
          panesVisible: !document.getElementById("detail-panes").hidden,
          paneLabel: document.getElementById("pane-playlist").textContent,
          files: grid("#files-body tr"),
          codecs: grid("#codecs-body tr"),
          hintHidden: document.getElementById("hidden-hint").hidden,
          hintText: document.getElementById("hidden-hint").textContent,
          reportHidden: document.getElementById("report-card").hidden,
          progressHidden: document.getElementById("progress-card").hidden,
          discardHidden: document.getElementById("discard-note").hidden,
          calls: window.__calls.slice(),
        };
      });
    const rowCountIs = (count) =>
      demoPage.waitForFunction(
        (want) => document.querySelectorAll("#playlist-body tr").length === want,
        count,
        { timeout: 30000 },
      );

    const initial = await readDemo();

    // The table loads ascending by `#`, so the first click on that column must
    // go straight to descending — a first click never leaves the order as-is.
    await demoPage.click('th[data-sort="position"]');
    const positionSorted = await readDemo();
    // The rows now read ascending by name too, so a first click on the name
    // column is descending again; the second click flips it.
    await demoPage.click('th[data-sort="name"]');
    const nameFirst = await readDemo();
    await demoPage.click('th[data-sort="name"]');
    const nameSecond = await readDemo();

    // Activating the other row swaps both panes to its playlist.
    await demoPage.evaluate(() => {
      document.querySelector('#playlist-body tr[data-name="00000.MPLS"] td.name').click();
    });
    const activated = await readDemo();

    // The display settings: each one re-projects the model the page already
    // holds, so the request log must not grow while the table changes.
    await demoPage.click("#settings-btn");
    await demoPage.click("#opt-short");
    await rowCountIs(3);
    const shortShown = await readDemo();
    await demoPage.click("#opt-short");
    await rowCountIs(2);
    await demoPage.click("#opt-human-sizes");
    const grouped = await readDemo();
    await demoPage.click("#opt-human-sizes");
    await demoPage.click("#opt-chapters");
    const noSuffix = await readDemo();
    await demoPage.click("#opt-chapters");
    await demoPage.click("#settings-close");
    const preScan = await readDemo();

    // Back to the table order before scanning, so the page's selection order is
    // the disc's presentation order — which is what makes the re-rendered
    // report below comparable to the scan's own, byte for byte.
    await demoPage.click('th[data-sort="position"]');

    // The measured scan through the page (both shown rows are ticked): the
    // report card appears and the panes' measured cells fill in place.
    await demoPage.click("#scan-btn");
    await demoPage.waitForFunction(() => !document.getElementById("report-card").hidden, {
      timeout: 120000,
    });
    const scanned = await readDemo();
    const report = await demoPage.evaluate(() => document.getElementById("report").textContent);

    // The report-section switches: every flip re-renders the held disc through
    // the package — the displayed text changes while the request log gains only
    // `render` kinds and the progress card never shows. Both orders of turning
    // the two sections off are walked, so the switches' composition is pinned.
    const reportNow = () => demoPage.evaluate(() => document.getElementById("report").textContent);
    const toggleSection = async (selector) => {
      const before = await reportNow();
      await demoPage.click(selector);
      await demoPage.waitForFunction(
        (prev) => document.getElementById("report").textContent !== prev,
        before,
        { timeout: 60000 },
      );
      return reportNow();
    };
    await demoPage.click("#settings-btn");
    const sdOff = await toggleSection("#opt-diagnostics");
    const bothOff1 = await toggleSection("#opt-summary");
    const qsOff = await toggleSection("#opt-diagnostics");
    const restored = await toggleSection("#opt-summary");
    const qsOff2 = await toggleSection("#opt-summary");
    const bothOff2 = await toggleSection("#opt-diagnostics");
    const sdOff2 = await toggleSection("#opt-summary");
    const restored2 = await toggleSection("#opt-diagnostics");
    const afterRenders = await readDemo();

    // The threshold: committing the value already in force must send nothing;
    // committing a new one re-runs `inspect`, re-classifies the table, and
    // visibly discards the measured results and the held report.
    await demoPage.fill("#opt-short-seconds", "20");
    await demoPage.locator("#opt-short-seconds").blur();
    await demoPage.fill("#opt-short-seconds", "5");
    await demoPage.locator("#opt-short-seconds").blur();
    await rowCountIs(3);
    const thresholdApplied = await readDemo();

    // Retention travels with the next measured scan, so flipping it must post
    // no request at all and leave everything the page holds where it is.
    await demoPage.click("#opt-keep-partial");
    const retentionOff = await readDemo();
    await demoPage.click("#settings-close");

    // A phone-width viewport must not scroll the page sideways — wide tables
    // scroll inside their own wrapper instead.
    await demoPage.setViewportSize({ width: 390, height: 844 });
    const pageScrolls = await demoPage.evaluate(
      () => document.documentElement.scrollWidth > document.documentElement.clientWidth,
    );

    // The settings survive a reload through `localStorage`; the dialog's
    // controls are initialized from what was stored.
    await demoPage.reload();
    const persisted = await demoPage.evaluate(() => ({
      seconds: document.getElementById("opt-short-seconds").value,
      short: document.getElementById("opt-short").checked,
      looping: document.getElementById("opt-looping").checked,
      human: document.getElementById("opt-human-sizes").checked,
      chapters: document.getElementById("opt-chapters").checked,
      diagnostics: document.getElementById("opt-diagnostics").checked,
      summary: document.getElementById("opt-summary").checked,
      keepPartial: document.getElementById("opt-keep-partial").checked,
    }));

    demo = {
      initial,
      positionSorted,
      nameFirst,
      nameSecond,
      activated,
      shortShown,
      grouped,
      noSuffix,
      preScan,
      scanned,
      report,
      sdOff,
      bothOff1,
      qsOff,
      restored,
      qsOff2,
      bothOff2,
      sdOff2,
      restored2,
      afterRenders,
      thresholdApplied,
      retentionOff,
      pageScrolls,
      persisted,
    };
  } finally {
    await browser.close();
    server.close();
  }

  function compare(label, text, want) {
    const got = Buffer.from(text, "utf8");
    if (got.equals(want)) {
      console.log(`PASS — Worker ${label} matches the golden (${want.length} bytes).`);
      return true;
    }
    console.error(
      `FAIL — ${label} (${got.length} bytes) diverged from golden (${want.length} bytes).`,
    );
    const limit = Math.min(got.length, want.length);
    for (let i = 0; i < limit; i++) {
      if (got[i] !== want[i]) {
        const ctx = (buf) =>
          JSON.stringify(buf.slice(Math.max(0, i - 30), i + 30).toString("utf8"));
        console.error(`  first diff at byte ${i}:`);
        console.error(`    golden: ${ctx(want)}`);
        console.error(`    got:    ${ctx(got)}`);
        break;
      }
    }
    return false;
  }

  // Every call returns both playlists; the threshold moves only `hiddenBy`, and
  // the table numbering describes the disc rather than any view of it.
  const want = {
    standard: ["00000.MPLS:", "00001.MPLS:short"],
    lowered: ["00000.MPLS:", "00001.MPLS:"],
    measured: ["00000.MPLS:", "00001.MPLS:"],
    numbering: ["1/1", "1/2"],
  };
  const listOk = JSON.stringify(listings) === JSON.stringify(want);
  if (listOk) {
    console.log("PASS — Worker classification follows the threshold and withholds nothing.");
  } else {
    console.error(`FAIL — classification: got ${JSON.stringify(listings)}`);
  }

  // The structural model: no demux ran, so `measured` is false and every rate is
  // zero because nothing measured it. The one ~30 s playlist is group 1,
  // position 1 and withheld by nothing; its 301,133,999 ticks integer-divide to
  // the 30 s the table cell prints, which the 30.1134 s float cannot promise.
  const inspectOk =
    JSON.stringify(structured.inspected) ===
    JSON.stringify({
      measured: false,
      volumeLabel: "WASMDISC",
      playlists: ["00000.MPLS"],
      streams: 2,
      rates: [0, 0],
      group: 1,
      position: 1,
      hiddenBy: [],
      ticks: 301_133_999,
    });
  if (inspectOk) {
    console.log("PASS — Worker inspect returns the unmeasured disc model.");
  } else {
    console.error(`FAIL — inspect model: got ${JSON.stringify(structured.inspected)}`);
  }

  // One measured scan, both outputs, and the model round-tripped back into the
  // report: `scan`'s report and a `renderReport` of the disc it returned must
  // both be the golden bytes, and switching a section off must drop it alone.
  const scanReportOk = compare("scan report", structured.report, golden);
  const roundTripOk = compare("renderReport round trip", structured.reRendered, golden);
  const scanOk =
    scanReportOk &&
    roundTripOk &&
    structured.measured === true &&
    structured.rate > 0 &&
    !structured.trimmed.includes("STREAM DIAGNOSTICS:") &&
    structured.trimmed.includes("QUICK SUMMARY:");
  if (!scanOk) {
    console.error(
      `FAIL — scan/renderReport: measured ${structured.measured}, first stream rate ${structured.rate}, trimmed ${structured.trimmed.length} B.`,
    );
  }

  // The same two calls handed a single `File` instead of a list, which is what
  // selects the `.iso` path.
  const isoScanOk = compare(".iso scan report", isoStructured.report, isoGolden);
  const isoRoundTripOk = compare(
    ".iso renderReport round trip",
    isoStructured.reRendered,
    isoGolden,
  );
  const isoStructuredOk =
    isoScanOk &&
    isoRoundTripOk &&
    isoStructured.label === "Blu-Ray" &&
    isoStructured.measured === true &&
    JSON.stringify(isoStructured.playlists) === '["00000.MPLS"]';
  if (!isoStructuredOk) {
    console.error(
      `FAIL — .iso structured API: label ${isoStructured.label}, playlists ${JSON.stringify(isoStructured.playlists)}, measured ${isoStructured.measured}.`,
    );
  }

  // The demo page. Cell values are pinned where the fixture pins them: the
  // 40 s patched playlist outranks the 30 s one (longest first in the shared
  // group), the clip's on-disk 11,145,216 B reads 10.63 MB, its packet-derived
  // 11,064,384 B reads 10.55 MB, and the two codec rows carry the report's own
  // codec names and measured rates.
  const demoEq = (label, got, wantValue) => {
    if (JSON.stringify(got) === JSON.stringify(wantValue)) {
      console.log(`PASS — demo ${label}.`);
      return true;
    }
    console.error(
      `FAIL — demo ${label}: got ${JSON.stringify(got)}, want ${JSON.stringify(wantValue)}`,
    );
    return false;
  };
  // A report section under the locked format runs from its heading line
  // through the second blank line below it (heading, blank, body, blank), and
  // the optional sections repeat once per rendered playlist — so a switch is
  // checked by stripping EVERY occurrence of its heading's section.
  const stripSection = (text, heading) => {
    const lines = text.split("\r\n");
    if (!lines.includes(heading)) {
      throw new Error(`section ${heading} not found`);
    }
    let start = lines.indexOf(heading);
    while (start !== -1) {
      let end = start;
      let blanks = 0;
      while (end + 1 < lines.length && blanks < 2) {
        end += 1;
        if (lines[end] === "") {
          blanks += 1;
        }
      }
      lines.splice(start, end - start + 1);
      start = lines.indexOf(heading);
    }
    return lines.join("\r\n");
  };
  let demoOk = true;
  demoOk &= demoEq("initial table order (by position)", demo.initial.names, [
    "00001.MPLS [02 Chapters]",
    "00000.MPLS",
  ]);
  demoOk &= demoEq("initial numbering", demo.initial.positions, ["1", "2"]);
  demoOk &= demoEq("initial active row", demo.initial.active, "00001.MPLS");
  demoOk &= demoEq("panes visible", demo.initial.panesVisible, true);
  demoOk &= demoEq("pane label", demo.initial.paneLabel, "00001.MPLS");
  demoOk &= demoEq(
    "initial hint names the short playlist",
    demo.initial.hintText,
    ["Hidden by filters (short): 00002.MPLS - enable in settings"].join("\n"),
  );
  demoOk &= demoEq("load cost one inspect", demo.initial.calls, ["inspect"]);
  demoOk &= demoEq("stream-file pane (unmeasured)", demo.initial.files, [
    ["00000.M2TS", "1", "00:00:40", "10.63 MB", "—"],
  ]);
  // The LPCM row's language is three NUL bytes: the fixture's playlist declares
  // that as the language code, and the locked report prints it verbatim (the
  // golden's Language cell carries the same bytes), so the model and the pane
  // carry it too.
  demoOk &= demoEq(
    "codec pane names + unmeasured rates",
    demo.initial.codecs.map((row) => [row[0], row[1], row[2]]),
    [
      ["MPEG-4 AVC Video", "", "—"],
      ["LPCM Audio", "\u0000\u0000\u0000", "—"],
    ],
  );
  demoOk &= demoEq(
    "codec descriptions populate",
    demo.initial.codecs.every((row) => row[3].length > 0),
    true,
  );
  demoOk &= demoEq("first click on the ascending # column descends", demo.positionSorted.names, [
    "00000.MPLS",
    "00001.MPLS [02 Chapters]",
  ]);
  demoOk &= demoEq("numbers travel with their rows", demo.positionSorted.positions, ["2", "1"]);
  demoOk &= demoEq("first click on the ascending name column descends", demo.nameFirst.names, [
    "00001.MPLS [02 Chapters]",
    "00000.MPLS",
  ]);
  demoOk &= demoEq("second click flips to ascending", demo.nameSecond.names, [
    "00000.MPLS",
    "00001.MPLS [02 Chapters]",
  ]);
  demoOk &= demoEq("row click activates", demo.activated.active, "00000.MPLS");
  demoOk &= demoEq("pane follows the active row", demo.activated.paneLabel, "00000.MPLS");
  demoOk &= demoEq("active playlist clip length", demo.activated.files[0][2], "00:00:30");
  demoOk &= demoEq("show-short lists the withheld playlist", demo.shortShown.names, [
    "00000.MPLS",
    "00001.MPLS [02 Chapters]",
    "00002.MPLS",
  ]);
  demoOk &= demoEq("show-short retires the hint", demo.shortShown.hintHidden, true);
  demoOk &= demoEq("grouped bytes in the size cells", demo.grouped.sizes, [
    "11,145,216",
    "11,145,216",
  ]);
  demoOk &= demoEq("chapter suffix off", demo.noSuffix.names, ["00000.MPLS", "00001.MPLS"]);
  demoOk &= demoEq("human-readable size cells return", demo.preScan.sizes, [
    "10.63 MB",
    "10.63 MB",
  ]);
  demoOk &= demoEq("display settings cost no wasm call", demo.preScan.calls, ["inspect"]);
  demoOk &= demoEq("scan keeps the active row", demo.scanned.active, "00000.MPLS");
  demoOk &= demoEq("measured size fills after the scan", demo.scanned.files, [
    ["00000.M2TS", "1", "00:00:30", "10.63 MB", "10.55 MB"],
  ]);
  // 973, not the report's 974: the pane integer-divides bits/s to kbps like the
  // desktop pane, where the report's codec table rounds.
  demoOk &= demoEq(
    "measured bit rates fill after the scan",
    demo.scanned.codecs.map((row) => row[2]),
    ["973 kbps", "1,536 kbps"],
  );
  demoOk &= demoEq("the demo report renders", demo.report.includes("QUICK SUMMARY:"), true);
  demoOk &= demoEq("the scan cost one scan call", demo.scanned.calls, ["inspect", "scan"]);

  // The section switches: eight flips, eight `render` requests, no scan and no
  // progress bar; every rendering equals the scan's own report minus exactly
  // the switched-off sections, whichever order the switches went down in.
  const strippedSd = stripSection(demo.report, "STREAM DIAGNOSTICS:");
  const strippedQs = stripSection(demo.report, "QUICK SUMMARY:");
  const strippedBoth = stripSection(strippedSd, "QUICK SUMMARY:");
  demoOk &= compare("demo render: diagnostics off", demo.sdOff, Buffer.from(strippedSd));
  demoOk &= compare(
    "demo render: both off (diagnostics first)",
    demo.bothOff1,
    Buffer.from(strippedBoth),
  );
  demoOk &= compare("demo render: summary off", demo.qsOff, Buffer.from(strippedQs));
  demoOk &= compare("demo render: both back on", demo.restored, Buffer.from(demo.report));
  demoOk &= compare("demo render: summary off (second pass)", demo.qsOff2, Buffer.from(strippedQs));
  demoOk &= compare(
    "demo render: both off (summary first)",
    demo.bothOff2,
    Buffer.from(strippedBoth),
  );
  demoOk &= compare(
    "demo render: diagnostics off (second pass)",
    demo.sdOff2,
    Buffer.from(strippedSd),
  );
  demoOk &= compare("demo render: restored again", demo.restored2, Buffer.from(demo.report));
  demoOk &= demoEq("section flips cost renders only", demo.afterRenders.calls, [
    "inspect",
    "scan",
    ...Array(8).fill("render"),
  ]);
  demoOk &= demoEq("no scan ran while re-rendering", demo.afterRenders.progressHidden, true);
  demoOk &= demoEq("measured cells survive the re-renders", demo.afterRenders.files, [
    ["00000.M2TS", "1", "00:00:30", "10.63 MB", "10.55 MB"],
  ]);

  // The threshold transition: the same value again sent nothing; 5 s re-ran
  // `inspect` (one request), re-classified the 10 s playlist into the table,
  // and visibly discarded the measured results and the held report.
  demoOk &= demoEq("threshold change cost one inspect", demo.thresholdApplied.calls, [
    "inspect",
    "scan",
    ...Array(8).fill("render"),
    "inspect",
  ]);
  demoOk &= demoEq("threshold re-classifies the table", demo.thresholdApplied.names, [
    "00001.MPLS [02 Chapters]",
    "00000.MPLS",
    "00002.MPLS",
  ]);
  demoOk &= demoEq("threshold retires the hint", demo.thresholdApplied.hintHidden, true);
  demoOk &= demoEq("threshold discards the measured sizes", demo.thresholdApplied.files, [
    ["00000.M2TS", "1", "00:00:30", "10.63 MB", "—"],
  ]);
  demoOk &= demoEq(
    "threshold discards the measured rates",
    demo.thresholdApplied.codecs.map((row) => row[2]),
    ["—", "—"],
  );
  demoOk &= demoEq("threshold hides the report", demo.thresholdApplied.reportHidden, true);
  demoOk &= demoEq("the discard is visible", demo.thresholdApplied.discardHidden, false);
  demoOk &= demoEq("threshold keeps the active row", demo.thresholdApplied.active, "00000.MPLS");

  // The retention switch: nothing was requested and nothing on the page moved.
  demoOk &= demoEq(
    "retention flip costs no wasm call",
    demo.retentionOff.calls,
    demo.thresholdApplied.calls,
  );
  demoOk &= demoEq("retention flip leaves the table", demo.retentionOff.names, [
    "00001.MPLS [02 Chapters]",
    "00000.MPLS",
    "00002.MPLS",
  ]);

  demoOk &= demoEq("no sideways page scroll at phone width", demo.pageScrolls, false);
  // Retention was switched off above, so the reload proves the stored `false`
  // is read back rather than defaulted to on like an absent setting.
  demoOk &= demoEq("settings survive a reload", demo.persisted, {
    seconds: "5",
    short: false,
    looping: false,
    human: true,
    chapters: true,
    diagnostics: true,
    summary: true,
    keepPartial: false,
  });
  demoOk = Boolean(demoOk);

  process.exit(listOk && inspectOk && scanOk && isoStructuredOk && demoOk ? 0 : 1);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
