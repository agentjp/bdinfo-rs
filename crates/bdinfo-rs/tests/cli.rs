//! End-to-end smoke tests that drive the built `bdinfo-rs` binary.
//!
//! `unused_crate_dependencies` is a known false positive for integration tests:
//! cargo makes the binary's deps (`clap`, `bdinfo-rs-core`) visible to this test
//! crate, but a black-box CLI test legitimately uses neither — it only spawns the
//! compiled binary. The expect is scoped to this file; the lint stays `deny` for
//! all real code.
#![expect(
    unused_crate_dependencies,
    reason = "black-box CLI test spawns the built binary; it links the bin's \
              deps (clap, bdinfo-rs-core) but uses neither directly"
)]

use std::path::PathBuf;
use std::process::Command;

/// A `Command` for the binary under test (path injected by cargo).
fn bdinfo_rs() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bdinfo-rs"))
}

#[test]
fn version_prints_and_succeeds() {
    for flag in ["-v", "--version"] {
        let output = bdinfo_rs().arg(flag).output().expect("spawn bdinfo-rs");
        assert!(output.status.success(), "{flag}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("bdinfo-rs "), "{flag}: {stdout}");
    }
}

#[test]
fn help_shows_the_normalized_surface_and_exits_zero() {
    let output = bdinfo_rs().arg("--help").output().expect("spawn bdinfo-rs");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // The positional surface plus the four flags…
    assert!(stdout.contains("<BD_PATH>"), "help: {stdout}");
    assert!(stdout.contains("[REPORT_DEST]"), "help: {stdout}");
    for flag in ["-l, --list", "-m, --mpls", "-w, --whole", "-h, --help", "-v, --version"] {
        assert!(stdout.contains(flag), "help is missing {flag}: {stdout}");
    }
    // …and nothing else: no subcommands, no removed switches.
    for gone in ["dump", "--strict", "--output", "--save", "--no-", "--level"] {
        assert!(!stdout.contains(gone), "help still shows {gone}: {stdout}");
    }
}

#[test]
fn no_arguments_print_help_and_exit_zero() {
    // A bare invocation is treated as a help request: the long help goes to
    // stdout and the process exits 0 (not clap's exit-2 usage error), with
    // nothing on stderr. Any actual argument still parses — see
    // `a_subcommand_style_invocation_is_just_a_bad_path` for the exit-2 path.
    let output = bdinfo_rs().output().expect("spawn bdinfo-rs");
    assert!(output.status.success(), "no-args exits 0: {:?}", output.status.code());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Usage:"), "no-args prints usage: {stdout}");
    assert!(stdout.contains("<BD_PATH>"), "no-args prints help: {stdout}");
    assert!(output.stderr.is_empty(), "no error output on a help request: {:?}", output.stderr);
}

/// A valid zero-item `*.mpls` (magic `MPLS0300`, one empty `PlayList`, no marks)
/// — enough for the scan to emit its playlist row.
fn empty_mpls() -> Vec<u8> {
    let playlist_offset: u32 = 0x3C;
    let playlist: Vec<u8> = [
        [0_u8; 4].as_slice(), // playlistLength
        &[0_u8; 2],           // reserved
        &0_u16.to_be_bytes(), // itemCount = 0
        &[0_u8; 2],           // subitemCount
    ]
    .concat();
    let chapters_offset = playlist_offset.wrapping_add(u32::try_from(playlist.len()).unwrap_or(0));
    let mut buf = b"MPLS0300".to_vec();
    buf.extend_from_slice(&playlist_offset.to_be_bytes());
    buf.extend_from_slice(&chapters_offset.to_be_bytes());
    buf.resize(usize::try_from(playlist_offset).unwrap_or(0), 0);
    buf.extend_from_slice(&playlist);
    buf.extend_from_slice(&[0_u8; 4]); // PlayListMark length
    buf.extend_from_slice(&0_u16.to_be_bytes()); // zero marks
    buf
}

/// A valid single-item `*.mpls`: one 60-second `PlayItem` over clip
/// `00000.M2TS` — long enough to survive the default playlist filter, so the
/// table lists it.
#[expect(
    clippy::expect_used,
    reason = "test fixture setup; a failed conversion should abort the test loudly"
)]
fn one_item_mpls() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"00000"); // clip name
    body.extend_from_slice(b"M2TS");
    body.extend_from_slice(&[0_u8; 3]); // codec id pad + flags
    body.extend_from_slice(&0_u32.to_be_bytes()); // in time (45 kHz)
    body.extend_from_slice(&2_700_000_u32.to_be_bytes()); // out time: 60 s
    body.extend_from_slice(&[0_u8; 12]);
    body.extend_from_slice(&[0_u8; 4]); // STN table length + reserved
    body.extend_from_slice(&[0_u8; 12]); // the empty stream counts + reserved

    let playlist_offset: usize = 0x3C;
    let mut playlist = Vec::new();
    playlist.extend_from_slice(&[0_u8; 4]); // PlayList length
    playlist.extend_from_slice(&[0_u8; 2]); // reserved
    playlist.extend_from_slice(&1_u16.to_be_bytes()); // item count
    playlist.extend_from_slice(&[0_u8; 2]); // sub-item count
    playlist.extend_from_slice(&u16::try_from(body.len()).expect("item length").to_be_bytes());
    playlist.extend_from_slice(&body);
    let chapters_offset = playlist_offset.wrapping_add(playlist.len());

    let mut buf = b"MPLS0300".to_vec();
    buf.extend_from_slice(&u32::try_from(playlist_offset).expect("offset").to_be_bytes());
    buf.extend_from_slice(&u32::try_from(chapters_offset).expect("offset").to_be_bytes());
    buf.extend_from_slice(&[0_u8; 4]); // extensions offset
    buf.resize(playlist_offset, 0);
    buf.extend_from_slice(&playlist);
    buf.extend_from_slice(&[0_u8; 4]); // PlayListMark length
    buf.extend_from_slice(&0_u16.to_be_bytes()); // zero marks
    buf
}

