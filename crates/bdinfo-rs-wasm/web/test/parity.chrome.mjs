// Headless-Chrome Worker parity test.
//
// Serves the built package over HTTP, opens it in headless Chrome (the system
// Google Chrome, via Playwright's `channel: "chrome"`), hands the page the
// committed Big Buck Bunny BD-ROM fixture as in-browser `File` objects, runs the
// FULL measured scan in the Worker (the `FileReaderSync` byte-offset path), and
// asserts the returned report is BYTE-IDENTICAL to the pinned native golden
// (`tests/golden_report.txt`) — the same golden the native ⇄ wasm parity test pins.
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

  let report;
  try {
    const page = await browser.newPage();
    page.on("console", (msg) => console.log(`  [page] ${msg.text()}`));
    page.on("pageerror", (err) => console.log(`  [pageerror] ${err.message}`));

    await page.goto(`${base}/test/harness.html`);
    await page.waitForFunction(() => window.__ready === true, { timeout: 30000 });

    report = await page.evaluate(async (items) => {
      const files = items.map((item) => {
        const binary = atob(item.b64);
        const bytes = new Uint8Array(binary.length);
        for (let i = 0; i < binary.length; i++) {
          bytes[i] = binary.charCodeAt(i);
        }
        const name = item.path.split("/").pop();
        return { path: item.path, file: new File([bytes], name) };
      });
      return await window.__analyze(files);
    }, entries);
  } finally {
    await browser.close();
    server.close();
  }

  const got = Buffer.from(report, "utf8");
  if (got.equals(golden)) {
    console.log(`PASS — Worker measured scan matches the golden (${golden.length} bytes).`);
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
