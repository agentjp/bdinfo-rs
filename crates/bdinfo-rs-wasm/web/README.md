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

## License

LGPL-2.1-only.
