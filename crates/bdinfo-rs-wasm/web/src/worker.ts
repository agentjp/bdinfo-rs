/// <reference lib="webworker" />
// The scan Worker: hosts the WebAssembly module OFF the main thread. It serves
// two requests over the same module instance:
//   - `list`: the fast STRUCTURAL scan → the playlist selection table (JSON).
//   - `scan`: the FULL measured scan over the picked (or selected) playlists.
// The wasm reads each file's bytes synchronously at byte offsets through
// `FileReaderSync` (the reason this must be a Worker — that API exists only in a
// Worker scope), so a multi-GB stream never has to fit in memory. Progress is
// forwarded to the main thread as it demuxes; the rendered report (or the table
// rows) is posted back when done.
import init, { list_playlists, scan_files } from "../pkg/bdinfo_rs_wasm.js";

/** List the playlists (structural scan) of the picked BDMV folder. */
interface ListRequest {
  kind: "list";
  paths: string[];
  files: File[];
}

/** Measure `selection` (by playlist name; empty = the `--whole` set). */
interface ScanRequest {
  kind: "scan";
  paths: string[];
  files: File[];
  selection: string[];
}

type Request = ListRequest | ScanRequest;

let ready: Promise<unknown> | null = null;

self.onmessage = async (event: MessageEvent<Request>) => {
  try {
    // Instantiate the wasm module once (its default export fetches the `.wasm`).
    if (ready === null) {
      ready = init();
    }
    await ready;

    const data = event.data;
    if (data.kind === "list") {
      const rows = list_playlists(data.paths, data.files);
      self.postMessage({ type: "rows", rows });
    } else {
      const report = scan_files(
        data.paths,
        data.files,
        data.selection,
        (file: string, done: number, total: number) => {
          self.postMessage({ type: "progress", file, done, total });
        },
      );
      self.postMessage({ type: "done", report });
    }
  } catch (error) {
    self.postMessage({
      type: "error",
      message: error instanceof Error ? error.message : String(error),
    });
  }
};
