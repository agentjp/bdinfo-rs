// The package's public API. Three calls run the bdinfo-rs scan entirely in the
// browser, off the main thread, over either a `webkitdirectory`-picked BDMV
// folder or a single Blu-ray `.iso` `File`:
//
//   - `inspect` — the fast STRUCTURAL scan (like `bdinfo-rs <disc> --list`): no
//     packet demux, so it reads the playlist and clip metadata rather than the
//     multi-GB stream files, and resolves with the whole disc model.
//   - `scan` — the FULL measured scan (like `bdinfo-rs <disc>`), resolving with
//     the rendered report AND the disc model, both from one demux.
//   - `renderReport` — re-renders the report from a disc model `scan` returned,
//     with different optional sections and without touching the media again.
//
// Each spawns the scan Worker (which hosts the WebAssembly module), hands it the
// files, and resolves with the result — the rendered classic disc report being
// the very bytes the native CLI writes to `BDINFO.<label>.txt`.
//
// `analyze`, `analyzeIso`, `listPlaylists` and `listPlaylistsIso` are the 2.0
// surface: each returns one slice of what the three calls above return. They
// keep working exactly as before and are deprecated for removal in 3.0.0.

import type { Disc, ScanResult } from "../pkg/bdinfo_rs_wasm.js";

// The structured disc model is generated from the Rust types, so these
// declarations and the values the WebAssembly module hands back have one source
// of truth. Re-exported from the package entry so a consumer that types a `Disc`
// does not have to reach into the `@bdinfo-rs/wasm/wasm` subpath for it.
export type {
  Chapter,
  Clip,
  ClipStream,
  Disc,
  Playlist,
  ScanError,
  ScanErrorReason,
  ScanResult,
  ScanStage,
  Stream,
} from "../pkg/bdinfo_rs_wasm.js";

/** One file of a disc, paired with its path relative to the picked folder. */
export interface BdmvFile {
  /** e.g. `BDMV/PLAYLIST/00000.mpls` (a `File.webkitRelativePath`). */
  path: string;
  /** The browser `File` handle; its bytes are read lazily inside the Worker. */
  file: File;
}

/**
 * What a call reads the disc from: the `(relativePath, File)` pairs of a
 * `webkitdirectory` folder pick, or a single Blu-ray `.iso` `File`. Every call
 * that takes one accepts both and tells them apart itself.
 */
export type DiscSource = BdmvFile[] | File;

/** Live demux progress: `done`/`total` bytes over the file being scanned. */
export interface ScanProgress {
  file: string;
  done: number;
  total: number;
}

/** A progress observer, called repeatedly as the scan demuxes. */
export type ProgressFn = (progress: ScanProgress) => void;

// Declared here rather than generated with the disc model: the WebAssembly
// module builds these rows as JSON text through a serializer of its own, so no
// TypeScript declaration is emitted for them — and `hiddenBy` is typed here as
// the closed set of rule names that serializer can emit, which a generated
// declaration would widen to `string[]`.
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

/**
 * Optional overrides for every call in this module. Each option documents which
 * calls read it; a call ignores the ones it does not name.
 */
export interface AnalyzeOptions {
  /**
   * A factory constructing the scan Worker to use. Defaults to
   * `new Worker(new URL("./worker.js", import.meta.url), { type: "module" })`,
   * which any bundler that follows the `new Worker(new URL(..., import.meta.url))`
   * convention (Vite, webpack 5, native ESM) rewrites to the emitted asset. Set
   * this when your toolchain can't follow that pattern and you host `worker.js`
   * (and the `.wasm` it loads) yourself — construct the module Worker from the
   * URL your bundler produced for `worker.js` and return it.
   *
   * Read by every call: each one runs in the scan Worker.
   */
  createWorker?: () => Worker;
  /**
   * The playlists to measure, by {@link PlaylistRow.name} — the browser
   * equivalent of the CLI's `--mpls`, measured unfiltered in the given order.
   * Omitted or empty measures the standard `--whole` set.
   *
   * Read by {@link scan}, {@link analyze} and {@link analyzeIso}.
   */
  selection?: string[];
  /**
   * Include playlists shorter than {@link AnalyzeOptions.shortPlaylistSeconds}
   * — the CLI's `--show-short-playlists`.
   *
   * Read by {@link inspect}, {@link listPlaylists} and
   * {@link listPlaylistsIso}, which withhold them by default. The measured
   * scans measure their `selection` unfiltered, so it changes nothing there.
   */
  showShortPlaylists?: boolean;
  /**
   * Include looping playlists — the CLI's `--show-looping-playlists`. Read by
   * the same calls as {@link AnalyzeOptions.showShortPlaylists}, and with the
   * same effect on the others: none.
   */
  showLoopingPlaylists?: boolean;
  /**
   * The length in seconds under which a playlist counts as short, defaulting to
   * 20. Zero or less means that default.
   *
   * Read by {@link inspect} only — {@link listPlaylists} and
   * {@link listPlaylistsIso} always judge against the 20 s default.
   */
  shortPlaylistSeconds?: number;
  /**
   * Render the report's `STREAM DIAGNOSTICS:` section, defaulting to `true` —
   * the section the CLI's report carries.
   *
   * Read by {@link scan} and {@link renderReport}. {@link analyze} and
   * {@link analyzeIso} always render every section.
   */
  streamDiagnostics?: boolean;
  /**
   * Render the report's `QUICK SUMMARY:` section, defaulting to `true`. Read by
   * the same calls as {@link AnalyzeOptions.streamDiagnostics}.
   */
  quickSummary?: boolean;
  /**
   * An optional {@link AbortSignal} that cancels the call in progress: when it
   * aborts, the scan Worker is terminated and the returned promise rejects with
   * the signal's reason (an `AbortError`), so callers can tell a user cancel
   * from a real failure by the rejection's `name`.
   *
   * Read by {@link inspect}, {@link scan}, {@link renderReport},
   * {@link analyze} and {@link analyzeIso}. Ignored by {@link listPlaylists} /
   * {@link listPlaylistsIso} — the structural scan is fast enough not to need
   * it.
   */
  signal?: AbortSignal;
}