/// A valid `*.clpi` declaring one AVC 1080p video stream at PID 0x1011.
#[expect(
    clippy::expect_used,
    reason = "test fixture setup; a failed conversion should abort the test loudly"
)]
fn avc_clpi() -> Vec<u8> {
    let mut clip_data = vec![0_u8, 1]; // reserved + num_prog = 1
    clip_data.extend_from_slice(&[0_u8; 6]); // spn start + program_map_pid
    clip_data.push(1); // stream count
    clip_data.push(0); // num_groups
    clip_data.extend_from_slice(&0x1011_u16.to_be_bytes());
    clip_data.push(5); // coding-info length
    clip_data.push(0x1B); // AVC video
    clip_data.extend_from_slice(&[0x62, 0x30, 0, 0]); // 1080p / 24 fps
    let mut buf = b"HDMV0300".to_vec();
    buf.extend_from_slice(&[0_u8; 4]);
    buf.extend_from_slice(&16_u32.to_be_bytes()); // ProgramInfo address
    buf.extend_from_slice(&u32::try_from(clip_data.len()).expect("length").to_be_bytes());
    buf.extend_from_slice(&clip_data);
    buf
}

/// A throwaway BD folder with one zero-item playlist (`00000.MPLS`, zero
/// length — dropped by the default playlist filter, so the table lists
/// nothing) and one stream file. Caller removes it.
#[expect(
    clippy::expect_used,
    reason = "test fixture setup; a failed write should abort the test loudly"
)]
fn report_bd(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("bdinfo-rs-e2e-{tag}-{}", std::process::id()));
    let bdmv = root.join("BDMV");
    std::fs::create_dir_all(bdmv.join("PLAYLIST")).expect("create PLAYLIST");
    std::fs::create_dir_all(bdmv.join("CLIPINF")).expect("create CLIPINF");
    std::fs::create_dir_all(bdmv.join("STREAM")).expect("create STREAM");
    std::fs::write(bdmv.join("PLAYLIST").join("00000.mpls"), empty_mpls()).expect("write mpls");
    std::fs::write(bdmv.join("STREAM").join("00000.m2ts"), vec![0_u8; 4096]).expect("write m2ts");
    root
}

