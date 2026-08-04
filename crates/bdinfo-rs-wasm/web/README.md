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

The published payload is **~497 KB of WebAssembly + ~44 KB of JS**. Only the
main-thread entry you import (~9 KB) loads up front; the scan Worker (~3 KB)
and the wasm-bindgen glue (~32 KB) that hosts the `.wasm` are fetched lazily
inside the Worker, and nothing past the entry loads at all until the first scan.

## Usage

Three calls mirror the CLI flow — inspect the disc, measure the playlists you
want, re-render the report as you like. Each takes either a picked BDMV folder
(as `(relativePath, File)` pairs) or a single Blu-ray `.iso` `File`, and each
runs in the browser, off the main thread:

```ts
import { inspect, renderReport, scan } from "@bdinfo-rs/wasm";

// `picked`: the (relativePath, File) pairs from a <input type="file" webkitdirectory>.
const picked = [...input.files].map((file) => ({
  path: file.webkitRelativePath,
  file,
}));

// 1. Fast STRUCTURAL scan (like `--list`) → the whole disc model, no demux.
const disc = await inspect(picked);
for (const playlist of disc.playlists) {
  console.log(`${playlist.name}  ${playlist.totalLengthSeconds}s  ${playlist.chapterCount} ch`);
}

// 2. FULL measured scan → the classic report AND the same scan as data, from
//    one demux. Pass `selection` (playlist names, like `--mpls`) to measure only
//    chosen playlists; omit it to measure the `--whole` set.
const measured = await scan(
  picked,
  ({ file, done, total }) => console.log(`${file}: ${done}/${total}`),
  { selection: [disc.playlists[0].name] },
);

console.log(measured.report); // the classic BDInfo-style disc report

// 3. Re-render that report with different sections — no media, no rescan.
const brief = await renderReport(measured.disc, { quickSummary: false });
```

An `.iso` goes through the same three calls; pass the `File` instead of the
list, and the image is opened through the read-only UDF reader:

```ts
const disc = await inspect(isoInput.files[0]);
const { report } = await scan(isoInput.files[0]);
```

### The disc model

`inspect` and `scan` both give you a `Disc`: the disc-level properties
(`volumeLabel`, `discTitle`, `sizeBytes`, `is3d`, `isUhd`, …) and every
`Playlist` on it, each carrying its `Stream`s, `Clip`s and `Chapter`s. Values
cross as raw numbers with unit-bearing names — `bitrateBps`, `sampleRateHz`,
`heightPixels`, `lengthSeconds` — so your UI can sort, filter and chart them
rather than parse report text. The types (`Disc`, `Playlist`, `Stream`, `Clip`,
`ClipStream`, `Chapter`, `ScanError`, `HiddenRule`, `ScanResult`) are exported
from the package entry and generated from the Rust definitions.

`disc.playlists` is in the disc's own file-name order and holds **every**
playlist. Each one also carries where it sits in the classic selection table —
`group` (shared-clip group, from 1), `position` (table order, from 1) and
`hiddenBy` — so you can build that table without reimplementing its grouping:

```ts
const table = disc.playlists
  .filter((playlist) => playlist.hiddenBy.length === 0)
  .sort((a, b) => a.position - b.position);
```

`totalLengthTicks` is `totalLengthSeconds` in the 100 ns ticks the report
formats its times from. Integer-divide it by 10,000,000 for the table's
`hh:mm:ss` cell; computing that from the `f64` seconds can land a tick either
side.

`disc.measured` tells the two scans apart: `false` after `inspect`, where every
measured value — bitrates, packet counts, chapter rates — is zero because
nothing measured it; `true` after `scan`, where a zero is a genuine zero.

`disc.isAacsEncrypted` says the disc's stream content is AACS-encrypted. Neither
call throws for it: the structure — playlists, streams as the disc declares
them, chapters — comes from cleartext metadata and is correct either way. Only
the stream content is unreadable, so a measured `scan` of such a disc demuxes
ciphertext and every value it measures is meaningless. What to do about that is
yours to decide; the demo tells the user and offers no measured scan.

### Re-rendering the report

The model carries **every value the report prints**, so `renderReport(disc)`
reproduces the report that `scan` returned byte for byte — a render, not an
approximation, pinned against the same golden the scan itself is pinned to. The
`Disc` is therefore the thing worth keeping: store it and every rendering of the
report stays one call away, with no media and no rescan.

```ts
const { report, disc } = await scan(picked);
// later, from the held disc alone — the same bytes, minus one section:
const trimmed = await renderReport(disc, { streamDiagnostics: false });
```

