# Disc test fixtures

Tiny BD-ROM discs the end-to-end tests in [`../cli.rs`](../cli.rs) scan with the built
binary. `BigBuckBunny` is a **real** encode and owns the report's byte contract: its
report is compared byte-for-byte against a committed golden, on a native runner for
every architecture we release. `MultiPlaylist` is **synthetic** and owns disc *shape* —
several playlists over several clips — with no golden of its own.

| Path | What it is |
| --- | --- |
| `BigBuckBunny/` | a BDMV folder (disc label `BigBuckBunny`) |
| `BigBuckBunny.iso` | the same disc as a UDF `.iso` (volume label `Blu-Ray`) |
| `MultiPlaylist/` | a synthetic BDMV folder: three playlists over three clips |
| `golden/folder.txt` | the exact report the folder scan must produce |
| `golden/iso.txt` | the exact report the `.iso` scan must produce |
| `golden/folder-no-stream-diagnostics.txt` | the folder report under `--no-stream-diagnostics` |
| `golden/folder-no-quick-summary.txt` | the folder report under `--no-quick-summary` |

`folder.txt` and `iso.txt` are identical except the `Disc Label:` line — which also pins
the documented "a folder takes its directory name, an `.iso` reads the real UDF volume
label" behaviour. The other two are `folder.txt` minus exactly one optional section each,
which is what pins that the two switches subtract and never reword. All four carry the
report's locked **CRLF** byte contract; `.gitattributes` keeps them (`-text`) and the disc
bytes (`binary`) verbatim across platforms.

## `MultiPlaylist`

`BigBuckBunny` has one playlist over one clip, so no selection can change what it
reports: `--list`, `-m 00000` and `--whole` all name the same row. `MultiPlaylist`
exists for everything that needs a disc where they can differ — subset selection,
per-playlist accounting, and injecting a read failure into one clip and watching
which playlists move. It is 283 KiB and carries no copyrighted content: every byte
is generated.

| Playlist | Length | Play items | Chapter marks (playlist seconds) |
| --- | --- | --- | --- |
| `00000.MPLS` | 30 s | `00011.M2TS` | 0, 10, 20 |
| `00001.MPLS` | 25 s | `00022.M2TS` | 0, 8, 16 |
| `00002.MPLS` | 50 s | `00033.M2TS` + `00011.M2TS` | 0, 10, 20, 35 |

Clip stems (`000{11,22,33}`) never collide with playlist stems, so a name in a log
says which kind of file it came from. `00011.M2TS` is shared: a failure injected into
it must show up in both `00000.MPLS` and `00002.MPLS`. `00001.MPLS` shares no clip
with the others, which is why the listing puts it in a group of its own. Every
playlist clears the default 20-second short-playlist filter, and none replays a clip
from the same in-time, so none is filtered as looping — the default listing shows all
three.

Each clip declares one AVC video stream (PID 0x1011, 1080p / 24 fps / 16:9) and one
AC-3 audio stream (PID 0x1100, 5.1 / 48 kHz / English), in both its `*.clpi` and its
playlist Stream-Number table. Each second of each `*.m2ts` carries a PAT and a PMT,
ten video access units (the first a 901-byte key frame spanning five transport
packets, the rest one packet each) and four audio access units, all timestamped on
the 90 kHz clock. That is what makes a scan of this disc produce *measured* numbers
— roughly 19 kbps video and 5 kbps audio — where a zero-filled stream file would
leave every rate at zero.

The bytes are minimal on purpose, and these limits are deliberate:

- **The elementary payloads are stubs.** A video access unit is an access-unit delimiter, a
  7-byte sequence-parameter-set prefix (enough for the profile/level the report prints) and
  filler; an audio one is an AC-3 sync-frame header and filler. Nothing here decodes to
  pictures or sound. The AC-3 header declares 640 kbps, which is nominal — the disc carries
  far less.
- **No PCR is inserted**, though the PMT names the video PID as the PCR source.
- **`index.bdmv` and `MovieObject.bdmv` are structural stubs**: correct magic (`INDX0200` /
  `MOBJ0200`) and length fields over empty navigation tables. Neither this tool nor classic
  BDInfo reads past the index magic (`BDROM.cs`, `ReadIndexVersion`).
- **The `*.clpi` files carry a `ProgramInfo` block and nothing else** — no sequence info, no
  EP map. That is the only block either tool reads.
- `BDMV/BACKUP/` holds copies of every metadata file, so the resilient scan's backup-recovery
  path has something to recover from.

## `BigBuckBunny` attribution

The audio/video content is **_Big Buck Bunny_ © 2008 Blender Foundation**
(<https://peach.blender.org>), licensed **Creative Commons Attribution 3.0**
(<https://creativecommons.org/licenses/by/3.0/>).

A ~30-second clip (`00000.MPLS`, `00:00:30.113`) was re-encoded to a Blu-ray-compliant
H.264 1080p video track and a 48 kHz/16-bit stereo **LPCM** audio track (the only freely
redistributable Blu-ray audio — AC-3/DTS are patent-encumbered), then muxed into the
BD-ROM structures above with [tsMuxeR](https://github.com/justdan96/tsMuxer). The visual
content is irrelevant to the test; only the disc structure and stream metadata are
exercised. The length is deliberate: it clears the default 20-second playlist filter, so
the table-driven presentation path (`--list` / `--whole` / the picker) is exercised
end-to-end, not just the `-m 00000` direct selection.

## Regenerating the goldens

If the locked report format deliberately changes, re-pin **all four** goldens by scanning
the committed discs. `MultiPlaylist` has no golden and needs nothing here. Each scan
writes `BDINFO.{label}.txt` into `$tmp`, so run them one at a time and copy the result
before the next overwrites it:

```pwsh
bdinfo-rs -m 00000 crates/bdinfo-rs/tests/fixtures/BigBuckBunny     $tmp
# copy $tmp/BDINFO.BigBuckBunny.txt -> golden/folder.txt

bdinfo-rs -m 00000 crates/bdinfo-rs/tests/fixtures/BigBuckBunny.iso $tmp
# copy $tmp/BDINFO.Blu-Ray.txt      -> golden/iso.txt

bdinfo-rs -m 00000 crates/bdinfo-rs/tests/fixtures/BigBuckBunny $tmp --no-stream-diagnostics
# copy $tmp/BDINFO.BigBuckBunny.txt -> golden/folder-no-stream-diagnostics.txt

bdinfo-rs -m 00000 crates/bdinfo-rs/tests/fixtures/BigBuckBunny $tmp --no-quick-summary
# copy $tmp/BDINFO.BigBuckBunny.txt -> golden/folder-no-quick-summary.txt
```