/// A throwaway BD folder whose 60-second playlist `00000.MPLS` survives the
/// default filter — the table lists it, `--whole` scans it, and the picker
/// can select it as `1`. Caller removes it.
#[expect(
    clippy::expect_used,
    reason = "test fixture setup; a failed write should abort the test loudly"
)]
fn movie_bd(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("bdinfo-rs-e2e-{tag}-{}", std::process::id()));
    let bdmv = root.join("BDMV");
    std::fs::create_dir_all(bdmv.join("PLAYLIST")).expect("create PLAYLIST");
    std::fs::create_dir_all(bdmv.join("CLIPINF")).expect("create CLIPINF");
    std::fs::create_dir_all(bdmv.join("STREAM")).expect("create STREAM");
    std::fs::write(bdmv.join("PLAYLIST").join("00000.mpls"), one_item_mpls()).expect("write mpls");
    std::fs::write(bdmv.join("CLIPINF").join("00000.clpi"), avc_clpi()).expect("write clpi");
    std::fs::write(bdmv.join("STREAM").join("00000.m2ts"), vec![0_u8; 4096]).expect("write m2ts");
    root
}

/// The default report file for a disc folder: `BDINFO.{dir name}.txt` inside it.
fn report_file(root: &std::path::Path) -> PathBuf {
    let label = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    root.join(format!("BDINFO.{label}.txt"))
}

#[test]
fn whole_narrates_the_classic_flow_and_saves_the_report() {
    let root = movie_bd("save");
    let output =
        bdinfo_rs().args([root.as_os_str(), "-w".as_ref()]).output().expect("spawn bdinfo-rs");
    let report = std::fs::read(report_file(&root)).expect("read the saved report");
    let _ = std::fs::remove_dir_all(&root).is_ok();

    assert!(output.status.success());
    let text = String::from_utf8_lossy(&report);
    assert!(text.contains("Disc Label:     "), "report: {text}");
    assert!(text.contains("BDInfo:         0.8.0.1\r\n"), "report: {text}");
    assert!(text.contains("BDINFO HOME:\r\n"), "report: {text}");
    assert!(text.contains("PLAYLIST: 00000.MPLS"), "report: {text}");
    // The whole classic narration lands on stdout, in flow order: the scan
    // preamble, the playlist table, the analysis preamble, the epilogue, and
    // the saved-report message naming the destination FOLDER.
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in [
        "Please wait while we scan the disc...",
        "#   Group  Playlist File  Length    Estimated Bytes Measured Bytes",
        "00000.MPLS     00:01:00",
        "Preparing to analyze the following:",
        "00000.MPLS --> 00000.M2TS",
        "Scan completed successfully.",
        "Please wait while we generate the report...",
        "Report saved to: ",
    ] {
        assert!(stdout.contains(line), "stdout is missing {line:?}: {stdout}");
    }
    assert!(
        !stdout.contains(&format!("Report saved to: {}", report_file(&root).display())),
        "the saved-report message names the folder, not the file"
    );
}

#[test]
fn a_report_dest_directory_receives_the_report() {
    let root = report_bd("dest");
    let dest = std::env::temp_dir().join(format!("bdinfo-rs-e2e-destdir-{}", std::process::id()));
    std::fs::create_dir_all(&dest).expect("create dest");
    let output = bdinfo_rs()
        .args([root.as_os_str(), dest.as_os_str(), "-w".as_ref()])
        .output()
        .expect("spawn bdinfo-rs");
    let saved = report_file(&dest).with_file_name(format!(
        "BDINFO.{}.txt",
        root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    ));
    let written = saved.is_file();
    let in_disc = report_file(&root).exists();
    let _ = std::fs::remove_dir_all(&root).is_ok();
    let _ = std::fs::remove_dir_all(&dest).is_ok();

    assert!(output.status.success());
    assert!(written, "the report lands in REPORT_DEST");
    assert!(!in_disc, "nothing is written into BD_PATH");
}

#[test]
fn a_missing_report_dest_exits_2() {
    let root = report_bd("baddest");
    let output = bdinfo_rs()
        .args([&*root.to_string_lossy(), "no/such/dest/xyzzy-42"])
        .output()
        .expect("spawn bdinfo-rs");
    let _ = std::fs::remove_dir_all(&root).is_ok();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not a directory"));
}

#[test]
fn missing_bd_path_exits_2() {
    let output = bdinfo_rs().arg("no/such/disc/xyzzy-42").output().expect("spawn bdinfo-rs");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn an_existing_non_bd_path_exits_1() {
    // The manifest dir exists but is not a BD structure → "unable to locate".
    let output = bdinfo_rs().arg(env!("CARGO_MANIFEST_DIR")).output().expect("spawn bdinfo-rs");
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("error:"));
}

