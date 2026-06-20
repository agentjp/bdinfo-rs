# Real-disc test fixtures

Tiny but **real** BD-ROM discs used by the cross-platform end-to-end test in
[`../cli.rs`](../cli.rs) (`a_real_bdmv_folder_scan_*` / `a_real_iso_scan_*`). The
binary scans each disc and its report is compared byte-for-byte against a committed
golden, on a native runner for every architecture we release.

| Path | What it is |
| --- | --- |
| `BigBuckBunny/` | a BDMV folder (disc label `BigBuckBunny`) |
| `BigBuckBunny.iso` | the same disc as a UDF `.iso` (volume label `Blu-Ray`) |
| `golden/folder.txt` | the exact report the folder scan must produce |
| `golden/iso.txt` | the exact report the `.iso` scan must produce |

The two goldens are identical except the `Disc Label:` line — which also pins the
documented "a folder takes its directory name, an `.iso` reads the real UDF volume
label" behaviour. They carry the report's locked **CRLF** byte contract; `.gitattributes`
keeps them (`-text`) and the disc bytes (`binary`) verbatim across platforms.

## Attribution

The audio/video content is **_Big Buck Bunny_ © 2008 Blender Foundation**
(<https://peach.blender.org>), licensed **Creative Commons Attribution 3.0**
(<https://creativecommons.org/licenses/by/3.0/>).

A short clip was re-encoded to a Blu-ray-compliant H.264 1080p video track and a
48 kHz/16-bit stereo **LPCM** audio track (the only freely redistributable Blu-ray
audio — AC-3/DTS are patent-encumbered), then muxed into the BD-ROM structures above
with [tsMuxeR](https://tsmuxer.com/). The visual content is irrelevant to the test;
only the disc structure and stream metadata are exercised.

## Regenerating

If the locked report format deliberately changes, re-pin both goldens by scanning
the committed discs and overwriting `golden/folder.txt` and `golden/iso.txt`:

```pwsh
bdinfo-rs -m 00000 crates/bdinfo-rs/tests/fixtures/BigBuckBunny     $tmp
bdinfo-rs -m 00000 crates/bdinfo-rs/tests/fixtures/BigBuckBunny.iso $tmp
# copy $tmp/BDINFO.BigBuckBunny.txt -> golden/folder.txt
# copy $tmp/BDINFO.Blu-Ray.txt      -> golden/iso.txt
```
