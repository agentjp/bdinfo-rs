// The package's public API: `analyze` runs the bdinfo-rs measured scan over a
// picked BDMV folder entirely in the browser, off the main thread.
//
// It spawns the scan Worker (which hosts the WebAssembly module), hands it the
// `(relativePath, File)` pairs, relays progress, and resolves with the rendered
// classic disc report — the very bytes the native CLI writes to `BDINFO.<label>.txt`.

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

/** Optional overrides for {@link analyze}. */
export interface AnalyzeOptions {
  /**
   * The URL of the scan Worker module to spawn. Defaults to
   * `new URL("./worker.js", import.meta.url)`, which any bundler that follows
   * the `new Worker(new URL(..., import.meta.url))` convention (Vite, webpack 5,
   * native ESM) rewrites to the emitted asset. Set this when your toolchain
   * can't follow that pattern and you host `worker.js` (and the `.wasm` it
   * loads) yourself — pass the URL your bundler produced for `worker.js`.
   */
  workerUrl?: string | URL;
}

type WorkerMessage =
  | ({ type: "progress" } & ScanProgress)
  | { type: "done"; report: string }
  | { type: "error"; message: string };

/**
 * Runs the full measured Blu-ray scan over `files` in a Worker and resolves with
 * the classic disc report. `onProgress`, if given, is called as the scan demuxes.
 * `options.workerUrl` overrides where the scan Worker is loaded from (see
 * {@link AnalyzeOptions}); the default suits any bundler-aware setup.
 *
 * Everything runs locally: no bytes leave the page.
 */
export function analyze(
  files: BdmvFile[],
  onProgress?: ProgressFn,
  options?: AnalyzeOptions,
): Promise<string> {
  return new Promise<string>((resolve, reject) => {
    const worker = new Worker(options?.workerUrl ?? new URL("./worker.js", import.meta.url), {
      type: "module",
    });

    worker.onmessage = (event: MessageEvent<WorkerMessage>) => {
      const message = event.data;
      switch (message.type) {
        case "progress":
          onProgress?.(message);
          break;
        case "done":
          worker.terminate();
          resolve(message.report);
          break;
        case "error":
          worker.terminate();
          reject(new Error(message.message));
          break;
      }
    };

    worker.onerror = (event: ErrorEvent) => {
      worker.terminate();
      reject(new Error(event.message || "scan worker failed"));
    };

    worker.postMessage({
      paths: files.map((entry) => entry.path),
      files: files.map((entry) => entry.file),
    });
  });
}
