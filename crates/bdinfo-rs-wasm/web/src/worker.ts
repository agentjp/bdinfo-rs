/// <reference lib="webworker" />
// The scan Worker: hosts the WebAssembly module OFF the main thread. It serves
// every request in `analyze.ts` over the same module instance:
//   - `inspect` / `inspect-iso`: the fast STRUCTURAL scan → the whole disc model.
//   - `scan-full` / `scan-iso-full`: the FULL measured scan → the rendered report
//     and the disc model, from one demux.
//   - `render`: re-render the report from a disc model — no media touched.
//   - `list` / `list-iso` and `scan` / `scan-iso`: what the deprecated 2.0 calls
//     need — the selection table (parsed here, so the main thread receives
//     structured rows, not JSON text) and the report alone.
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
  list_iso_playlists,
  list_playlists,
  render_report,
  scan_files,
  scan_files_full,
  scan_iso,
  scan_iso_full,
} from "../pkg/bdinfo_rs_wasm.js";

/** The two playlist-filter opt-outs a listing request carries (see `analyze.ts`). */
interface ListOptions {
  showShortPlaylists: boolean;
  showLoopingPlaylists: boolean;
}

/**
 * The optional report sections a rendering request carries. Both are `true`
 * unless the caller switched one off, which is the report the CLI writes.
 */
interface ReportOptions {
  streamDiagnostics: boolean;
  quickSummary: boolean;
}

/** List the playlists (structural scan) of the picked BDMV folder. */
interface ListRequest extends ListOptions {
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

/** List the playlists (structural scan) of a single picked `.iso`. */
interface ListIsoRequest extends ListOptions {
  kind: "list-iso";
  file: File;
}

/** Measure a single picked `.iso` (`selection` by name; empty = `--whole`). */
interface ScanIsoRequest {
  kind: "scan-iso";
  file: File;
  selection: string[];
}

/**
 * The whole disc model of the picked BDMV folder from a structural scan.
 * `shortPlaylistSeconds` is the length under which a playlist counts as short;
 * zero means the wasm module's 20 s default.
 */
interface InspectRequest extends ListOptions {
  kind: "inspect";
  paths: string[];
  files: File[];
  shortPlaylistSeconds: number;
}

/** The whole disc model of a single picked `.iso` from a structural scan. */
interface InspectIsoRequest extends ListOptions {
  kind: "inspect-iso";
  file: File;
  shortPlaylistSeconds: number;
}

/** Measure the picked BDMV folder, answering with the report AND the model. */
interface ScanFullRequest extends ReportOptions {
  kind: "scan-full";
  paths: string[];
  files: File[];
  selection: string[];
}

/** Measure a single picked `.iso`, answering with the report AND the model. */
interface ScanIsoFullRequest extends ReportOptions {
  kind: "scan-iso-full";
  file: File;
  selection: string[];
}

/** Re-render the report from a disc model a measured scan already produced. */
interface RenderRequest extends ReportOptions {
  kind: "render";
  disc: Disc;
}

type Request =
  | ListRequest
  | ScanRequest
  | ListIsoRequest
  | ScanIsoRequest
  | InspectRequest
  | InspectIsoRequest
  | ScanFullRequest
  | ScanIsoFullRequest
  | RenderRequest;

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
      case "list":
        self.postMessage({
          type: "rows",
          rows: JSON.parse(
            list_playlists(
              data.paths,
              data.files,
              data.showShortPlaylists,
              data.showLoopingPlaylists,
            ),
          ),
        });
        break;
      case "list-iso":
        self.postMessage({
          type: "rows",
          rows: JSON.parse(
            list_iso_playlists(data.file, data.showShortPlaylists, data.showLoopingPlaylists),
          ),
        });
        break;
      case "scan":
        self.postMessage({
          type: "done",
          report: scan_files(data.paths, data.files, data.selection, onProgress),
        });
        break;
      case "scan-iso":
        self.postMessage({
          type: "done",
          report: scan_iso(data.file, data.selection, onProgress),
        });
        break;
      case "inspect":
        self.postMessage({
          type: "disc",
          disc: inspect_files(
            data.paths,
            data.files,
            data.showShortPlaylists,
            data.showLoopingPlaylists,
            data.shortPlaylistSeconds,
          ),
        });
        break;
      case "inspect-iso":
        self.postMessage({
          type: "disc",
          disc: inspect_iso(
            data.file,
            data.showShortPlaylists,
            data.showLoopingPlaylists,
            data.shortPlaylistSeconds,
          ),
        });
        break;
      case "scan-full":
        self.postMessage({
          type: "result",
          result: scan_files_full(
            data.paths,
            data.files,
            data.selection,
            onProgress,
            data.streamDiagnostics,
            data.quickSummary,
          ),
        });
        break;
      case "scan-iso-full":
        self.postMessage({
          type: "result",
          result: scan_iso_full(
            data.file,
            data.selection,
            onProgress,
            data.streamDiagnostics,
            data.quickSummary,
          ),
        });
        break;
      case "render":
        self.postMessage({
          type: "done",
          report: render_report(data.disc, data.streamDiagnostics, data.quickSummary),
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
