// The package's public API. Three calls run the bdinfo-rs scan entirely in the
// browser, off the main thread, over either a `webkitdirectory`-picked BDMV
// folder or a single Blu-ray `.iso` `File`:
//
//   - `inspect` — the fast STRUCTURAL scan (like `bdinfo-rs <disc> --list`): no
//     whole-file demux, so it reads the playlist and clip metadata rather than
//     measuring the multi-GB stream files, and resolves with the whole disc
//     model. `options.codecs` deepens it to the bounded codec pass.
//   - `scan` — the FULL measured scan (like `bdinfo-rs <disc>`), resolving with
//     the rendered report AND the disc model, both from one demux.
//   - `renderReport` — re-renders the report from a disc model `scan` returned,
//     with different optional sections and without touching the media again.
//
// `reportFileName` rounds the flow off: the sanitized `BDINFO.<label>.txt` name
// a report is saved under, from the same rule the native CLI applies.
//
// Each spawns the scan Worker (which hosts the WebAssembly module), hands it the
// files, and resolves with the result — the rendered classic disc report being
// the very bytes the native CLI writes to `BDINFO.<label>.txt`.

import type {
  Disc,
  MeasuredSnapshot,
  ScanOptions as ModuleOptions,
  ScanResult,
} from "../pkg/bdinfo_rs_wasm.js";