#[test]
fn whole_selects_only_what_the_table_lists() {
    // The zero-length playlist falls to the default filter, so the table
    // lists nothing and `--whole` selects nothing: the report is written
    // with no playlist sections. Only `--mpls` reaches filtered playlists.
    let root = report_bd("whole");
    let path = root.to_string_lossy().into_owned();
    let whole = bdinfo_rs().args([&path, "--whole"]).output().expect("spawn bdinfo-rs");
    let whole_report = std::fs::read_to_string(report_file(&root)).expect("read report");
    let _ = std::fs::remove_dir_all(&root).is_ok();

    assert!(whole.status.success());
    assert!(!whole_report.contains("PLAYLIST: 00000.MPLS"));
    assert!(whole_report.contains("Disc Label:     "));
}

#[test]
fn mpls_selects_only_the_named_playlists() {
    let root = report_bd("mpls");
    let path = root.to_string_lossy().into_owned();

    let selected = bdinfo_rs().args([&path, "--mpls", "00000"]).output().expect("spawn bdinfo-rs");
    let report = std::fs::read_to_string(report_file(&root)).expect("read report");
    let unknown = bdinfo_rs().args([&path, "--mpls", "99999"]).output().expect("spawn bdinfo-rs");
    let _ = std::fs::remove_dir_all(&root).is_ok();

    // The named playlist reports even though the default filter would drop
    // it, and the flow echoes the requested list without printing a table.
    assert!(selected.status.success());
    assert!(report.contains("PLAYLIST: 00000.MPLS"));
    let stdout = String::from_utf8_lossy(&selected.stdout);
    assert!(stdout.contains("\n00000\n"), "the mpls list echoes: {stdout}");
    assert!(!stdout.contains("Playlist File"), "no table in mpls mode: {stdout}");
    // A selection matching nothing is a fatal error.
    assert_eq!(unknown.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("No matching playlists found on BD"));
}

#[test]
fn list_prints_the_playlist_table_and_exits() {
    let root = movie_bd("list");
    let path = root.to_string_lossy().into_owned();
    let output = bdinfo_rs().args([&path, "--list"]).output().expect("spawn bdinfo-rs");
    let wrote_report = report_file(&root).exists();
    let _ = std::fs::remove_dir_all(&root).is_ok();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("#   Group  Playlist File  Length    Estimated Bytes Measured Bytes"),
        "table: {stdout}"
    );
    assert!(stdout.contains("1   1      00000.MPLS     00:01:00"), "table: {stdout}");
    assert!(!stdout.contains("Preparing to analyze"), "--list exits after the table: {stdout}");
    assert!(!wrote_report, "--list writes no report file");
}

#[test]
fn progress_stays_on_stderr_and_the_epilogue_on_stdout() {
    let root = movie_bd("progress");
    let path = root.to_string_lossy().into_owned();
    let output = bdinfo_rs().args([&path, "--whole"]).output().expect("spawn bdinfo-rs");
    let _ = std::fs::remove_dir_all(&root).is_ok();

    assert!(output.status.success());
    // The live progress redraws on stderr…
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Scanning"), "the progress line draws: {stderr}");
    assert!(stderr.contains("% - 00000.M2TS | Elapsed: "), "progress detail: {stderr}");
    // …and the classic epilogue is flow narration on stdout.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Scan completed successfully."), "stdout: {stdout}");
    assert!(!stderr.contains("Scan completed"), "no epilogue on stderr: {stderr}");
}

#[test]
fn scan_errors_use_the_classic_epilogue_and_exit_3() {
    let root = report_bd("errors");
    std::fs::write(root.join("BDMV").join("PLAYLIST").join("00001.mpls"), b"XXXXjunk")
        .expect("write corrupt mpls");
    let path = root.to_string_lossy().into_owned();
    let output = bdinfo_rs().args([&path, "--whole"]).output().expect("spawn bdinfo-rs");
    let report = std::fs::read_to_string(report_file(&root)).expect("read report");
    let _ = std::fs::remove_dir_all(&root).is_ok();

    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Scan completed with errors (see report)."), "stdout: {stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("warning: scan completed with 1 error(s):"), "stderr: {stderr}");
    // The report itself carries the WARNING block.
    assert!(report.contains("WARNING: File errors"));
}

