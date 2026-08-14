// Node live-measured harness — the tallies a scan reports WHILE it runs, and
// the gate that rate-limits them.
//
// The golden harness next door proves what a finished scan renders; this one
// drives the same production export (`scan_files`, with the measured callback
// its sixth argument installs) over the synthetic `MultiPlaylist` disc and pins
// what arrives mid-scan:
//
//   - snapshots arrive per demuxed read chunk, not only at the end;
//   - each covers exactly the playlists that sequence the file it was taken over;
//   - the byte tallies only grow, and the last ones land on the numbers the finished
//     disc model carries — which is what lets a page tick cells and then take the summaries
//     without the cells jumping;
//   - a scan given no callback renders the identical report, so the observer is additive.
//
// It then drives `throttleMeasured` (dist/measured.js — what the Worker relays
// through) on a MOCK CLOCK: the gate is what keeps the wide payload off the
// Worker boundary at read speed, and a mock clock is the only way to assert its
// rate without waiting in real time.
//
// The disc is the same three playlists over three clips faults.node.mjs
// describes; 00011.M2TS is shared by 00000 and 00002, and is the one clip
// bigger than a read chunk, so it alone yields two chunk snapshots.
//
// Prereq: `npm run build` (emits pkg/ and dist/). Run with `npm run test:node`.

import { readdir, readFile } from "node:fs/promises";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { installShims, ShimFile } from "./shims.mjs";

installShims();

const here = dirname(fileURLToPath(import.meta.url));
const discRoot = resolve(here, "../../../bdinfo-rs/tests/fixtures/MultiPlaylist");
const wasmPath = resolve(here, "../pkg/bdinfo_rs_wasm_bg.wasm");

/** Bytes per transport packet — what turns a packet count into a byte tally. */
const PACKET_BYTES = 192;

/** Which playlists each stream file belongs to, by the disc's own topology. */
const COVERAGE = {
  "00011.M2TS": ["00000.MPLS", "00002.MPLS"],
  "00022.M2TS": ["00001.MPLS"],
  "00033.M2TS": ["00002.MPLS"],
};

/** Every file under `dir`, as absolute paths. */
async function walk(dir) {
  const found = [];
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name);
    found.push(...(entry.isDirectory() ? await walk(full) : [full]));
  }
  return found;
}

/** The measured bytes the finished model carries for `playlist`. */
function modelBytes(playlist) {
  return playlist.clips.reduce((bytes, clip) => bytes + clip.packetCount * PACKET_BYTES, 0);
}