// The structured disc model is generated from the Rust types, so these
// declarations and the values the WebAssembly module hands back have one source
// of truth. Re-exported from the package entry so a consumer that types a `Disc`
// does not have to reach into the `@bdinfo-rs/wasm/wasm` subpath for it.
export type {
  Chapter,
  Clip,
  ClipStream,
  Disc,
  HiddenRule,
  MeasuredClip,
  MeasuredPlaylist,
  MeasuredSnapshot,
  MeasuredStream,
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

/**
 * A live-tally observer, called as a measured scan raises its numbers — at most
 * once a second, whatever the read speed.
 */
export type MeasuredFn = (measured: MeasuredSnapshot) => void;

/**
 * Optional overrides for every call in this module. Each option documents which
 * calls read it; a call ignores the ones it does not name.
 */
export interface ScanOptions {
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
   * The playlists to measure, by {@link Playlist.name} — the browser
   * equivalent of the CLI's `--mpls`, measured unfiltered in the given order.
   * Omitted or empty measures the standard `--whole` set.
   *
   * Read by {@link scan}.
   */
  selection?: string[];
  /**
   * The length in seconds under which a playlist counts as short, defaulting to
   * 20 when omitted. Must be finite and within 0..=86400 (one day): zero
   * switches the short rule off, since no playlist is shorter than zero
   * seconds, and anything outside that domain — negative, non-finite, past the
   * ceiling — rejects the call rather than silently scanning with the default.
   *
   * Read by {@link inspect} and {@link scan}, both of which classify every
   * playlist against it and report the outcome in {@link Playlist.hiddenBy}.
   * It changes no other value: which playlists a `Disc` holds, which ones a
   * `selection` measures, and the rendered report are all the same either way.
   */
  shortPlaylistSeconds?: number;
  /**
   * Read each stream file's head for codec detail during an {@link inspect},
   * defaulting to `false`.
   *
   * Set it and the inspect reads just far enough into each stream file to
   * parse the first parameter sets, so every {@link Stream} carries its full
   * codec description — profile, level, HDR metadata — without the whole-file
   * demux a {@link scan} costs. `Disc.measured` stays `false` and bitrates,
   * packet counts and chapter rates are still zero.
   *
   * Read by {@link inspect} alone — a {@link scan} demuxes everything and
   * carries full codec detail already.
   */
  codecs?: boolean;
  /**
   * Render the report's `STREAM DIAGNOSTICS:` section, defaulting to `true` —
   * the section the CLI's report carries.
   *
   * Read by {@link scan} and {@link renderReport}.
   */
  streamDiagnostics?: boolean;
  /**
   * Render the report's `QUICK SUMMARY:` section, defaulting to `true`. Read by
   * the same calls as {@link ScanOptions.streamDiagnostics}.
   */
  quickSummary?: boolean;
  /**
   * Keep what the scan measured of a stream file whose read failed partway,
   * defaulting to `true`.
   *
   * A kept partial file carries everything the scan accumulated up to the
   * failing read, so the report's chapter rows and stream diagnostics — and the
   * matching {@link Disc} values — cover the span before the failure and stay
   * zero after it. Set it to `false` and that span is discarded, leaving those
   * cells zero throughout. Either way the failure itself is reported in
   * {@link Disc.errors}.
   *
   * Read by {@link scan}.
   */
  keepPartial?: boolean;
  /**
   * An observer of the scan measured tallies as they build up, so a table can
   * tick its measured cells during the scan instead of waiting for the report.
   *
   * It is called with one {@link MeasuredSnapshot} at most once a second — the
   * scan produces them far faster on a quick source, and the extra ones are
   * dropped rather than posted. Each snapshot covers only the playlists that
   * play the stream file it was taken over, so keep your last known numbers for
   * the rest; within one scan the byte tallies only grow. The values land
   * exactly on the ones {@link scan} resolves with, so a cell that ticks here
   * does not jump when the scan finishes.
   *
   * Read by {@link scan}. Omitting it costs nothing: a scan nobody watches
   * builds no snapshots at all.
   */
  onMeasured?: MeasuredFn;
  /**
   * An optional {@link AbortSignal} that cancels the call in progress: when it
   * aborts, the scan Worker is terminated and the returned promise rejects with
   * the signal's reason (an `AbortError`), so callers can tell a user cancel
   * from a real failure by the rejection's `name`.
   *
   * Read by every call in this module.
   */
  signal?: AbortSignal;
}

/** Everything the scan Worker posts back; see `worker.ts` for who sends what. */
type WorkerMessage =
  | ({ type: "progress" } & ScanProgress)
  | { type: "measured"; measured: MeasuredSnapshot }
  | { type: "done"; report: string }
  | { type: "disc"; disc: Disc }
  | { type: "result"; result: ScanResult }
  | { type: "file-name"; name: string }
  | { type: "error"; message: string };

/** Spawns the scan Worker (a module worker by the bundler-aware convention). */
function spawnWorker(options?: ScanOptions): Worker {
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

/**
 * The options object a request carries into the WebAssembly module — the
 * options the module itself takes. The rest of {@link ScanOptions} stops here:
 * the Worker factory and the abort signal drive this module and cannot be
 * posted to a Worker at all, and `selection` travels as its own request field.
 *
 * Each option is forwarded as given, `undefined` included — that is how the
 * module spells "left out", and the module is where the defaults live.
 */
function moduleOptions(options?: ScanOptions): ModuleOptions {
  return {
    streamDiagnostics: options?.streamDiagnostics,
    quickSummary: options?.quickSummary,
    shortPlaylistSeconds: options?.shortPlaylistSeconds,
    keepPartial: options?.keepPartial,
    codecs: options?.codecs,
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
 * post one request, forward demux progress to `onProgress` and live tallies to
 * `options.onMeasured`, settle on the first reply `take` accepts or on the first
 * `error`, and terminate the Worker on every exit — reply, failure, or cancel.
 * `take` returns `null` for a message that is not this request's answer, which
 * is what keeps the message loop out of the calls themselves.
 *
 * `message` must be one of the request shapes `worker.ts` declares as `Request`;
 * that union is the contract, and it cannot be imported here because `worker.ts`
 * type-checks against the WebWorker library rather than the DOM.
 */
function request<T>(
  message: unknown,
  take: (reply: WorkerMessage) => { value: T } | null,
  options?: ScanOptions,
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
      if (reply.type === "measured") {
        options?.onMeasured?.(reply.measured);
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
 * No stream file is measured, so it returns quickly and `disc.measured` is
 * `false`: every measured value — bitrates, packet counts, chapter rates — is
 * zero because nothing measured it rather than because it is genuinely zero.
 * `options.codecs` deepens the scan to the bounded codec pass, which reads
 * each stream file's head so the streams carry their full codec description
 * (profile, level, HDR) while everything else stays as cheap as before. Show
 * the playlists as a checklist, then hand the chosen {@link Playlist.name}s to
 * {@link scan}'s `options.selection`.
 *
 * `disc.playlists` holds every playlist on the disc, each carrying its
 * {@link Playlist.group}, {@link Playlist.position} and
 * {@link Playlist.hiddenBy}, so the classic selection table is
 * `disc.playlists.filter((p) => p.hiddenBy.length === 0)` sorted by `position`
 * — a client-side filter, with no rescan to widen or narrow it.
 *
 * Everything runs locally: no bytes leave the page.
 */
export function inspect(source: DiscSource, options?: ScanOptions): Promise<Disc> {
  const forwarded = moduleOptions(options);
  const message = Array.isArray(source)
    ? { kind: "inspect", ...payload(source), options: forwarded }
    : { kind: "inspect-iso", file: source, options: forwarded };
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
 * `onProgress`, if given, is called as the scan demuxes, and
 * `options.onMeasured` as its measured numbers climb — the two together are
 * what a page needs to show a live scan. `options.selection`
 * measures only the named playlists (the CLI's `--mpls`), defaulting to the
 * standard `--whole` set. `options.streamDiagnostics` and
 * `options.quickSummary` choose the report's optional sections, both rendered
 * unless switched off. `options.keepPartial` decides what becomes of a stream
 * file whose read fails partway: what was measured of it is kept unless
 * switched off. `options.signal` cancels the scan.
 *
 * `result.disc.measured` is `true`, so a zero in it is a genuine zero. Keep
 * `result.disc` and {@link renderReport} re-renders the report from it with
 * other sections, without reading the disc again.
 *
 * {@link Disc.shortStreamNotices} names any stream file measured shorter than
 * the disc declares. Such a file reads to a clean end of file — no entry in
 * `disc.errors`, no report `WARNING:` line — so the field is the scan's only
 * trace of the loss; show it beside the report.
 *
 * Everything runs locally: no bytes leave the page.
 */
export function scan(
  source: DiscSource,
  onProgress?: ProgressFn,
  options?: ScanOptions,
): Promise<ScanResult> {
  const shared = { selection: options?.selection ?? [], options: moduleOptions(options) };
  const message = Array.isArray(source)
    ? { kind: "scan", ...payload(source), ...shared }
    : { kind: "scan-iso", file: source, ...shared };
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
 * The playlists print in the order the scan that produced the `disc` printed
 * them ({@link Disc.reportOrder}), so a `disc` from a {@link scan} with a
 * `selection` re-renders as that scan reported it — a playlist it never
 * measured cannot reappear as a block of zeros. The model still holds every
 * playlist on the disc; scan again to measure more of them. A `disc` from
 * {@link inspect} was never rendered from and prints in the disc's presentation
 * order instead — the standard `--whole` set, grouped by shared clip files and
 * longest first — with every measured value zero.
 */
export function renderReport(disc: Disc, options?: ScanOptions): Promise<string> {
  return request(
    { kind: "render", disc, options: moduleOptions(options) },
    (reply) => (reply.type === "done" ? { value: reply.report } : null),
    options,
  );
}

/**
 * The file name a report for a disc labelled `label` is saved under —
 * `BDINFO.<label>.txt` with every character illegal in a file name replaced,
 * exactly the name the native CLI and the desktop app write.
 *
 * The sanitizer is the core library's (property-tested there): whatever bytes
 * a disc puts in its volume label, the result is one flat path component, so a
 * hostile label can neither escape a chosen directory nor break the save. Pass
 * {@link Disc.volumeLabel} and hand the result to a download attribute.
 *
 * Of the options, only `createWorker` and `signal` apply — the name is
 * computed by the WebAssembly module, so the call runs in the scan Worker like
 * every other.
 */
export function reportFileName(label: string, options?: ScanOptions): Promise<string> {
  return request(
    { kind: "file-name", label, options: moduleOptions(options) },
    (reply) => (reply.type === "file-name" ? { value: reply.name } : null),
    options,
  );
}
