// The browser globals the Node harnesses run the built wasm against, plus an
// injectable read-fault policy.
//
// `FileReaderSync` exists only on a Worker thread, so Node has neither it nor
// `File`. These three classes are the smallest stand-ins the streaming seam
// touches: a byte window with `size`/`slice`, a `File` over one (what the wasm
// `instanceof File` check sees), and the synchronous read itself. Both the
// folder path (`scan_files`) and the `.iso` path (`scan_iso`) read every byte
// through exactly one call pair — `File.slice(start, end)` then
// `FileReaderSync.readAsArrayBuffer(blob)` — so a policy consulted inside
// `readAsArrayBuffer` can fail a chosen byte range of a chosen file and nothing
// else.
//
// A thrown read is what a browser surfaces for a damaged optical volume, and it
// is the only failure the seam can see: a short file reads back short and ends
// in a clean EOF, which is a different defect entirely.

/** The installed fault policy: lower-cased file name → first failing offset. */
let faultOffsets = new Map();
/** Whether one thrown read makes every later read of every file throw. */
let poisonVolume = false;
/** Whether a read has already thrown under `poisonVolume`. */
let poisoned = false;
/** Every `readAsArrayBuffer` call since the last `setFaults`, in order. */
let reads = [];

/**
 * Installs a read-fault policy and clears the read log.
 *
 * `files` maps a file name (matched case-insensitively against `File.name`) to
 * the first byte offset that cannot be read: a read throws once its window
 * reaches that offset, and a read that stops before it succeeds. `poisonVolume`
 * models a whole failing volume — once any read has thrown, every later read of
 * every file throws too.
 *
 * @param {{files?: Record<string, number>, poisonVolume?: boolean}} policy
 */
export function setFaults(policy = {}) {
  faultOffsets = new Map(
    Object.entries(policy.files ?? {}).map(([name, at]) => [name.toLowerCase(), at]),
  );
  poisonVolume = policy.poisonVolume === true;
  poisoned = false;
  reads = [];
}

/** Removes every fault, restoring the healthy reader. */
export function clearFaults() {
  setFaults({});
}

/**
 * The reads since the last `setFaults`, oldest first, each
 * `{name, start, end, failed}` with `[start, end)` the requested byte window.
 */
export function readLog() {
  return reads;
}

/**
 * The value a failing read throws.
 *
 * Deliberately not an `Error` and deliberately without a `message`: the wasm
 * side reads `.message` off the thrown value and falls back to the literal
 * string `JavaScript exception` when there is none, which is what a browser
 * scan of a damaged disc has been observed to report. Throwing an `Error` here
 * would pin the fallback out of the harness.
 *
 * @param {string} why
 * @param {string} name
 */
function faultValue(why, name) {
  return { shimFault: why, file: name };
}

/**
 * The fault a read of `name` ending at byte `end` hits, or `null` for none.
 *
 * Where the read starts does not matter: a window that reaches the failing
 * offset fails, and one that stops short of it succeeds.
 */
function readFault(name, end) {
  if (poisoned) {
    return faultValue("volume poisoned by an earlier failure", name);
  }
  const at = faultOffsets.get(name.toLowerCase());
  if (at === undefined || end <= at) {
    return null;
  }
  poisoned = poisonVolume;
  return faultValue(`unreadable from byte ${at}`, name);
}

/**
 * A minimal synchronous `Blob`: a byte window with `size` and `slice`.
 *
 * It also carries where the window came from — the originating file name and
 * its absolute start offset — because `readAsArrayBuffer` receives the slice,
 * not the file, and the fault policy is keyed on both.
 */
export class ShimBlob {
  constructor(bytes, name = "", start = 0) {
    this._bytes = bytes;
    this._name = name;
    this._start = start;
  }
  get size() {
    return this._bytes.length;
  }
  slice(start, end) {
    return new ShimBlob(this._bytes.subarray(start, end), this._name, this._start + start);
  }
}

/** A `File` over a byte buffer — what the wasm `instanceof File` check sees. */
export class ShimFile extends ShimBlob {
  constructor(bytes, name) {
    super(bytes, name);
    this.name = name;
  }
}

/** `FileReaderSync.readAsArrayBuffer` — the synchronous byte read the seam needs. */
export class ShimFileReaderSync {
  readAsArrayBuffer(blob) {
    const b = blob._bytes;
    const start = blob._start;
    const end = start + b.byteLength;
    const fault = readFault(blob._name, end);
    reads.push({ name: blob._name, start, end, failed: fault !== null });
    if (fault !== null) {
      throw fault;
    }
    return b.buffer.slice(b.byteOffset, b.byteOffset + b.byteLength);
  }
}

/** Publishes the shims as the globals the wasm streaming seam looks for. */
export function installShims() {
  globalThis.File = ShimFile;
  globalThis.FileReaderSync = ShimFileReaderSync;
}
