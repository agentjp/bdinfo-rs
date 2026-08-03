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
    // patched to ~40 s the same way as above) makes the table two rows, so
    // ordering is observable. A constructed `File` has an empty read-only
    // `webkitRelativePath`; an own property carries the folder path the demo's
    // picker handler reads.
    const demoPage = await browser.newPage();
    demoPage.on("console", (msg) => console.log(`  [demo] ${msg.text()}`));
    demoPage.on("pageerror", (err) => console.log(`  [demo pageerror] ${err.message}`));
    await demoPage.goto(`${base}/index.html`);
    await demoPage.evaluate((items) => {
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
      const files = items.map((item) => toFile(item.path, decode(item.b64)));
      const source = items.find((item) => item.path.endsWith("00000.mpls"));
      const patched = decode(source.b64);
      new DataView(patched.buffer).setUint32(86, 27_000_000 + 45_000 * 40);
      files.push(toFile("WASMDISC/BDMV/PLAYLIST/00001.mpls", patched));
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
    // row, and both panes' cell texts.
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
          active: document.querySelector("#playlist-body tr.active")?.dataset.name ?? null,
          panesVisible: !document.getElementById("detail-panes").hidden,
          paneLabel: document.getElementById("pane-playlist").textContent,
          files: grid("#files-body tr"),
          codecs: grid("#codecs-body tr"),
        };
      });

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

    // The measured scan through the page (both rows are ticked by default):
    // the report card appears and the panes' measured cells fill in place.
    await demoPage.click("#scan-btn");
    await demoPage.waitForFunction(() => !document.getElementById("report-card").hidden, {
      timeout: 120000,
    });
    const scanned = await readDemo();
    const report = await demoPage.evaluate(() => document.getElementById("report").textContent);

    // A phone-width viewport must not scroll the page sideways — wide tables
    // scroll inside their own wrapper instead.
    await demoPage.setViewportSize({ width: 390, height: 844 });
    const pageScrolls = await demoPage.evaluate(
      () => document.documentElement.scrollWidth > document.documentElement.clientWidth,
    );

    demo = {
      initial,
      positionSorted,
      nameFirst,
      nameSecond,
      activated,
      scanned,
      report,
      pageScrolls,
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
  let demoOk = true;
  demoOk &= demoEq("initial table order (by position)", demo.initial.names, [
    "00001.MPLS",
    "00000.MPLS",
  ]);
  demoOk &= demoEq("initial numbering", demo.initial.positions, ["1", "2"]);
  demoOk &= demoEq("initial active row", demo.initial.active, "00001.MPLS");
  demoOk &= demoEq("panes visible", demo.initial.panesVisible, true);
  demoOk &= demoEq("pane label", demo.initial.paneLabel, "00001.MPLS");
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
    "00001.MPLS",
  ]);
  demoOk &= demoEq("numbers travel with their rows", demo.positionSorted.positions, ["2", "1"]);
  demoOk &= demoEq("first click on the ascending name column descends", demo.nameFirst.names, [
    "00001.MPLS",
    "00000.MPLS",
  ]);
  demoOk &= demoEq("second click flips to ascending", demo.nameSecond.names, [
    "00000.MPLS",
    "00001.MPLS",
  ]);
  demoOk &= demoEq("row click activates", demo.activated.active, "00000.MPLS");
  demoOk &= demoEq("pane follows the active row", demo.activated.paneLabel, "00000.MPLS");
  demoOk &= demoEq("active playlist clip length", demo.activated.files[0][2], "00:00:30");
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
  demoOk &= demoEq("no sideways page scroll at phone width", demo.pageScrolls, false);
  demoOk = Boolean(demoOk);

  process.exit(listOk && inspectOk && scanOk && isoStructuredOk && demoOk ? 0 : 1);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
