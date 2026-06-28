# @bdinfo-rs/wasm

In-browser [Blu-ray disc analyzer](https://github.com/agentjp/bdinfo-rs) — the
**bdinfo-rs** measured scan compiled to WebAssembly. Point it at a disc's `BDMV`
folder and it runs the full measured scan (M2TS demux + per-stream/per-chapter
statistics) entirely in the browser, off the main thread in a Web Worker. **No
bytes leave the page**, and a multi-GB `*.m2ts` never has to fit in memory — the
files are read synchronously at byte offsets via `FileReaderSync`.

The rendered report is byte-for-byte the classic disc report the native CLI
writes — pinned to the same golden the native end-to-end test uses.

## Usage

```ts
import { analyze } from "@bdinfo-rs/wasm";

// `files`: the (relativePath, File) pairs from a <input type="file" webkitdirectory>.
const picked = [...input.files].map((file) => ({
  path: file.webkitRelativePath,
  file,
}));

const report = await analyze(picked, ({ file, done, total }) => {
  console.log(`${file}: ${done}/${total}`);
});

console.log(report); // the classic BDInfo-style disc report
```

`analyze` spawns the scan Worker, relays demux progress, and resolves with the
report string. See `demo.html` for a complete vanilla example.

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

## License

LGPL-2.1-only. The full license text ships in the package (`LICENSE`).
