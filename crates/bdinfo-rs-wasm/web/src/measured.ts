// The rate gate on the live measured tallies, between the WebAssembly module
// and the page.
//
// The module hands its measured observer one snapshot per demuxed read chunk —
// a BYTE cadence (5 MiB on this target), deliberately untimed, because only the
// consumer knows how often it can draw. Relaying every one of them would post a
// whole disc worth of playlist, clip and stream numbers across the Worker
// boundary at whatever rate the media happens to read at, which on a fast
// source is far more often than a page can use. This gate turns that byte
// cadence into a wall-clock one, at the rate classic BDInfo samples its own
// live grids.
//
// It lives in its own module so both the Worker relay and the Node harness can
// use it: the harness drives it with a mock clock, which is the only way to
// assert the rate without waiting in real time.
import type { MeasuredSnapshot } from "../pkg/bdinfo_rs_wasm.js";

/**
 * How often a live snapshot is relayed at most — 1 Hz, the rate classic BDInfo
 * samples its live grids at, and the same interval the desktop app ticks its
 * grids on.
 */
export const MEASURE_INTERVAL_MS = 1000;

/**
 * Wraps `relay` so it runs at most once per `intervalMs`, dropping the
 * snapshots that arrive in between.
 *
 * The first snapshot always passes, so a display starts ticking as soon as the
 * scan has measured anything. Dropping the rest loses nothing: the byte tallies
 * only grow, so a dropped snapshot is a number a later one carries anyway, and
 * the scan ends by handing back the finished disc, which replaces every cell —
 * including whatever the last dropped snapshot would have raised.
 *
 * `now` is the clock, injectable so a test can run the gate without waiting.
 */
export function throttleMeasured(
  relay: (snapshot: MeasuredSnapshot) => void,
  intervalMs: number = MEASURE_INTERVAL_MS,
  now: () => number = Date.now,
): (snapshot: MeasuredSnapshot) => void {
  let relayed: number | null = null;
  return (snapshot) => {
    const at = now();
    if (relayed !== null && at - relayed < intervalMs) {
      return;
    }
    relayed = at;
    relay(snapshot);
  };
}
