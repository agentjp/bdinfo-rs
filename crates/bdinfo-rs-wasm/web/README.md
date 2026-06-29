# @bdinfo-rs/wasm

[![npm](https://img.shields.io/npm/v/@bdinfo-rs/wasm)](https://www.npmjs.com/package/@bdinfo-rs/wasm)
[![license](https://img.shields.io/npm/l/@bdinfo-rs/wasm)](./LICENSE)

In-browser [Blu-ray disc analyzer](https://github.com/agentjp/bdinfo-rs) — the
**bdinfo-rs** measured scan compiled to WebAssembly. Point it at a disc's `BDMV`
folder and it runs the full measured scan (M2TS demux + per-stream/per-chapter
statistics) entirely in the browser, off the main thread in a Web Worker. **No
bytes leave the page**, and a multi-GB `*.m2ts` never has to fit in memory — the
files are read synchronously at byte offsets via `FileReaderSync`.

The rendered report is byte-for-byte the classic disc report, pinned to its own
golden — rendered from the same Big Buck Bunny fixture the native end-to-end test
scans, and held byte-identical across native, Node, and headless Chrome and
Firefox.

## Install

```sh
npm i @bdinfo-rs/wasm
```

The published payload is **~360 KB of WebAssembly + ~22 KB of JS**. Only the
main-thread entry you import (~4 KB) loads up front; the scan Worker (~2 KB) and
the wasm-bindgen glue (~17 KB) that hosts the `.wasm` are fetched lazily inside
the Worker, and nothing past the entry loads at all until you call `analyze` or
`listPlaylists`.

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
in the source repository for a complete vanilla example (the demo is not shipped
in the npm package).

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

If your bundler can't follow that pattern, host the Worker yourself and pass its
URL explicitly. The package's `exports` map deliberately keeps the internals
private (`dist/worker.js` and `pkg/` are not importable subpaths), so copy
`dist/worker.js` **together with the `pkg/` directory** out of `node_modules`
into your own source, preserving their relative layout — `worker.js` loads the
wasm-bindgen glue and `.wasm` via `import "../pkg/bdinfo_rs_wasm.js"`, so `pkg/`
must stay one level below it. Then pass the URL your bundler produces for the
copied worker:

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

**LGPL-2.1-or-later.** This package is a single WebAssembly module that
statically links `bdinfo-rs-core` (itself a Rust port of, and derivative work
based on, [BDInfo](https://github.com/UniqProject/BDInfo) © 2010 Cinema Squid),
so the whole package is covered by the GNU Lesser General Public License,
version 2.1 or (at your option) any later version.

The tarball ships the full license text (`LICENSE`) and the attribution and
derivative-work notice (`NOTICE`). The **complete corresponding source** for the
linked code is the public repository at the matching release tag —
`https://github.com/agentjp/bdinfo-rs` at `v<this package's version>` — from
which the `.wasm` is built (`crates/bdinfo-rs-wasm`).
