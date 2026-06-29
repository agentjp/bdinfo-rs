# @bdinfo-rs/wasm

In-browser [Blu-ray disc analyzer](https://github.com/agentjp/bdinfo-rs) — the
**bdinfo-rs** measured scan compiled to WebAssembly. Point it at a disc's `BDMV`
folder and it runs the full measured scan (M2TS demux + per-stream/per-chapter
statistics) entirely in the browser, off the main thread in a Web Worker. **No
bytes leave the page**, and a multi-GB `*.m2ts` never has to fit in memory — the
files are read synchronously at byte offsets via `FileReaderSync`.

The rendered report is byte-for-byte the classic disc report the native CLI
writes — pinned to its own golden, rendered from the same Big Buck Bunny fixture
the native end-to-end test scans and held byte-identical across native, Node, and
headless Chrome and Firefox.

## Usage

The two calls mirror the CLI flow — list the playlists, pick some, then measure
them (all in the browser, off the main thread):

```ts
import { analyze, listPlaylists } from "@bdinfo-rs/wasm";

// `files`: the (relativePath, File) pairs from a <input type="file" webkitdirectory>.
const picked = [...input.files].map((file) => ({
  path: file.webkitRelativePath,
  file,
}));

// 1. Fast STRUCTURAL scan → the playlist selection table (like `--list`).
const playlists = await listPlaylists(picked);
for (const row of playlists) {
  console.log(`${row.position}. ${row.name}  ${row.length}  ${row.estimatedBytes ?? "-"} bytes`);
}

// 2. FULL measured scan. Pass `selection` (playlist names, like `--mpls`) to
//    measure only chosen playlists; omit it to measure the `--whole` set.
const report = await analyze(
  picked,
  ({ file, done, total }) => console.log(`${file}: ${done}/${total}`),
  { selection: [playlists[0].name] },
);

console.log(report); // the classic BDInfo-style disc report
```

`listPlaylists` resolves with the selection-table rows (`position`, `group`,
`name`, `length`, `estimatedBytes`, `hasHidden`); `analyze` spawns the scan
Worker, relays demux progress, and resolves with the report string. Omit both
`onProgress` and `selection` for the simplest whole-disc scan. See `index.html`
for a complete vanilla example.

## Bundler support

This is an **ES-modules-only**, browser-only package (no CommonJS build). It runs
the scan off the main thread, so it ships **two assets the analyzer loads at
runtime**: the Web Worker (`dist/worker.js`) and the WebAssembly module
(`pkg/bdinfo_rs_wasm_bg.wasm`, fetched by the Worker). Your toolchain must emit
both as addressable assets.

`analyze` spawns the Worker with the standard

```ts
new Worker(new URL("./worker.js", import.meta.url), { type: "module" });
```

pattern. Any bundler that understands it works out of the box:

- **Vite** — handled natively (it rewrites the `new URL(..., import.meta.url)`
  worker reference and emits the `.wasm` as an asset).
- **webpack 5** — handled natively (the same worker/asset detection).
- **Native ES modules** (no bundler — served straight from the package on a
  static host or via an import map) — works as published.

If your bundler can't follow that pattern, host `dist/worker.js` and
`pkg/bdinfo_rs_wasm_bg.wasm` yourself and pass the worker URL explicitly:

```ts
import workerUrl from "./worker.js?worker&url"; // however your bundler exposes it

await analyze(picked, onProgress, { workerUrl });
```

The raw wasm-bindgen module is also exported directly for advanced use:

```ts
import init, { scan_files } from "@bdinfo-rs/wasm/wasm";
```

## Browser support

The scan needs two browser capabilities: **`<input type="file" webkitdirectory>`**
for the folder pick and **`FileReaderSync`** for synchronous byte-range reads
inside a Worker. Both are available on **desktop Chrome / Edge, desktop Firefox,
and Android Chrome**. The package's parity suite runs on **headless Chrome and
Firefox** (plus Node), so those are the verified engines; desktop **Safari**
exposes the same APIs but is **untested**. `FileReaderSync` is Worker-only by
design, which is why `analyze` always runs the scan in a Web Worker and never on
the main thread.

**iOS is the one known gap:** iOS WebKit could not pick a folder on iOS ≤ 18.3
(the `webkitdirectory` bit was unimplemented; it shipped in iOS 18.4). Treat the
folder pick as progressive enhancement — when `webkitdirectory` is unavailable,
degrade gracefully to a plain multi-file picker (`<input type="file" multiple>`)
or drag-and-drop, and tell the user to select the disc's files individually or
update to iOS 18.4+.

## Content Security Policy

A `--target web` wasm module is compiled and instantiated at runtime, so a page
that sets a `script-src` (or `default-src`) CSP must allow WebAssembly with
**`'wasm-unsafe-eval'`** (the broader `'unsafe-eval'` also works); otherwise the
module is blocked. With no CSP, wasm runs freely. The scan itself must run in a
Web Worker — `analyze` handles that for you.

## License

LGPL-2.1-only. The full license text ships in the package (`LICENSE`).
