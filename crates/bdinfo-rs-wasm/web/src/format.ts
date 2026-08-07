// The demo's byte-size cell formatter, in a module of its own so a test can
// reach it: `demo.ts` reads the page's DOM at import time and cannot be loaded
// outside a browser. Nothing in the published package imports this — the demo
// is the site, not the API.
//
// The desktop app formats the same cell in Rust (`model::byte_cell`), which no
// amount of arranging can share with TypeScript. What holds the two together is
// the vector table `crates/bdinfo-rs-gui/tests/size-vectors.tsv`: both sides
// assert every row of it, so a change here that is not mirrored there fails a
// test on both.

/**
 * A size cell under the size-format setting: `83.62 GB` / `335.37 MB`
 * (1024-based, like BDInfo) when `humanReadable`, the thousands-grouped exact
 * byte count (`11,145,216`) when not, and `—` for a size nothing knows yet.
 */
export function sizeCell(bytes: number | null, humanReadable: boolean): string {
  if (bytes === null || bytes <= 0) {
    return "—";
  }
  if (!humanReadable) {
    return bytes.toLocaleString("en-US");
  }
  const units = ["B", "KB", "MB", "GB", "TB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(unit === 0 ? 0 : 2)} ${units[unit]}`;
}