async function main() {
  const { initSync, scan_files } = await import("../pkg/bdinfo_rs_wasm.js");
  initSync({ module: await readFile(wasmPath) });
  const { MEASURE_INTERVAL_MS, throttleMeasured } = await import("../dist/measured.js");

  // The disc as a folder pick, exactly as the demo hands one over.
  const paths = [];
  const files = [];
  for (const full of await walk(discRoot)) {
    const rel = relative(discRoot, full).split("\\").join("/");
    paths.push(`MultiPlaylist/${rel}`);
    files.push(
      new ShimFile(new Uint8Array(await readFile(full)), rel.slice(rel.lastIndexOf("/") + 1)),
    );
  }

  const failures = [];
  const check = (ok, message) => {
    if (!ok) {
      failures.push(message);
    }
  };

  // ── the observed scan ──────────────────────────────────────────────────────

  const snapshots = [];
  const watched = scan_files(paths, files, [], undefined, undefined, (snapshot) => {
    snapshots.push(snapshot);
  });

  // The measurement pass demuxes four chunks over three clips and closes each
  // file with one more snapshot, so a scan that only reported at the end could
  // not produce this many.
  check(snapshots.length >= 5, `expected several snapshots, got ${snapshots.length}`);

  // Each snapshot names the file it was taken over and covers exactly the
  // playlists that sequence it — the property a display leans on when it keeps
  // its last known numbers for every other row.
  for (const snapshot of snapshots) {
    const covered = snapshot.playlists.map((playlist) => playlist.name);
    const want = COVERAGE[snapshot.file];
    check(
      want !== undefined && JSON.stringify(covered) === JSON.stringify(want),
      `snapshot over ${snapshot.file} covered ${JSON.stringify(covered)}, want ${JSON.stringify(want)}`,
    );
  }

  // The byte tallies only grow, per playlist, across the whole scan — and they
  // do move mid-file: 00011.M2TS spans two read chunks, so the playlists that
  // play it are seen part-measured before they are seen whole.
  const seen = new Map();
  const first = new Map();
  for (const snapshot of snapshots) {
    for (const playlist of snapshot.playlists) {
      const previous = seen.get(playlist.name) ?? 0;
      check(
        playlist.measuredBytes >= previous,
        `${playlist.name} fell from ${previous} to ${playlist.measuredBytes}`,
      );
      seen.set(playlist.name, playlist.measuredBytes);
      if (!first.has(playlist.name)) {
        first.set(playlist.name, playlist.measuredBytes);
      }
    }
  }
  check(
    first.get("00000.MPLS") < seen.get("00000.MPLS"),
    `the shared clip must be seen part-measured first: ${first.get("00000.MPLS")} then ${seen.get("00000.MPLS")}`,
  );

  // Where the ticking ends is where the report begins: for every playlist, the
  // last snapshot that covered it carries the finished model's numbers — the
  // playlist total, each clip row, and each stream rate keyed by (pid, angle).
  const last = new Map();
  for (const snapshot of snapshots) {
    for (const playlist of snapshot.playlists) {
      last.set(playlist.name, playlist);
    }
  }
  for (const playlist of watched.disc.playlists) {
    const live = last.get(playlist.name);
    if (live === undefined) {
      check(false, `no snapshot ever covered ${playlist.name}`);
      continue;
    }
    check(
      live.measuredBytes === modelBytes(playlist) && live.measuredBytes > 0,
      `${playlist.name} settled at ${live.measuredBytes}, model says ${modelBytes(playlist)}`,
    );
    check(
      live.clips.length === playlist.clips.length &&
        live.clips.every(
          (clip, index) =>
            clip.name === playlist.clips[index].name &&
            clip.angleIndex === playlist.clips[index].angleIndex &&
            clip.measuredBytes === playlist.clips[index].packetCount * PACKET_BYTES,
        ),
      `${playlist.name} clip rows diverged from the model`,
    );
    check(
      live.streams.length === playlist.streams.length &&
        playlist.streams.every((stream) => {
          const rate = live.streams.find(
            (row) => row.pid === stream.pid && row.angleIndex === stream.angleIndex,
          );
          return (
            rate !== undefined &&
            rate.bitrateBps === stream.bitrateBps &&
            rate.activeBitrateBps === stream.activeBitrateBps
          );
        }),
      `${playlist.name} stream rates diverged from the model`,
    );
  }

  // The callback is additive: the same scan without one renders the identical
  // report, so an existing five-argument caller sees no change at all.
  const plain = scan_files(paths, files, []);
  check(
    plain.report === watched.report,
    `an unobserved scan rendered ${plain.report.length} bytes against ${watched.report.length}`,
  );

  // ── the rate gate ──────────────────────────────────────────────────────────

  // The gate under the scan's own cadence, on a mock clock: seven snapshots
  // 100 ms apart is 600 ms of scanning, which the page must see as ONE update
  // (the first) rather than seven.
  let clock = 0;
  const relayed = [];
  const gate = throttleMeasured(
    (snapshot) => relayed.push(snapshot),
    MEASURE_INTERVAL_MS,
    () => clock,
  );
  for (const snapshot of snapshots) {
    gate(snapshot);
    clock += 100;
  }
  check(
    relayed.length === 1 && relayed[0] === snapshots[0],
    `a fast scan must relay its first snapshot and no more, got ${relayed.length}`,
  );

  // And over a longer run at the same speed: one relay per interval, never two,
  // with the first one immediate. 1,000 events 5 ms apart span 4,995 ms, so the
  // gate passes those at 0, 1000, 2000, 3000 and 4000 ms.
  clock = 0;
  const burst = [];
  const fast = throttleMeasured(
    (snapshot) => burst.push(snapshot),
    MEASURE_INTERVAL_MS,
    () => clock,
  );
  const at = [];
  for (let i = 0; i < 1000; i++) {
    clock = i * 5;
    fast({ file: `${i}`, playlists: [] });
    if (burst.length > at.length) {
      at.push(clock);
    }
  }
  check(
    burst.length === 5 && JSON.stringify(at) === JSON.stringify([0, 1000, 2000, 3000, 4000]),
    `the gate relayed ${burst.length} of 1,000 events, at ${JSON.stringify(at)}`,
  );

  if (failures.length === 0) {
    console.log(
      `PASS — ${snapshots.length} live snapshots over ${Object.keys(COVERAGE).length} stream files: ` +
        "per-file coverage, monotone tallies, mid-file movement, they settle on the model, " +
        "an unobserved scan is unchanged, and the gate holds 1,000 events to 5 relays.",
    );
    process.exit(0);
  }

  console.error(`FAIL — ${failures.length} live-measured assertion(s):`);
  for (const failure of failures) {
    console.error(`  - ${failure}`);
  }
  process.exit(1);
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
