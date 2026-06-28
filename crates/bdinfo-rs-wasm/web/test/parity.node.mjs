// Node golden-parity test — no browser, no driver.
//
// Loads the BUILT, wasm-opt'd module (pkg/) straight into Node via `initSync`
// (the synchronous byte-init path, no fetch), shims the three browser globals the
// streaming export touches (`File`, `FileReaderSync`, plus `.size`/`.slice`), and
// drives the SAME production export the Worker uses — `scan_files` over a
// `(relativePath, File)` list built from the committed Big Buck Bunny BD-ROM
// fixture. It then asserts the rendered report is BYTE-IDENTICAL to the pinned
// golden (`tests/golden_report.txt`) — the very golden the native CLI e2e test
// and the in-browser parity test both pin. So this ties the wasm channel to the
// locked-output contract on every gate run, with only Node + the built wasm.
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

async function main() {
  const golden = await readFile(goldenPath);

  const { initSync, scan_files } = await import("../pkg/bdinfo_rs_wasm.js");
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

  const report = scan_files(paths, files);
  const got = Buffer.from(report, "utf8");

  if (got.equals(golden)) {
    console.log(`PASS — Node measured scan matches the golden (${golden.length} bytes).`);
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
