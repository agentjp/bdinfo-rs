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
// The deployed CSP, served on the demo page below so its inline theme-boot
// script runs (or is blocked) exactly as deployed: a stale script hash in
// _headers is a silent wrong-theme-on-cold-start in production, but a hard
// failure of the boot assertions here.
const headersPath = resolve(here, "../_headers");

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

function startServer(csp) {
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
      const headers = { "content-type": MIME[extname(safe)] ?? "application/octet-stream" };
      // Only the demo page: the harness page's inline module script is a test
      // fixture the deployed policy never covers.
      if (urlPath === "/index.html") {
        headers["content-security-policy"] = csp;
      }
      res.writeHead(200, headers);
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

  const csp = (await readFile(headersPath, "utf8")).match(/^\s*Content-Security-Policy: (.+)$/m)[1];
  const server = await startServer(csp);
  const { port } = server.address();
  const base = `http://127.0.0.1:${port}`;

  const browser = await chromium.launch({
    channel: "chrome",
    headless: true,
    args: ["--no-sandbox"],
  });

  let listings;
  let structured;
  let surface;
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

    // The rest of the option/API surface through the real Worker: an
    // out-of-domain threshold rejects instead of defaulting, a codecs inspect
    // deepens the stream descriptions while staying unmeasured, and the report
    // save-file name comes back through the core sanitizer.
    surface = await page.evaluate(async () => {
      const rejected = await window.__inspect(window.__files, { shortPlaylistSeconds: -1 }).then(
        () => null,
        (error) => String(error.message ?? error),
      );
      const codecs = await window.__inspect(window.__files, { codecs: true });
      return {
        rejected,
        codecsMeasured: codecs.measured,
        codecsVideo: codecs.playlists[0].streams[0].fullDescription,
        fileName: await window.__reportFileName("a/b:c"),
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
      // below can assert which settings cost a wasm call and which cost none,
      // and which playlists each measured scan was asked for.
      const NativeWorker = window.Worker;
      window.__calls = [];
      window.__selections = [];
      // Cancels the running scan from inside the page, one message after its
      // first measured snapshot has been applied. The fixture disc scans in
      // well under a second, so a cancel driven from the test process would
      // race the completion; this cannot.
      window.__cancelOnMeasured = false;
      window.Worker = class extends NativeWorker {
        #handler = null;
        postMessage(message, ...rest) {
          if (typeof message === "object" && message !== null && "kind" in message) {
            window.__calls.push(message.kind);
            if ("selection" in message) {
              window.__selections.push(message.selection);
            }
          }
          super.postMessage(message, ...rest);
        }
        get onmessage() {
          return this.#handler;
        }
        set onmessage(handler) {
          this.#handler = handler;
          super.onmessage = (event) => {
            handler(event);
            if (
              window.__cancelOnMeasured &&
              event.data?.type === "measured" &&
              event.data.measured.playlists.some((playlist) => playlist.measuredBytes > 0)
            ) {
              window.__cancelOnMeasured = false;
              document.getElementById("cancel-btn").click();
            }
          };
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
      // Kept for the damaged re-pick at the end of the run.
      window.__demoFiles = files;
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
          measured: texts('#playlist-body td[data-cell="measured"]'),
          active: document.querySelector("#playlist-body tr.active")?.dataset.name ?? null,
          panesVisible: !document.getElementById("detail-panes").hidden,
          paneLabel: document.getElementById("pane-playlist").textContent,
          files: grid("#files-body tr"),
          codecs: grid("#codecs-body tr"),
          hintHidden: document.getElementById("hidden-hint").hidden,
          hintText: document.getElementById("hidden-hint").textContent,
          revealLabel: document.getElementById("reveal-btn").textContent,
          // The disc-info strip: the lines it shows, and what it says on them.
          info: [...document.querySelectorAll("#disc-info p")]
            .filter((line) => !line.hidden)
            .map((line) => line.textContent.replace(/\s+/g, " ").trim()),
          badgeHidden: document.getElementById("encrypted-badge").hidden,
          // The source card: the slim bar names what is loaded, and the
          // dropzone is only up while there is nothing loaded to name.
          picked: document.getElementById("picked").hidden
            ? null
            : document.getElementById("picked-name").textContent,
          dropzoneHidden: document.getElementById("dropzone").hidden,
          viewDisabled: document.getElementById("view-report-btn").disabled,
          errorsHidden: document.getElementById("scan-errors").hidden,
          errorsCount: document.getElementById("scan-errors-count").textContent,
          errorLines: texts("#scan-errors-list li"),
          reportHidden: document.getElementById("report-card").hidden,
          progressHidden: document.getElementById("progress-card").hidden,
          discardHidden: document.getElementById("discard-note").hidden,
          selCount: document.getElementById("sel-count").textContent,
          times: document.getElementById("progress-times").textContent,
          // The scan-set controls: live between scans, inert while one runs.
          scanDisabled: document.getElementById("scan-btn").disabled,
          boxesDisabled: [...document.querySelectorAll("#playlist-body input[type=checkbox]")].map(
            (box) => box.disabled,
          ),
          sortFrozen: [...document.querySelectorAll("th[data-sort]")].map((th) =>
            th.classList.contains("frozen"),
          ),
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

    // The transient reveal: Show puts the withheld playlist in the table and
    // Hide takes it back out, with the stored settings untouched throughout —
    // it is session state, not a fourth filter switch.
    const storedShort = () =>
      demoPage.evaluate(
        () => JSON.parse(window.localStorage.getItem("bdinfo-rs.settings")).showShortPlaylists,
      );
    await demoPage.click("#reveal-btn");
    await rowCountIs(3);
    const revealed = { ...(await readDemo()), stored: await storedShort() };
    await demoPage.click("#reveal-btn");
    await rowCountIs(2);
    const rehidden = await readDemo();
    // A settings change drops it: the settings are then saying what the table
    // shows, and a reveal over the old view would be describing nothing.
    await demoPage.click("#reveal-btn");
    await rowCountIs(3);
    await demoPage.click("#settings-btn");
    await demoPage.click("#opt-chapters");
    await rowCountIs(2);
    const revealDropped = await readDemo();
    await demoPage.click("#opt-chapters");
    await demoPage.click("#settings-close");

    const reportNow = () => demoPage.evaluate(() => document.getElementById("report").textContent);

    // The report BEFORE any measured scan: the same render over a disc whose
    // measured values are all zero. It prints the scan set the button would
    // measure — the table's own rows, in the table's own order, which the
    // name-ascending sort in force here makes visible — and it is re-rendered
    // when that set changes rather than left describing a stale selection.
    const uncheck = (name) =>
      demoPage.evaluate((playlist) => {
        document
          .querySelector(`#playlist-body tr[data-name="${playlist}"] input[type=checkbox]`)
          .click();
      }, name);
    const reportChanges = async (act) => {
      const before = await reportNow();
      await act();
      await demoPage.waitForFunction(
        (prev) => document.getElementById("report").textContent !== prev,
        before,
        { timeout: 60000 },
      );
      return reportNow();
    };
    const preview = await reportChanges(() => demoPage.click("#view-report-btn"));
    const previewUnchecked = await reportChanges(() => uncheck("00000.MPLS"));
    const previewRestored = await reportChanges(() => uncheck("00000.MPLS"));
    const previewState = await readDemo();

    // Back to the table order before scanning, so the page's selection order is
    // the disc's presentation order — which is what makes the re-rendered
    // report below comparable to the scan's own, byte for byte. Re-ordering the
    // table re-orders the report the pre-scan render is showing, so this one
    // costs a render; a request is logged as the click makes it, so the counts
    // below need no waiting.
    await demoPage.click('th[data-sort="position"]');
    const resorted = await readDemo();

    // Nothing ticked is a whole-disc scan, not a refusal to scan: the button
    // stays live and the count says what pressing it will do. Neither click
    // moves the scan SET here — every row was ticked, and unticking them all
    // falls back to the same rows in the same order — so the shown report is
    // already the right one and no render is asked for.
    await demoPage.click("#clear-sel");
    const cleared = await readDemo();
    await demoPage.click("#select-all");
    const reselected = await readDemo();

    // The measured scan through the page (both shown rows are ticked): the
    // report card appears and the panes' measured cells fill in place.
    //
    // `runScan` installs the scan and redraws the table synchronously, so
    // everything read after the click below is genuinely mid-scan state — no
    // polling, and no race with a fixture that finishes in under a second.
    const midScan = await demoPage.evaluate(() => {
      const read = () => ({
        names: [...document.querySelectorAll("#playlist-body td.name")].map((td) => td.textContent),
        selCount: document.getElementById("sel-count").textContent,
        boxesDisabled: [...document.querySelectorAll("#playlist-body input[type=checkbox]")].map(
          (box) => box.disabled,
        ),
        sortFrozen: [...document.querySelectorAll("th[data-sort]")].map((th) =>
          th.classList.contains("frozen"),
        ),
        selectAllDisabled: document.getElementById("select-all").disabled,
        clearDisabled: document.getElementById("clear-sel").disabled,
        scanDisabled: document.getElementById("scan-btn").disabled,
        viewDisabled: document.getElementById("view-report-btn").disabled,
        revealDisabled: document.getElementById("reveal-btn").disabled,
        progressHidden: document.getElementById("progress-card").hidden,
        times: document.getElementById("progress-times").textContent,
      });
      document.getElementById("scan-btn").click();
      const frozen = read();
      // Every edit to the scan set is inert while the scan runs — the header
      // sort, both selection buttons, and the check cell that stands in for a
      // row's (now disabled) box.
      document.querySelector('th[data-sort="name"]').click();
      document.getElementById("select-all").click();
      document.getElementById("clear-sel").click();
      document.querySelector("#playlist-body td.col-check").click();
      const afterEdits = read();
      // The active row is not part of the scan set and stays live: clicking the
      // other row re-targets the panes under the running scan.
      document.querySelector('#playlist-body tr[data-name="00001.MPLS"] td.name').click();
      const activated = {
        active: document.querySelector("#playlist-body tr.active")?.dataset.name ?? null,
        paneLabel: document.getElementById("pane-playlist").textContent,
      };
      document.querySelector('#playlist-body tr[data-name="00000.MPLS"] td.name').click();
      return { frozen, afterEdits, activated };
    });
    await demoPage.waitForFunction(() => !document.getElementById("report-card").hidden, {
      timeout: 120000,
    });
    const scanned = await readDemo();
    const report = await demoPage.evaluate(() => document.getElementById("report").textContent);

    // Ctrl+C copies the highlighted row's disc path, in the desktop app's
    // precedence. The clipboard write is stubbed rather than read back: what the
    // page copies is the assertion, and a headless run has no clipboard
    // permission to lean on.
    const copied = await demoPage.evaluate(async () => {
      const paths = [];
      navigator.clipboard.writeText = (text) => {
        paths.push(text);
        return Promise.resolve();
      };
      const press = () => {
        document.dispatchEvent(
          new KeyboardEvent("keydown", { key: "c", ctrlKey: true, bubbles: true }),
        );
        // The copy awaits the clipboard write, so let the microtasks run.
        return new Promise((done) => setTimeout(done, 0));
      };
      const firstFile = () => document.querySelector("#files-body tr");
      // Nothing picked in the panes: the active playlist row is the target.
      await press();
      // A highlighted Stream File row wins, and carries its clip's real name.
      firstFile().click();
      await press();
      const flagged = firstFile().classList.contains("copied-row");
      // A highlighted Codec row copies nothing — the scope the classic tool has.
      document.querySelector("#codecs-body tr").click();
      await press();
      // A text selection is the user's own copy; the shortcut stands aside.
      document.querySelector("#codecs-body tr").click();
      const range = document.createRange();
      range.selectNodeContents(document.getElementById("sel-count"));
      window.getSelection().removeAllRanges();
      window.getSelection().addRange(range);
      await press();
      window.getSelection().removeAllRanges();
      return { paths, flagged };
    });

    // The report-section switches: every flip re-renders the held disc through
    // the package — the displayed text changes while the request log gains only
    // `render` kinds and the progress card never shows. Both orders of turning
    // the two sections off are walked, so the switches' composition is pinned.
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

    // The threshold input accepts 0 — the committed "rule off" value: one more
    // inspect re-classifies the table (nothing is short under 0 s either) and
    // the stored setting becomes 0.
    await demoPage.fill("#opt-short-seconds", "0");
    await demoPage.locator("#opt-short-seconds").blur();
    await demoPage.waitForFunction(
      (want) => window.__calls.length === want,
      retentionOff.calls.length + 1,
      { timeout: 30000 },
    );
    const zeroThreshold = await readDemo();
    await demoPage.click("#settings-close");

    // A cancelled whole-disc scan. Nothing is ticked, so the scan set is every
    // listed row — which the recorded request proves — and the page cancels
    // itself the moment the first measured snapshot has been applied. What that
    // snapshot ticked must still be standing afterwards, beside no report: a
    // cancel measures something real and renders nothing.
    await demoPage.click("#clear-sel");
    const beforeCancel = await readDemo();
    await demoPage.evaluate(() => {
      window.__cancelOnMeasured = true;
      document.getElementById("scan-btn").click();
    });
    await demoPage.waitForFunction(
      () => document.getElementById("progress-card").hidden,
      undefined,
      { timeout: 120000 },
    );
    const cancelled = await readDemo();
    const cancelledSelection = await demoPage.evaluate(() => window.__selections.at(-1));

    // A phone-width viewport must not scroll the page sideways — wide tables
    // scroll inside their own wrapper instead.
    await demoPage.setViewportSize({ width: 390, height: 844 });
    const pageScrolls = await demoPage.evaluate(
      () => document.documentElement.scrollWidth > document.documentElement.clientWidth,
    );

    // The post-pick source bar: a loaded disc replaces the dropzone with the
    // slim bar naming it, and "Change disc…" puts the dropzone back without
    // disturbing the disc the page is showing — the re-pick below lands on the
    // bar again.
    const barLoaded = await readDemo();
    await demoPage.click("#change-disc");
    const barChanging = await readDemo();

    // A damaged disc through the page: the same folder plus a file in PLAYLIST
    // that is not a playlist, which the structural listing records and reads
    // past. A listing renders no report, so the failure strip is the only place
    // this shows — the state a browser user reaches before scanning anything.
    await demoPage.evaluate(() => {
      const junk = new File([new TextEncoder().encode("NOPE0100garbage")], "00009.mpls");
      Object.defineProperty(junk, "webkitRelativePath", {
        value: "WASMDISC/BDMV/PLAYLIST/00009.mpls",
      });
      const transfer = new DataTransfer();
      for (const file of [...window.__demoFiles, junk]) {
        transfer.items.add(file);
      }
      const picker = document.getElementById("picker");
      picker.files = transfer.files;
      picker.dispatchEvent(new Event("change"));
    });
    await demoPage.waitForFunction(
      () => !document.getElementById("scan-errors").hidden,
      undefined,
      { timeout: 30000 },
    );
    const damagedListing = await readDemo();

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

    // The theme. Headless Chrome runs OS-light by default, so the OS side is
    // pinned explicitly: Auto (nothing stored yet) follows an emulated OS flip
    // live, a manual chip beats the OS and rewrites both theme-color metas,
    // and the settings dialog closes and reopens with the stored chip pressed.
    const pageBg = () => demoPage.evaluate(() => getComputedStyle(document.body).backgroundColor);
    const pressedChips = () =>
      demoPage.evaluate(() =>
        [...document.querySelectorAll("#theme-chips .btn")].map((chip) =>
          chip.getAttribute("aria-pressed"),
        ),
      );
    await demoPage.emulateMedia({ colorScheme: "dark" });
    const autoDark = await pageBg();
    await demoPage.emulateMedia({ colorScheme: "light" });
    const autoLight = await pageBg();
    await demoPage.click("#settings-btn");
    await demoPage.click('#theme-chips button[data-theme-choice="dark"]');
    const overrideDark = {
      bg: await pageBg(),
      pressed: await pressedChips(),
      metas: await demoPage.evaluate(() =>
        [...document.querySelectorAll('meta[name="theme-color"]')].map((meta) => meta.content),
      ),
    };
    await demoPage.click('#theme-chips button[data-theme-choice="light"]');
    const overrideLight = await pageBg();
    await demoPage.click("#settings-close");

    // The boot: reload with a stored Light choice under a dark OS. The init
    // script records what data-theme was the moment the page's <style> element
    // appeared — mutation callbacks flush before the module scripts run, so
    // only the parser-run inline boot script can have set it by then. Light
    // ground despite the dark OS = the stored override, applied with no dark
    // flash; the CSP served above is what would block a stale-hashed script.
    await demoPage.addInitScript(() => {
      window.__themeAtStyle = "pending";
      const observer = new MutationObserver(() => {
        if (document.head?.querySelector("style") && window.__themeAtStyle === "pending") {
          window.__themeAtStyle = document.documentElement.dataset.theme ?? "absent";
          observer.disconnect();
        }
      });
      observer.observe(document, { childList: true, subtree: true });
    });
    await demoPage.reload();
    const booted = await demoPage.evaluate(() => ({
      atStyle: window.__themeAtStyle,
      attr: document.documentElement.dataset.theme ?? null,
      bg: getComputedStyle(document.body).backgroundColor,
      reportBg: getComputedStyle(document.getElementById("report")).backgroundColor,
    }));
    await demoPage.click("#settings-btn");
    const reopened = {
      open: await demoPage.evaluate(() => document.getElementById("settings-dialog").open),
      pressed: await pressedChips(),
    };
    await demoPage.click("#settings-close");
    const closedAgain = await demoPage.evaluate(
      () => document.getElementById("settings-dialog").open,
    );

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
      revealed,
      rehidden,
      revealDropped,
      preview,
      previewUnchecked,
      previewRestored,
      previewState,
      resorted,
      cleared,
      reselected,
      midScan,
      scanned,
      report,
      copied,
      barLoaded,
      barChanging,
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
      zeroThreshold,
      beforeCancel,
      cancelled,
      cancelledSelection,
      pageScrolls,
      damagedListing,
      persisted,
      theme: { autoDark, autoLight, overrideDark, overrideLight, booted, reopened, closedAgain },
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

  // The new-surface checks: rejection, codecs depth, and the sanitized name.
  const surfaceOk =
    typeof surface.rejected === "string" &&
    surface.rejected.includes("shortPlaylistSeconds") &&
    surface.codecsMeasured === false &&
    surface.codecsVideo.includes("High Profile 4.1") &&
    surface.fileName === "BDINFO.a_b_c.txt";
  if (surfaceOk) {
    console.log("PASS — Worker rejection, codecs inspect and reportFileName behave.");
  } else {
    console.error(`FAIL — option/API surface: ${JSON.stringify(surface)}`);
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
  // The disc-info strip. This disc declares no title and carries no feature
  // flag, and none of its playlists hides a stream, so the size line is the
  // whole strip — and it counts every file the page was handed, the six of the
  // fixture plus the two patched playlists above.
  const discBytes = entries.reduce(
    (total, item) => total + Buffer.from(item.b64, "base64").length,
    0,
  );
  const mplsBytes = Buffer.from(
    entries.find((item) => item.path.endsWith("00000.mpls")).b64,
    "base64",
  ).length;
  const strip = `Disc Size: ${(discBytes + mplsBytes * 2 + 14).toLocaleString("en-US")} bytes (10.63 MB)`;
  demoOk &= demoEq("the disc-info strip carries the size line alone", demo.initial.info, [strip]);
  demoOk &= demoEq("an unencrypted disc shows no badge", demo.initial.badgeHidden, true);
  // The source card: a loaded disc is named by the slim bar, and the dropzone
  // it replaces is put away until "Change disc…" asks for it.
  demoOk &= demoEq("a loaded disc shows the source bar", demo.initial.picked, "WASMDISC");
  demoOk &= demoEq("the dropzone stands down while loaded", demo.initial.dropzoneHidden, true);
  demoOk &= demoEq("the report is offered before any scan", demo.initial.viewDisabled, false);
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

  // The transient reveal: the withheld playlist joins the table as an ordinary
  // row, the line says so and offers the way back, and the stored setting that
  // withheld it is untouched — which is what makes the reveal transient rather
  // than a fourth filter switch. A settings change then drops it.
  demoOk &= demoEq("Show reveals the withheld playlist", demo.revealed.names, [
    "00000.MPLS",
    "00001.MPLS [02 Chapters]",
    "00002.MPLS",
  ]);
  demoOk &= demoEq(
    "the revealed line says so and offers Hide",
    [demo.revealed.hintText, demo.revealed.revealLabel],
    ["Showing filtered playlists (short): 00002.MPLS - enable in settings to keep", "Hide"],
  );
  demoOk &= demoEq("the reveal never touched the stored setting", demo.revealed.stored, false);
  demoOk &= demoEq(
    "Hide takes the playlist back out",
    [demo.rehidden.names.length, demo.rehidden.revealLabel, demo.rehidden.hintText],
    [2, "Show", "Hidden by filters (short): 00002.MPLS - enable in settings"],
  );
  demoOk &= demoEq(
    "a settings change drops the reveal",
    [demo.revealDropped.names.length, demo.revealDropped.revealLabel],
    [2, "Show"],
  );

  // The pre-scan report: the same render over a disc nothing has measured. It
  // prints the rows the table is showing, in the table's own order — the
  // name-ascending sort in force here, not the disc's presentation order, which
  // would have put the 40 s playlist first — and it follows the selection while
  // it is shown, at the cost of one render each time that selection moves.
  const blocks = (text) => text.split("\r\n").filter((line) => line.startsWith("PLAYLIST: "));
  demoOk &= demoEq(
    "the pre-scan report prints the table's rows in its order",
    blocks(demo.preview),
    ["PLAYLIST: 00000.MPLS", "PLAYLIST: 00001.MPLS"],
  );
  // The measured Movie Size of this clip: in the scan's report, and in nothing
  // a render before it could know.
  demoOk &= demoEq(
    "the pre-scan report measures nothing",
    [demo.preview.includes("11,064,384"), demo.report.includes("11,064,384")],
    [false, true],
  );
  demoOk &= demoEq("unticking a row drops its block", blocks(demo.previewUnchecked), [
    "PLAYLIST: 00001.MPLS",
  ]);
  demoOk &= compare(
    "restoring the selection renders the same report again",
    demo.previewRestored,
    Buffer.from(demo.preview),
  );
  demoOk &= demoEq("the pre-scan report cost renders only", demo.previewState.calls, [
    ...demo.preScan.calls,
    "render",
    "render",
    "render",
  ]);
  demoOk &= demoEq("no scan ran for it", demo.previewState.progressHidden, true);
  demoOk &= demoEq("re-ordering the table re-renders it", demo.resorted.calls, [
    ...demo.previewState.calls,
    "render",
  ]);
  demoOk &= demoEq(
    "a selection change that leaves the scan set alone renders nothing",
    [demo.cleared.calls, demo.reselected.calls],
    [demo.resorted.calls, demo.resorted.calls],
  );

  // The empty selection: the button is live and the count says the scan covers
  // everything, and ticking the rows again returns the plain count.
  demoOk &= demoEq(
    "an empty selection still scans",
    demo.cleared.selCount,
    "0 selected — scans all",
  );
  demoOk &= demoEq("the button is live at nothing selected", demo.cleared.scanDisabled, false);
  demoOk &= demoEq("a selection counts itself", demo.reselected.selCount, "2 selected");
  demoOk &= demoEq("the controls are live between scans", demo.preScan.sortFrozen, [
    false,
    false,
    false,
    false,
    false,
    false,
  ]);
  demoOk &= demoEq("the boxes are live between scans", demo.preScan.boxesDisabled, [false, false]);

  // The mid-scan freeze: the scan set cannot move under the running scan, the
  // readout waits for the first progress event, and the active row stays live.
  demoOk &= demoEq("a running scan disables the boxes", demo.midScan.frozen.boxesDisabled, [
    true,
    true,
  ]);
  demoOk &= demoEq("a running scan freezes every sort header", demo.midScan.frozen.sortFrozen, [
    true,
    true,
    true,
    true,
    true,
    true,
  ]);
  demoOk &= demoEq(
    "a running scan disables the selection buttons",
    [
      demo.midScan.frozen.selectAllDisabled,
      demo.midScan.frozen.clearDisabled,
      demo.midScan.frozen.scanDisabled,
    ],
    [true, true, true],
  );
  // The reveal and the pre-scan report name the scan set too, so they are
  // frozen with the rest of it.
  demoOk &= demoEq(
    "a running scan disables the reveal and the report button",
    [demo.midScan.frozen.revealDisabled, demo.midScan.frozen.viewDisabled],
    [true, true],
  );
  demoOk &= demoEq("the progress card is up", demo.midScan.frozen.progressHidden, false);
  demoOk &= demoEq("the readout is blank before the first event", demo.midScan.frozen.times, "");
  demoOk &= demoEq("a frozen sort header does not re-order", demo.midScan.afterEdits.names, [
    "00001.MPLS [02 Chapters]",
    "00000.MPLS",
  ]);
  demoOk &= demoEq(
    "frozen selection edits change nothing",
    demo.midScan.afterEdits.selCount,
    "2 selected",
  );
  demoOk &= demoEq("the active row stays live mid-scan", demo.midScan.activated, {
    active: "00001.MPLS",
    paneLabel: "00001.MPLS",
  });
  demoOk &= demoEq("the scan releases the controls", demo.scanned.boxesDisabled, [false, false]);
  demoOk &= demoEq("the scan releases the headers", demo.scanned.sortFrozen, [
    false,
    false,
    false,
    false,
    false,
    false,
  ]);
  // Digits normalized: the times themselves depend on how long the scan took,
  // the shape does not — and it is a real estimate, not the `--:--:--` of a
  // scan that never measured a byte.
  demoOk &= demoEq(
    "the readout keeps the last elapsed and remaining",
    demo.scanned.times.replace(/\d/g, "0"),
    "Elapsed 00:00:00 · Remaining 00:00:00",
  );
  demoOk &= demoEq("scan keeps the active row", demo.scanned.active, "00000.MPLS");
  demoOk &= demoEq("measured size fills after the scan", demo.scanned.files, [
    ["00000.M2TS", "1", "00:00:30", "10.63 MB", "10.55 MB"],
  ]);
  // The table's own measured column: empty before any scan, and filled for both
  // rows afterwards — they play the same clip, so they measure the same bytes.
  // Mid-scan the same cells tick from the scan's snapshots; the page reads them
  // here after it finished, where the numbers are the report's.
  demoOk &= demoEq("measured column starts empty", demo.initial.measured, ["—", "—"]);
  demoOk &= demoEq("measured column fills after the scan", demo.scanned.measured, [
    "10.55 MB",
    "10.55 MB",
  ]);
  // 973, not the report's 974: the pane integer-divides bits/s to kbps like the
  // desktop pane, where the report's codec table rounds.
  demoOk &= demoEq(
    "measured bit rates fill after the scan",
    demo.scanned.codecs.map((row) => row[2]),
    ["973 kbps", "1,536 kbps"],
  );
  demoOk &= demoEq("the demo report renders", demo.report.includes("QUICK SUMMARY:"), true);
  demoOk &= demoEq("the scan cost one scan call", demo.scanned.calls, [
    ...demo.reselected.calls,
    "scan",
  ]);

  // Ctrl+C, in the desktop app's precedence: a highlighted Stream File row wins
  // and names its clip's real stream file, an unhighlighted pane falls back to
  // the active playlist, and a Codec row copies nothing. The paths are
  // disc-relative — a browser page is given no location for what it was handed.
  demoOk &= demoEq("Ctrl+C copies the highlighted row's disc path", demo.copied.paths, [
    "BDMV/PLAYLIST/00000.MPLS",
    "BDMV/STREAM/00000.M2TS",
  ]);
  demoOk &= demoEq("the copied row is marked", demo.copied.flagged, true);

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
    ...demo.scanned.calls,
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
    ...demo.afterRenders.calls,
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

  // The zero threshold: accepted as a committed value (one more inspect), and
  // nothing on the disc is short under it.
  demoOk &= demoEq("a zero threshold costs one inspect", demo.zeroThreshold.calls, [
    ...demo.retentionOff.calls,
    "inspect",
  ]);
  demoOk &= demoEq("a zero threshold leaves nothing short", demo.zeroThreshold.names, [
    "00001.MPLS [02 Chapters]",
    "00000.MPLS",
    "00002.MPLS",
  ]);
  demoOk &= demoEq("a zero threshold shows no short hint", demo.zeroThreshold.hintHidden, true);

  // The cancelled whole-disc scan: the request named every listed row without a
  // tick anywhere, the cells it managed to tick are still standing, and no
  // report was rendered from them. The controls are released again.
  demoOk &= demoEq("nothing measured before the cancelled scan", demo.beforeCancel.measured, [
    "—",
    "—",
    "—",
  ]);
  demoOk &= demoEq("an empty selection asks for every listed row", demo.cancelledSelection, [
    "00001.MPLS",
    "00000.MPLS",
    "00002.MPLS",
  ]);
  demoOk &= demoEq(
    "a cancelled scan keeps what it measured",
    demo.cancelled.measured.every((text) => text !== "—"),
    true,
  );
  demoOk &= demoEq("a cancelled scan renders no report", demo.cancelled.reportHidden, true);
  demoOk &= demoEq("a cancelled scan releases the boxes", demo.cancelled.boxesDisabled, [
    false,
    false,
    false,
  ]);

  demoOk &= demoEq("no sideways page scroll at phone width", demo.pageScrolls, false);

  // "Change disc…" is the way back to the dropzone, and it is only that: the
  // disc the page is showing stays exactly where it is until a new pick
  // replaces it, and that pick lands on the source bar again.
  demoOk &= demoEq(
    "Change disc… puts the dropzone back",
    [demo.barChanging.picked, demo.barChanging.dropzoneHidden],
    [null, false],
  );
  demoOk &= demoEq("it leaves the loaded disc alone", demo.barChanging.names, demo.barLoaded.names);
  demoOk &= demoEq(
    "a fresh pick lands on the source bar again",
    [demo.damagedListing.picked, demo.damagedListing.dropzoneHidden],
    ["WASMDISC", true],
  );

  // The failure strip: absent for healthy media, and on a damaged listing it
  // names the file and says what was wrong with it, in the report's wording.
  demoOk &= demoEq("healthy media shows no failure strip", demo.initial.errorsHidden, true);
  demoOk &= demoEq("a damaged listing shows the strip", demo.damagedListing.errorsHidden, false);
  demoOk &= demoEq(
    "the strip counts the failures",
    demo.damagedListing.errorsCount,
    "Recorded 1 error — the readable rest is shown.",
  );
  demoOk &= demoEq("the strip names the failure", demo.damagedListing.errorLines, [
    "playlist 00009.mpls: unknown file type: NOPE0100",
  ]);
  // Retention was switched off above, so the reload proves the stored `false`
  // is read back rather than defaulted to on like an absent setting; the
  // stored 0 threshold likewise comes back as the committed 0, not the default.
  demoOk &= demoEq("settings survive a reload", demo.persisted, {
    seconds: "0",
    short: false,
    looping: false,
    human: true,
    chapters: true,
    diagnostics: true,
    summary: true,
    keepPartial: false,
  });
  // The theme: Auto follows the emulated OS both ways; a manual chip beats a
  // light OS, tints its own chip, and writes its ground into both theme-color
  // metas; a reload under a dark OS with a stored Light choice still boots
  // light — the attribute is already set when the page's <style> appears, so
  // there is no dark flash — while the report well keeps its dark ground; and
  // the settings dialog reopens with the stored chip pressed, then closes.
  demoOk &= demoEq("auto follows a dark OS", demo.theme.autoDark, "rgb(11, 13, 16)");
  demoOk &= demoEq("auto follows an OS flip to light", demo.theme.autoLight, "rgb(244, 246, 249)");
  demoOk &= demoEq(
    "the dark chip beats the light OS",
    demo.theme.overrideDark.bg,
    "rgb(11, 13, 16)",
  );
  demoOk &= demoEq("the pressed chip is the choice", demo.theme.overrideDark.pressed, [
    "false",
    "false",
    "true",
  ]);
  demoOk &= demoEq("a manual choice rewrites both metas", demo.theme.overrideDark.metas, [
    "#0b0d10",
    "#0b0d10",
  ]);
  demoOk &= demoEq(
    "the light chip repaints the ground",
    demo.theme.overrideLight,
    "rgb(244, 246, 249)",
  );
  demoOk &= demoEq("the stored choice is applied before the style exists", demo.theme.booted, {
    atStyle: "light",
    attr: "light",
    bg: "rgb(244, 246, 249)",
    reportBg: "rgb(8, 9, 11)",
  });
  demoOk &= demoEq("the dialog reopens with the stored chip pressed", demo.theme.reopened, {
    open: true,
    pressed: ["false", "true", "false"],
  });
  demoOk &= demoEq("the dialog closes again", demo.theme.closedAgain, false);
  demoOk = Boolean(demoOk);

  process.exit(listOk && inspectOk && scanOk && surfaceOk && isoStructuredOk && demoOk ? 0 : 1);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
