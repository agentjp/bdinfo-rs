// The package's public API. Two calls mirror the CLI flow, both running the
// bdinfo-rs scan entirely in the browser, off the main thread:
//
//   - `listPlaylists` — the fast STRUCTURAL scan: the playlist selection table
//     (like `bdinfo-rs <disc> --list`), so the UI can show a multi-select
//     checklist before the heavy work.
//   - `analyze` — the FULL measured scan over the picked folder; pass
//     `options.selection` to measure only chosen playlists (like `--mpls`),
//     or omit it to measure the standard `--whole` set.
//
// Each spawns the scan Worker (which hosts the WebAssembly module), hands it the
// `(relativePath, File)` pairs, and resolves with the result — the rendered
// classic disc report (the very bytes the native CLI writes to
// `BDINFO.<label>.txt`), or the selection-table rows.

/** One file of a disc, paired with its path relative to the picked folder. */
export interface BdmvFile {
  /** e.g. `BDMV/PLAYLIST/00000.mpls` (a `File.webkitRelativePath`). */
  path: string;
  /** The browser `File` handle; its bytes are read lazily inside the Worker. */
  file: File;
}

/** Live demux progress: `done`/`total` bytes over the file being scanned. */
export interface ScanProgress {
  file: string;
  done: number;
  total: number;
}

/** A progress observer, called repeatedly as the scan demuxes. */
export type ProgressFn = (progress: ScanProgress) => void;

/**
 * One playlist of the disc — a row of the selection table {@link listPlaylists}
 * returns, mirroring the CLI's `#`/Group/Playlist File/Length/Estimated Bytes
 * columns. Pass the chosen rows' {@link PlaylistRow.name}s to {@link analyze} as
 * `options.selection` to measure just those playlists.
 */
export interface PlaylistRow {
  /** 1-based position in the table — the handle the user picks. */
  position: number;
  /** Shared-clip group number (1-based). */
  group: number;
  /** The playlist file name, e.g. `00000.MPLS`. */
  name: string;
  /** `hh:mm:ss` total length. */
  length: string;
  /** Estimated bytes (interleaved `*.ssif` size, else `*.m2ts` size), or `null`. */
  estimatedBytes: number | null;
  /** Whether the playlist hides any stream (the CLI's `(*)` note). */
  hasHidden: boolean;
  /**
   * The filter rules that classify this playlist as withheld: `"short"` (under
   * 20 s), `"looping"`, both, or none. {@link listPlaylists} drops such
   * playlists unless the matching option ({@link AnalyzeOptions.showShortPlaylists}
   * / {@link AnalyzeOptions.showLoopingPlaylists}) was passed, so a row with a
   * non-empty `hiddenBy` only appears when it was — but the rules it names are
   * the same either way, so you can list once with both options on and re-apply
   * either rule to the rows without re-scanning.
   */
  hiddenBy: ("short" | "looping")[];
}

/** Optional overrides for {@link analyze} and {@link listPlaylists}. */
export interface AnalyzeOptions {
  /**
   * A factory constructing the scan Worker to use. Defaults to
   * `new Worker(new URL("./worker.js", import.meta.url), { type: "module" })`,
   * which any bundler that follows the `new Worker(new URL(..., import.meta.url))`
   * convention (Vite, webpack 5, native ESM) rewrites to the emitted asset. Set
   * this when your toolchain can't follow that pattern and you host `worker.js`
   * (and the `.wasm` it loads) yourself — construct the module Worker from the
   * URL your bundler produced for `worker.js` and return it.
   */
  createWorker?: () => Worker;
  /**
   * The playlists to measure, by {@link PlaylistRow.name} — the browser
   * equivalent of the CLI's `--mpls`, measured unfiltered in the given order.
   * Omitted or empty measures the standard `--whole` set. Ignored by
   * {@link listPlaylists}.
   */
  selection?: string[];
  /**
   * List playlists shorter than 20 seconds too — the CLI's
   * `--show-short-playlists`. Used by {@link listPlaylists} /
   * {@link listPlaylistsIso}, which drop them by default; ignored by
   * {@link analyze} / {@link analyzeIso}, which measure `selection` unfiltered.
   */
  showShortPlaylists?: boolean;
  /**
   * List looping playlists too — the CLI's `--show-looping-playlists`. Used by
   * {@link listPlaylists} / {@link listPlaylistsIso}, which drop them by
   * default; ignored by {@link analyze} / {@link analyzeIso}, which measure
   * `selection` unfiltered.
   */
  showLoopingPlaylists?: boolean;
  /**
   * An optional {@link AbortSignal} that cancels an in-progress measured scan
   * ({@link analyze} / {@link analyzeIso}): when it aborts, the scan Worker is
   * terminated and the returned promise rejects with the signal's reason (an
   * `AbortError`). Ignored by {@link listPlaylists} — the structural scan is
   * fast enough not to need it.
   */
  signal?: AbortSignal;
}

