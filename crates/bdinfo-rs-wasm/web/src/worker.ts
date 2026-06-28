/// <reference lib="webworker" />
// The scan Worker: hosts the WebAssembly module OFF the main thread and runs the
// FULL measured scan over a webkitdirectory-picked BDMV folder. The wasm reads
// each file's bytes synchronously at byte offsets through `FileReaderSync` (the
// reason this must be a Worker — that API exists only in a Worker scope), so a
// multi-GB stream never has to fit in memory. Progress is forwarded to the main
// thread as it demuxes; the rendered report is posted back when done.
import init, { scan_files } from "../pkg/bdinfo_rs_wasm.js";

/** A `(relativePath, File)` pair — one file of the picked BDMV folder. */
interface ScanRequest {
  paths: string[];
  files: File[];
}

let ready: Promise<unknown> | null = null;

self.onmessage = async (event: MessageEvent<ScanRequest>) => {
  try {
    // Instantiate the wasm module once (its default export fetches the `.wasm`).
    if (ready === null) {
      ready = init();
    }
    await ready;

    const { paths, files } = event.data;
    const report = scan_files(paths, files, (file: string, done: number, total: number) => {
      self.postMessage({ type: "progress", file, done, total });
    });
    self.postMessage({ type: "done", report });
  } catch (error) {
    self.postMessage({
      type: "error",
      message: error instanceof Error ? error.message : String(error),
    });
  }
};