`streamDiagnostics` and `quickSummary` both default to on, which is the report
the CLI writes. A `disc` from a `scan` with a `selection` holds every playlist
but measured values only for the ones that scan named, so re-rendering it prints
the rest at zero — scan again to measure them.

### Playlist filtering

The classic report withholds playlists shorter than 20 seconds and looping
ones. This package never withholds anything: every playlist crosses, and each
carries the rules that classify it as withheld in `hiddenBy` — `"short"`,
`"looping"`, both, or none. Filtering is therefore a client-side array
operation, instant and rescan-free:

```ts
const disc = await inspect(picked);
const standard = disc.playlists.filter((playlist) => playlist.hiddenBy.length === 0);
const withoutShort = disc.playlists.filter(
  (playlist) => !playlist.hiddenBy.includes("short"),
);
```

`shortPlaylistSeconds` moves the length threshold behind `"short"`. It is the
one filter setting that has to be passed to the call, because it changes the
classification rather than the view — pass it to `inspect` or `scan` and every
playlist is judged against it:

```ts
const disc = await inspect(picked, { shortPlaylistSeconds: 5 });
```

Nothing else moves with it: which playlists a `Disc` holds, which ones a
`selection` measures, and the rendered report are the same either way.

### Cancelling

Pass an `AbortSignal`. Aborting it terminates the scan Worker and rejects the
promise with an `AbortError`, so a user cancel is distinguishable from a real
failure by the rejection's `name`:

```ts
const controller = new AbortController();
const { report } = await scan(picked, undefined, { signal: controller.signal });
```

See `index.html` in the source repository for a complete vanilla example (the
demo is not shipped in the npm package).

## Bundler support

This is an **ES-modules-only**, browser-only package (no CommonJS build). It runs
the scan off the main thread, so it ships **two assets the analyzer loads at
runtime**: the Web Worker (`dist/worker.js`) and the WebAssembly module
(`pkg/bdinfo_rs_wasm_bg.wasm`, fetched by the Worker). Your toolchain must emit
both as addressable assets.

Every call spawns the Worker with the standard

```ts
new Worker(new URL("./worker.js", import.meta.url), { type: "module" });
```

pattern. Any bundler that understands it works out of the box:

- **Vite** — handled natively (it rewrites the `new URL(..., import.meta.url)`
  worker reference and emits the `.wasm` as an asset).
- **webpack 5** — handled natively (the same worker/asset detection).
- **Native ES modules** (no bundler — served straight from the package on a
  static host or via an import map) — works as published.

If your bundler can't follow that pattern, host the Worker yourself and pass a
factory constructing it. The package's `exports` map deliberately keeps the
internals private (`dist/worker.js` and `pkg/` are not importable subpaths), so copy
`dist/worker.js` **together with the `pkg/` directory** out of `node_modules`
into your own source, preserving their relative layout — `worker.js` loads the
wasm-bindgen glue and `.wasm` via `import "../pkg/bdinfo_rs_wasm.js"`, so `pkg/`
must stay one level below it. Then construct the module Worker from the URL your
bundler produces for the copied worker:

```ts
import workerUrl from "./worker.js?worker&url"; // however your bundler exposes it

await scan(picked, onProgress, {
  createWorker: () => new Worker(workerUrl, { type: "module" }),
});
```

The raw wasm-bindgen module is also exported directly for advanced use:

```ts
import init, { scan_files } from "@bdinfo-rs/wasm/wasm";
```

It exports `inspect_files`, `inspect_iso`, `scan_files`, `scan_iso` and
`render_report` — what `inspect`, `scan` and `renderReport` call, minus the
Worker — plus one entry point the package deliberately does not wrap:
`scan_report(bytes)` takes a whole disc pre-framed into one length-prefixed
byte buffer and renders it from memory. It exists as the in-memory seam the
byte-parity tests drive through both the native and the browser build; it is
not a fourth way to scan a disc, and a real consumer has no such buffer.

Remember that every scanning export reads its bytes through `FileReaderSync`
and therefore has to run inside a Web Worker.

## Browser support

The scan needs two browser capabilities: **`<input type="file" webkitdirectory>`**
for the folder pick and **`FileReaderSync`** for synchronous byte-range reads
inside a Worker. Both are available on **desktop Chrome / Edge, desktop Firefox,
and Android Chrome**. The package's parity suite runs on **headless Chrome and
Firefox** (plus Node), so those are the verified engines; desktop **Safari**
exposes the same APIs but is **untested**. `FileReaderSync` is Worker-only by
design, which is why every call runs in a Web Worker and never on the main
thread.

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
Web Worker — the package handles that for you.

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