type WorkerMessage =
  | ({ type: "progress" } & ScanProgress)
  | { type: "done"; report: string }
  | { type: "rows"; rows: PlaylistRow[] }
  | { type: "error"; message: string };

/** Spawns the scan Worker (a module worker by the bundler-aware convention). */
function spawnWorker(options?: AnalyzeOptions): Worker {
  // The default path MUST stay a bare `new Worker(new URL("./worker.js",
  // import.meta.url), …)` literal: that exact shape is what Vite and webpack 5
  // statically detect to compile the Worker into a chunk and emit the `.wasm` it
  // loads as an asset. Folding the override into that call would make the first
  // argument an expression rather than a `new URL(...)` node, which defeats that
  // detection (the bundler then ships a broken worker and no wasm), so the
  // override is a separate branch that keeps the default literal intact.
  if (options?.createWorker) {
    return options.createWorker();
  }
  return new Worker(new URL("./worker.js", import.meta.url), { type: "module" });
}

/** The `(relativePath, File)` lists the Worker takes, from `files`. */
function payload(files: BdmvFile[]): { paths: string[]; files: File[] } {
  return {
    paths: files.map((entry) => entry.path),
    files: files.map((entry) => entry.file),
  };
}

/** The two playlist-filter opt-outs a listing request carries, defaulted off. */
function listingOptions(options?: AnalyzeOptions): {
  showShortPlaylists: boolean;
  showLoopingPlaylists: boolean;
} {
  return {
    showShortPlaylists: options?.showShortPlaylists ?? false,
    showLoopingPlaylists: options?.showLoopingPlaylists ?? false,
  };
}

/**
 * The reject reason for a cancelled scan. Always an `AbortError` (the signal's
 * own reason when present, else a fresh one), so callers can tell a user cancel
 * from a real scan failure by its `name`.
 */
function cancelledError(signal?: AbortSignal): DOMException {
  const reason = signal?.reason;
  return reason instanceof DOMException && reason.name === "AbortError"
    ? reason
    : new DOMException("scan cancelled", "AbortError");
}

/**
 * Lists the disc's playlists via the fast structural scan, resolving with the
 * selection-table rows (see {@link PlaylistRow}). No stream files are demuxed,
 * so it returns quickly; show the rows as a checklist, then hand the chosen
 * names to {@link analyze}'s `options.selection`.
 *
 * Short and looping playlists are dropped like the CLI's `--list`; pass
 * `options.showShortPlaylists` / `options.showLoopingPlaylists` to list them
 * too, and read each row's {@link PlaylistRow.hiddenBy} to tell them apart.
 *
 * Everything runs locally: no bytes leave the page.
 */
export function listPlaylists(files: BdmvFile[], options?: AnalyzeOptions): Promise<PlaylistRow[]> {
  return new Promise<PlaylistRow[]>((resolve, reject) => {
    const worker = spawnWorker(options);

    worker.onmessage = (event: MessageEvent<WorkerMessage>) => {
      const message = event.data;
      if (message.type === "rows") {
        worker.terminate();
        resolve(message.rows);
      } else if (message.type === "error") {
        worker.terminate();
        reject(new Error(message.message));
      }
    };

    worker.onerror = (event: ErrorEvent) => {
      worker.terminate();
      reject(new Error(event.message || "scan worker failed"));
    };

    worker.postMessage({ kind: "list", ...payload(files), ...listingOptions(options) });
  });
}

/**
 * Lists a single Blu-ray `.iso`'s playlists via the fast structural scan,
 * resolving with the selection-table rows (see {@link PlaylistRow}) — the `.iso`
 * counterpart of {@link listPlaylists}. The image is opened through the UDF
 * reader; no stream data is demuxed, so it returns quickly. Hand the chosen
 * names to {@link analyzeIso}'s `options.selection`.
 *
 * Short and looping playlists are dropped like the CLI's `--list`; pass
 * `options.showShortPlaylists` / `options.showLoopingPlaylists` to list them
 * too, and read each row's {@link PlaylistRow.hiddenBy} to tell them apart.
 *
 * Everything runs locally: no bytes leave the page.
 */
