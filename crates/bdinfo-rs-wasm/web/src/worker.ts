/// <reference lib="webworker" />
// The scan Worker: hosts the WebAssembly module OFF the main thread. It serves
// every request in `analyze.ts` over the same module instance:
//   - `inspect` / `inspect-iso`: the STRUCTURAL scan (optionally deepened to
//     the bounded codec pass by `options.codecs`) → the whole disc model.
//   - `scan` / `scan-iso`: the FULL measured scan → the rendered report and the
//     disc model, from one demux.
//   - `render`: re-render the report from a disc model — no media touched.
//   - `file-name`: the sanitized report save-file name for a disc label.
// The wasm reads each file's bytes synchronously at byte offsets through
// `FileReaderSync` (the reason this must be a Worker — that API exists only in a
// Worker scope), so a multi-GB stream never has to fit in memory. Progress is
// forwarded to the main thread as it demuxes; the answer is posted back when
// done.
//
// This module is internal: `dist/worker.js` is not an importable subpath of the
// package, so the request and reply shapes below are free to change with the
// `analyze.ts` that speaks them.
import init, {
  type Disc,
  inspect_files,
  inspect_iso,
  render_report,
  report_file_name,
  type ScanOptions,
  scan_files,
  scan_iso,
} from "../pkg/bdinfo_rs_wasm.js";

/**
 * The options object every request carries, built in `analyze.ts` and handed to
 * the module as it stands. Each option is optional and each call reads the ones
 * it names, so one shape serves every request here.
 */
interface WithOptions {
  options: ScanOptions;
}

/** The whole disc model of the picked BDMV folder from a structural scan. */
interface InspectRequest extends WithOptions {
  kind: "inspect";
  paths: string[];
  files: File[];
}

/** The whole disc model of a single picked `.iso` from a structural scan. */
interface InspectIsoRequest extends WithOptions {
  kind: "inspect-iso";
  file: File;
}

/**
 * Measure the picked BDMV folder, answering with the report AND the model.
 * `selection` names the playlists to measure; empty is the `--whole` set.
 */
interface ScanRequest extends WithOptions {
  kind: "scan";
  paths: string[];
  files: File[];
  selection: string[];
}

/** Measure a single picked `.iso`, answering with the report AND the model. */
interface ScanIsoRequest extends WithOptions {
  kind: "scan-iso";
  file: File;
  selection: string[];
}

/** Re-render the report from a disc model a measured scan already produced. */
interface RenderRequest extends WithOptions {
  kind: "render";
  disc: Disc;
}

/** The sanitized report save-file name for a disc label. */
interface FileNameRequest extends WithOptions {
  kind: "file-name";
  label: string;
}

type Request =
  | InspectRequest
  | InspectIsoRequest
  | ScanRequest
  | ScanIsoRequest
  | RenderRequest
  | FileNameRequest;

let ready: Promise<unknown> | null = null;

self.onmessage = async (event: MessageEvent<Request>) => {
  try {
    // Instantiate the wasm module once (its default export fetches the `.wasm`).
    if (ready === null) {
      ready = init();
    }
    await ready;

    const data = event.data;
    const onProgress = (file: string, done: number, total: number) => {
      self.postMessage({ type: "progress", file, done, total });
    };
    switch (data.kind) {
      case "inspect":
        self.postMessage({
          type: "disc",
          disc: inspect_files(data.paths, data.files, data.options),
        });
        break;
      case "inspect-iso":
        self.postMessage({
          type: "disc",
          disc: inspect_iso(data.file, data.options),
        });
        break;
      case "scan":
        self.postMessage({
          type: "result",
          result: scan_files(data.paths, data.files, data.selection, onProgress, data.options),
        });
        break;
      case "scan-iso":
        self.postMessage({
          type: "result",
          result: scan_iso(data.file, data.selection, onProgress, data.options),
        });
        break;
      case "render":
        self.postMessage({
          type: "done",
          report: render_report(data.disc, data.options),
        });
        break;
      case "file-name":
        self.postMessage({
          type: "file-name",
          name: report_file_name(data.label),
        });
        break;
    }
  } catch (error) {
    self.postMessage({
      type: "error",
      message: error instanceof Error ? error.message : String(error),
    });
  }
};