#[test]
fn the_interactive_picker_selects_by_table_index() {
    use std::io::Write as _;
    let root = movie_bd("picker");
    let mut child = bdinfo_rs()
        .arg(root.as_os_str())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn bdinfo-rs");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"x\n9\n1\nq\n")
        .expect("write picker input");
    let output = child.wait_with_output().expect("wait for bdinfo-rs");
    let saved = report_file(&root).is_file();
    let _ = std::fs::remove_dir_all(&root).is_ok();

    assert!(output.status.success());
    assert!(saved, "the picked playlist reports");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Select (q when finished): "), "prompts: {stdout}");
    assert!(stdout.contains("Invalid Input!"), "rejects words: {stdout}");
    assert!(stdout.contains("Invalid Selection!"), "rejects out-of-range: {stdout}");
    assert!(stdout.contains("Added 1"), "confirms the pick: {stdout}");
    assert!(stdout.contains("00000.MPLS --> 00000.M2TS"), "analyzes the pick: {stdout}");
}

#[test]
fn an_empty_picker_selection_exits_without_a_report() {
    let root = movie_bd("noselection");
    let output = bdinfo_rs()
        .arg(root.as_os_str())
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn bdinfo-rs");
    let saved = report_file(&root).exists();
    let _ = std::fs::remove_dir_all(&root).is_ok();

    assert!(output.status.success());
    assert!(!saved, "no selection, no report");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No playlists selected. Exiting."), "stdout: {stdout}");
}

#[test]
fn a_subcommand_style_invocation_is_just_a_bad_path() {
    // The old subcommands are gone: `dump`/`report` parse as a BD_PATH that
    // does not exist.
    for word in ["dump", "report", "version"] {
        let output = bdinfo_rs().arg(word).output().expect("spawn bdinfo-rs");
        assert_eq!(output.status.code(), Some(2), "{word}");
    }
}

// --- Real-disc end-to-end: the same scan on every platform ---------------------
//
// The two fixtures under `tests/fixtures/` are tiny but REAL BD-ROM discs: a ~5 s
// Big Buck Bunny clip (CC BY 3.0 — see that dir's README) authored with tsMuxeR
// into a 1080p H.264 video track plus an LPCM audio track. One is a BDMV folder,
// the other the same disc as a UDF `.iso`. We scan each with the built binary and
// assert the report matches a committed golden byte-for-byte.
//
// This is the cross-platform guarantee. The report is locked (CRLF, UTF-8 no BOM,
// invariant number spellings, ties-to-even fixed point), so one golden must
// reproduce identically on x86_64 and aarch64 across Linux, Windows and macOS —
// the CI `test` matrix runs this on a native runner for every released binary, and
// a byte differing between arches would mean a real determinism bug. The
// `.gitattributes` rules keep the disc bytes (`binary`) and the golden's CRLF
// (`-text`) verbatim so checkout can't perturb either.

/// Absolute path to a committed real-disc fixture under `tests/fixtures/`.
fn real_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name)
}

/// Scan `disc` with `-m 00000` into a fresh temp dest and return the report it
/// writes as `BDINFO.{label}.txt`. `label` is the disc label the scan derives: a
/// folder takes its directory name, an `.iso` its UDF volume label.
#[expect(
    clippy::expect_used,
    reason = "end-to-end test driver; a failed spawn / read / decode should abort the test loudly"
)]
fn scan_report(disc: &std::path::Path, label: &str, tag: &str) -> String {
    let dest = std::env::temp_dir().join(format!("bdinfo-rs-real-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dest).expect("create dest");
    let output = bdinfo_rs()
        .args([disc.as_os_str(), dest.as_os_str(), "-m".as_ref(), "00000".as_ref()])
        .output()
        .expect("spawn bdinfo-rs");
    let report = std::fs::read(dest.join(format!("BDINFO.{label}.txt"))).expect("read the report");
    let _ = std::fs::remove_dir_all(&dest).is_ok();
    assert!(output.status.success(), "scan failed: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8(report).expect("the report is valid UTF-8")
}

#[test]
fn a_real_bdmv_folder_scan_matches_the_golden_byte_for_byte() {
    let got = scan_report(&real_fixture("BigBuckBunny"), "BigBuckBunny", "folder");
    assert_eq!(
        got,
        include_str!("fixtures/golden/folder.txt"),
        "folder report drifted from golden"
    );
}

#[test]
fn a_real_iso_scan_matches_the_golden_byte_for_byte() {
    let got = scan_report(&real_fixture("BigBuckBunny.iso"), "Blu-Ray", "iso");
    assert_eq!(got, include_str!("fixtures/golden/iso.txt"), "iso report drifted from golden");
}