export function listPlaylistsIso(file: File, options?: AnalyzeOptions): Promise<PlaylistRow[]> {
  return new Promise<PlaylistRow[]>((resolve, reject) => {
    const worker = spawnWorker(options);

    worker.onmessage = (event: MessageEvent<WorkerMessage>) => {
      const message = event.data;
      if (message.type === "rows") {
        worker.terminate();
        resolve(message.rows);
      } else if (message.type === "error") {
        worker.terminate();
        reject(new Error(message.message));
      }
    };

    worker.onerror = (event: ErrorEvent) => {
      worker.terminate();
      reject(new Error(event.message || "scan worker failed"));
    };

    worker.postMessage({ kind: "list-iso", file, ...listingOptions(options) });
  });
}

/**
 * Runs the full measured Blu-ray scan in a Worker and resolves with the classic
 * disc report. `onProgress`, if given, is called as the scan demuxes;
 * `options.selection` measures only the named playlists (see
 * {@link AnalyzeOptions}), defaulting to the standard `--whole` set.
 * `options.createWorker` overrides how the scan Worker is constructed.
 *
 * Everything runs locally: no bytes leave the page.
 */
export function analyze(
  files: BdmvFile[],
  onProgress?: ProgressFn,
  options?: AnalyzeOptions,
): Promise<string> {
  return new Promise<string>((resolve, reject) => {
    const signal = options?.signal;
    if (signal?.aborted) {
      reject(cancelledError(signal));
      return;
    }
    const worker = spawnWorker(options);

    // Cancel = terminate the Worker (its normal teardown path), just earlier.
    const onAbort = () => {
      worker.terminate();
      reject(cancelledError(signal));
    };
    const unlisten = () => signal?.removeEventListener("abort", onAbort);
    signal?.addEventListener("abort", onAbort, { once: true });

    worker.onmessage = (event: MessageEvent<WorkerMessage>) => {
      const message = event.data;
      switch (message.type) {
        case "progress":
          onProgress?.(message);
          break;
        case "done":
          unlisten();
          worker.terminate();
          resolve(message.report);
          break;
        case "error":
          unlisten();
          worker.terminate();
          reject(new Error(message.message));
          break;
        default:
          break;
      }
    };

    worker.onerror = (event: ErrorEvent) => {
      unlisten();
      worker.terminate();
      reject(new Error(event.message || "scan worker failed"));
    };

    worker.postMessage({ kind: "scan", ...payload(files), selection: options?.selection ?? [] });
  });
}

/**
 * Runs the full measured Blu-ray scan of a single `.iso` `File` in a Worker and
 * resolves with the classic disc report — the browser equivalent of
 * `bdinfo-rs <disc>.iso`. The image is opened through the read-only UDF reader
 * and streamed (its bytes are read on demand at byte offsets), never loaded
 * whole, so a multi-GB `.iso` is fine. `onProgress` and `options` behave exactly
 * as in {@link analyze}.
 *
 * Everything runs locally: no bytes leave the page.
 */
export function analyzeIso(
  file: File,
  onProgress?: ProgressFn,
  options?: AnalyzeOptions,
): Promise<string> {
  return new Promise<string>((resolve, reject) => {
    const signal = options?.signal;
    if (signal?.aborted) {
      reject(cancelledError(signal));
      return;
    }
    const worker = spawnWorker(options);

    // Cancel = terminate the Worker (its normal teardown path), just earlier.
    const onAbort = () => {
      worker.terminate();
      reject(cancelledError(signal));
    };
    const unlisten = () => signal?.removeEventListener("abort", onAbort);
    signal?.addEventListener("abort", onAbort, { once: true });

    worker.onmessage = (event: MessageEvent<WorkerMessage>) => {
      const message = event.data;
      switch (message.type) {
        case "progress":
          onProgress?.(message);
          break;
        case "done":
          unlisten();
          worker.terminate();
          resolve(message.report);
          break;
        case "error":
          unlisten();
          worker.terminate();
          reject(new Error(message.message));
          break;
        default:
          break;
      }
    };

    worker.onerror = (event: ErrorEvent) => {
      unlisten();
      worker.terminate();
      reject(new Error(event.message || "scan worker failed"));
    };

    worker.postMessage({ kind: "scan-iso", file, selection: options?.selection ?? [] });
  });
}