/** Everything the scan Worker posts back; see `worker.ts` for who sends what. */
type WorkerMessage =
  | ({ type: "progress" } & ScanProgress)
  | { type: "done"; report: string }
  | { type: "rows"; rows: PlaylistRow[] }
  | { type: "disc"; disc: Disc }
  | { type: "result"; result: ScanResult }
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
 * The playlist-filter options an {@link inspect} request carries: a listing's
 * two opt-outs plus the short-playlist threshold only this call reads. Zero is
 * how the WebAssembly module spells "use the 20 s default".
 */
function inspectOptions(options?: AnalyzeOptions): {
  showShortPlaylists: boolean;
  showLoopingPlaylists: boolean;
  shortPlaylistSeconds: number;
} {
  return {
    ...listingOptions(options),
    shortPlaylistSeconds: options?.shortPlaylistSeconds ?? 0,
  };
}

/**
 * The two optional report sections a render request carries. Both default to
 * `true` — an omitted option renders its section, so a render with no options
 * reproduces the report the CLI writes.
 */
function reportOptions(options?: AnalyzeOptions): {
  streamDiagnostics: boolean;
  quickSummary: boolean;
} {
  return {
    streamDiagnostics: options?.streamDiagnostics ?? true,
    quickSummary: options?.quickSummary ?? true,
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
 * Runs one scan-Worker request end to end and resolves with what `take` pulls
 * out of the reply that answers it.
 *
 * `request` is the shape every call in this module shares: spawn the Worker,
 * post one request, forward demux progress to `onProgress`, settle on the first
 * reply `take` accepts or on the first `error`, and terminate the Worker on
 * every exit — reply, failure, or cancel. `take` returns `null` for a message
 * that is not this request's answer, which is what keeps the message loop out of
 * the calls themselves.
 *
 * `message` must be one of the request shapes `worker.ts` declares as `Request`;
 * that union is the contract, and it cannot be imported here because `worker.ts`
 * type-checks against the WebWorker library rather than the DOM.
 */
function request<T>(
  message: unknown,
  take: (reply: WorkerMessage) => { value: T } | null,
  options?: AnalyzeOptions,
  onProgress?: ProgressFn,
): Promise<T> {
  return new Promise<T>((resolve, reject) => {
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
    signal?.addEventListener("abort", onAbort, { once: true });
    const close = () => {
      signal?.removeEventListener("abort", onAbort);
      worker.terminate();
    };

    worker.onmessage = (event: MessageEvent<WorkerMessage>) => {
      const reply = event.data;
      if (reply.type === "progress") {
        onProgress?.(reply);
        return;
      }
      if (reply.type === "error") {
        close();
        reject(new Error(reply.message));
        return;
      }
      const answer = take(reply);
      if (answer !== null) {
        close();
        resolve(answer.value);
      }
    };

    worker.onerror = (event: ErrorEvent) => {
      close();
      reject(new Error(event.message || "scan worker failed"));
    };

    worker.postMessage(message);
  });
}

/**
 * Runs the fast structural scan and resolves with the whole disc model (see
 * {@link Disc}) — a `webkitdirectory` folder pick or a single Blu-ray `.iso`
 * `File` in, everything the disc's metadata knows out.
 *
 * No stream file is demuxed, so it returns quickly and `disc.measured` is
 * `false`: every measured value — bitrates, packet counts, chapter rates — is
 * zero because nothing measured it rather than because it is genuinely zero.
 * Show the playlists as a checklist, then hand the chosen
 * {@link Playlist.name}s to {@link scan}'s `options.selection`.
 *
 * `disc.playlists` holds the playlists the selection filter keeps, which
 * `options.showShortPlaylists`, `options.showLoopingPlaylists` and
 * `options.shortPlaylistSeconds` widen; the disc-level properties and
 * `disc.errors` always describe the whole disc.
 *
 * Everything runs locally: no bytes leave the page.
 */
export function inspect(source: DiscSource, options?: AnalyzeOptions): Promise<Disc> {
  const message = Array.isArray(source)
    ? { kind: "inspect", ...payload(source), ...inspectOptions(options) }
    : { kind: "inspect-iso", file: source, ...inspectOptions(options) };
  return request(
    message,
    (reply) => (reply.type === "disc" ? { value: reply.disc } : null),
    options,
  );
}

/**
 * Runs the full measured Blu-ray scan in a Worker and resolves with both
 * outputs of that one demux: the classic disc report and the same scan as a
 * {@link Disc} (see {@link ScanResult}). Takes a `webkitdirectory` folder pick
 * or a single Blu-ray `.iso` `File`; an `.iso` is opened through the read-only
 * UDF reader and read on demand at byte offsets, never loaded whole, so a
 * multi-GB image is fine.
 *
 * `onProgress`, if given, is called as the scan demuxes. `options.selection`
 * measures only the named playlists (the CLI's `--mpls`), defaulting to the
 * standard `--whole` set. `options.streamDiagnostics` and
 * `options.quickSummary` choose the report's optional sections, both rendered
 * unless switched off. `options.signal` cancels the scan.
 *
 * `result.disc.measured` is `true`, so a zero in it is a genuine zero. Keep
 * `result.disc` and {@link renderReport} re-renders the report from it with
 * other sections, without reading the disc again.
 *
 * Everything runs locally: no bytes leave the page.
 */
export function scan(
  source: DiscSource,
  onProgress?: ProgressFn,
  options?: AnalyzeOptions,
): Promise<ScanResult> {
  const selection = options?.selection ?? [];
  const message = Array.isArray(source)
    ? { kind: "scan-full", ...payload(source), selection, ...reportOptions(options) }
    : { kind: "scan-iso-full", file: source, selection, ...reportOptions(options) };
  return request(
    message,
    (reply) => (reply.type === "result" ? { value: reply.result } : null),
    options,
    onProgress,
  );
}

/**
 * Re-renders the classic disc report from a {@link Disc} — no media, no
 * rescan — with whichever optional sections `options.streamDiagnostics` and
 * `options.quickSummary` ask for, both rendered unless switched off.
 *
 * The disc model carries every value the report prints, so this is a render
 * rather than an approximation: `renderReport(result.disc)` on the `disc` from
 * a {@link scan} reproduces that scan's report byte for byte. Store the `Disc`
 * rather than the report text and every rendering of it stays one call away.
 * It is also the only supported way to turn a `Disc` into report text —
 * formatting the model by hand produces something that merely resembles the
 * locked format.
 *
 * The playlists print in the disc's presentation order: the standard `--whole`
 * set, grouped by shared clip files and longest first. A `disc` from a
 * {@link scan} with a `selection` holds every playlist but measured values only
 * for the ones that scan named, so re-rendering it prints the rest at zero —
 * scan again to measure them.
 */
export function renderReport(disc: Disc, options?: AnalyzeOptions): Promise<string> {
  return request(
    { kind: "render", disc, ...reportOptions(options) },
    (reply) => (reply.type === "done" ? { value: reply.report } : null),
    options,
  );
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
 *
 * @deprecated since 2.1.0, removed in 3.0.0. Use {@link inspect}, which runs
 * the same structural scan and resolves with the whole {@link Disc} instead of
 * seven table columns. `position`, `group` and `hiddenBy` describe the table
 * rather than the disc and are not on the model, so a UI that renders a
 * selection table derives them from `disc.playlists` itself.
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
 *
 * @deprecated since 2.1.0, removed in 3.0.0. Use {@link inspect}, which takes
 * the `.iso` `File` directly and resolves with the whole {@link Disc}; see the
 * note on {@link listPlaylists} for the three row columns the model leaves to
 * the caller.
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
 *
 * @deprecated since 2.1.0, removed in 3.0.0. Use {@link scan}, which runs the
 * same measured scan and resolves with the same report plus the {@link Disc} it
 * was rendered from, takes the folder or the `.iso` through one parameter, and
 * accepts the report-section options.
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
 *
 * @deprecated since 2.1.0, removed in 3.0.0. Use {@link scan}, which takes the
 * `.iso` `File` directly and resolves with the report plus the {@link Disc} it
 * was rendered from.
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
