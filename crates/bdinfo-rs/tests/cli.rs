//! End-to-end smoke tests that drive the built `bdinfo-rs` binary.
//!
//! `unused_crate_dependencies` is a known false positive for integration tests:
//! cargo makes the binary's deps (`clap`, `bdinfo-rs-core`) visible to this test
//! crate, but a black-box CLI test legitimately uses neither — it only spawns the
//! compiled binary. The expect is scoped to this file; the lint stays `deny` for
//! all real code.
#![expect(
    unused_crate_dependencies,
    reason = "black-box CLI test spawns the built binary; it links the bin's clap but never \
              names it (bdinfo-rs-core IS named, but only for the shared threshold ceiling)"
)]

pub mod common;

use std::path::{Path, PathBuf};

use common::{bdinfo_rs, real_fixture};

#[test]
fn version_prints_and_succeeds() {
    for flag in ["-v", "--version"] {
        let output = bdinfo_rs().arg(flag).output().expect("spawn bdinfo-rs");
        assert!(output.status.success(), "{flag}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("bdinfo-rs "), "{flag}: {stdout}");
    }
}

/// The help card the binary prints, byte for byte — usage line included, which
/// holds only because the command pins `bin_name`: clap otherwise names the
/// program as it was invoked, `.exe` suffix and all.
const CARD: &str = "\
Usage: bdinfo-rs [OPTIONS] <BD_PATH> [REPORT_DEST]

Arguments:
  <BD_PATH>      BDMV folder or .iso image
  [REPORT_DEST]  Report folder (default: BD_PATH; required for .iso)

Options:
  -l, --list                              List playlists and exit
  -m, --mpls <NAME,...>                   Scan only the named playlists
  -w, --whole                             Scan every listed playlist
      --show-short-playlists              Also list short playlists
      --show-looping-playlists            Also list looping playlists
      --short-playlist-seconds <SECONDS>  Short-playlist cutoff (default: 20)
      --drop-partial                      Discard partially scanned stream data
      --no-stream-diagnostics             Omit the STREAM DIAGNOSTICS sections
      --no-quick-summary                  Omit the QUICK SUMMARY blocks
      --no-banner                         Never print the banner
  -v, --version                           Print version
  -h, --help                              Print help
";

#[test]
fn every_help_path_prints_the_same_compact_card() {
    let long = bdinfo_rs().arg("--help").output().expect("spawn bdinfo-rs");
    let short = bdinfo_rs().arg("-h").output().expect("spawn bdinfo-rs");
    let bare = bdinfo_rs().output().expect("spawn bdinfo-rs");

    // Every help path succeeds, the bare run included: it is a help request,
    // not a usage error, so an install validator smoke-running the executable
    // sees a clean exit.
    assert!(long.status.success(), "--help: {:?}", long.status.code());
    assert!(short.status.success(), "-h: {:?}", short.status.code());
    assert!(bare.status.success(), "bare run: {:?}", bare.status.code());
    assert_eq!(short.stdout, long.stdout, "-h and --help differ");
    assert_eq!(bare.stdout, long.stdout, "a bare run and --help differ");
    for output in [&long, &short, &bare] {
        assert!(output.stderr.is_empty(), "the help is not an error report: {:?}", output.stderr);
    }

    // A pipe gets the plain header, a blank line, then the card.
    let stdout = String::from_utf8_lossy(&long.stdout);
    let expected = format!(
        "bdinfo-rs {} - BDInfo-style Blu-ray disc reports\n\n{CARD}",
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(stdout, expected, "the help page drifted");
    // No colour when piped, and one screen wide.
    assert!(!stdout.contains('\u{1b}'), "escape codes in a piped help: {stdout:?}");
    for line in stdout.lines() {
        assert!(line.chars().count() <= 80, "wider than 80 columns: {line:?}");
    }
}

#[test]
fn no_banner_drops_the_header_from_the_help_page() {
    let output = bdinfo_rs().args(["--no-banner", "-h"]).output().expect("spawn bdinfo-rs");
    assert!(output.status.success());
    // The card alone: no header line, no blank line above it.
    assert_eq!(String::from_utf8_lossy(&output.stdout), CARD);
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
fn one_item_mpls() -> Vec<u8> {
    repeated_item_mpls(1, 60)
}

/// [`one_item_mpls`] with the play item repeated `items` times, each running
/// `seconds` from in-time 0. Every repeat replays one clip file from one
/// in-time, which is what the core's loop detection keys on, and the
/// playlist's total length is `items * seconds`.
#[expect(
    clippy::expect_used,
    reason = "test fixture setup; a failed conversion should abort the test loudly"
)]
fn repeated_item_mpls(items: u16, seconds: u32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(b"00000"); // clip name
    body.extend_from_slice(b"M2TS");
    body.extend_from_slice(&[0_u8; 3]); // codec id pad + flags
    body.extend_from_slice(&0_u32.to_be_bytes()); // in time (45 kHz)
    body.extend_from_slice(&seconds.wrapping_mul(45_000).to_be_bytes()); // out time
    body.extend_from_slice(&[0_u8; 12]);
    body.extend_from_slice(&[0_u8; 4]); // STN table length + reserved
    body.extend_from_slice(&[0_u8; 12]); // the empty stream counts + reserved

    let playlist_offset: usize = 0x3C;
    let mut playlist = Vec::new();
    playlist.extend_from_slice(&[0_u8; 4]); // PlayList length
    playlist.extend_from_slice(&[0_u8; 2]); // reserved
    playlist.extend_from_slice(&items.to_be_bytes()); // item count
    playlist.extend_from_slice(&[0_u8; 2]); // sub-item count
    for _ in 0..items {
        playlist.extend_from_slice(&u16::try_from(body.len()).expect("item length").to_be_bytes());
        playlist.extend_from_slice(&body);
    }
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

/// A throwaway BD folder whose four playlists over one shared clip cover every
/// combination the two filter rules can produce: `00000.MPLS` (60 s) survives
/// the default filter, `00001.MPLS` (two items, 120 s) loops, `00002.MPLS`
/// (10 s) is short, and `00003.MPLS` (two items, 10 s) is both. Caller removes
/// it.
#[expect(
    clippy::expect_used,
    reason = "test fixture setup; a failed write should abort the test loudly"
)]
fn filtered_bd(tag: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("bdinfo-rs-e2e-{tag}-{}", std::process::id()));
    let bdmv = root.join("BDMV");
    std::fs::create_dir_all(bdmv.join("PLAYLIST")).expect("create PLAYLIST");
    std::fs::create_dir_all(bdmv.join("CLIPINF")).expect("create CLIPINF");
    std::fs::create_dir_all(bdmv.join("STREAM")).expect("create STREAM");
    for (name, items, seconds) in
        [("00000", 1, 60), ("00001", 2, 60), ("00002", 1, 10), ("00003", 2, 5)]
    {
        std::fs::write(
            bdmv.join("PLAYLIST").join(format!("{name}.mpls")),
            repeated_item_mpls(items, seconds),
        )
        .expect("write mpls");
    }
    std::fs::write(bdmv.join("CLIPINF").join("00000.clpi"), avc_clpi()).expect("write clpi");
    std::fs::write(bdmv.join("STREAM").join("00000.m2ts"), vec![0_u8; 4096]).expect("write m2ts");
    root
}

/// The default report file for a disc folder: `BDINFO.{dir name}.txt` inside it.
fn report_file(root: &Path) -> PathBuf {
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
    // A scan flow opens on the scan notice: the help header reaches no other
    // path than the help itself.
    assert!(stdout.starts_with("Please wait while we scan the disc..."), "opening: {stdout}");
    assert!(
        stdout.contains("#   Group  Playlist File  Length    Estimated Bytes Measured Bytes"),
        "table: {stdout}"
    );
    assert!(stdout.contains("1   1      00000.MPLS     00:01:00"), "table: {stdout}");
    assert!(!stdout.contains("Preparing to analyze"), "--list exits after the table: {stdout}");
    assert!(!wrote_report, "--list writes no report file");
}

/// The `--list` stdout for `filtered_bd`, run with the given extra switches.
#[expect(
    clippy::expect_used,
    reason = "end-to-end test driver; a failed spawn should abort the test loudly"
)]
fn listing(disc: &Path, switches: &[&str]) -> String {
    let mut args = vec![disc.to_string_lossy().into_owned(), "--list".to_owned()];
    args.extend(switches.iter().map(|&s| s.to_owned()));
    let output = bdinfo_rs().args(&args).output().expect("spawn bdinfo-rs");
    assert!(output.status.success(), "{switches:?}: {}", String::from_utf8_lossy(&output.stderr));
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The table's data rows, in table order — the lines opening with the row
/// number, which the header and the hint lines never do.
fn table_row_lines(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|line| line.starts_with(|c: char| c.is_ascii_digit()) && line.contains(".MPLS"))
        .collect()
}

const LOOPING_HINT: &str =
    "Hidden by filters (looping): 00001.MPLS, 00003.MPLS - rerun with --show-looping-playlists";
const SHORT_HINT: &str =
    "Hidden by filters (short): 00002.MPLS, 00003.MPLS - rerun with --show-short-playlists";

#[test]
fn the_hidden_playlist_hint_follows_the_table_and_names_both_rules() {
    let root = filtered_bd("hint");
    let stdout = listing(&root, &[]);
    let _ = std::fs::remove_dir_all(&root).is_ok();

    // Only the plain 60-second playlist is listed…
    assert_eq!(table_row_lines(&stdout).len(), 1, "table: {stdout}");
    assert!(stdout.contains("1   1      00000.MPLS     00:01:00"), "table: {stdout}");
    // …and one hint line per rule follows the table, looping first, each
    // naming the playlists that rule withheld — 00003.MPLS is short AND
    // looping, so it is named on both. `--list` stops there, so the hint
    // block is the tail of the whole run.
    assert!(stdout.ends_with(&format!("{LOOPING_HINT}\n{SHORT_HINT}\n")), "hint block: {stdout}");
}

#[test]
fn each_show_switch_reveals_and_silences_only_its_own_category() {
    let root = filtered_bd("switches");
    let loops = listing(&root, &["--show-looping-playlists"]);
    let shorts = listing(&root, &["--show-short-playlists"]);
    let both = listing(&root, &["--show-looping-playlists", "--show-short-playlists"]);
    let _ = std::fs::remove_dir_all(&root).is_ok();

    // The looping switch admits the 120-second loop — the longest playlist on
    // the disc, so it heads the table — and drops the looping hint only.
    assert_eq!(table_row_lines(&loops).len(), 2, "table: {loops}");
    assert!(loops.contains("1   1      00001.MPLS     00:02:00"), "table: {loops}");
    assert!(!loops.contains("(looping)"), "the looping hint is silenced: {loops}");
    assert!(loops.contains(SHORT_HINT), "the short hint remains: {loops}");
    // The short switch admits the 10-second playlist and drops the short hint
    // only; 00003.MPLS is still withheld because it also loops.
    assert_eq!(table_row_lines(&shorts).len(), 2, "table: {shorts}");
    assert!(shorts.contains("2   1      00002.MPLS     00:00:10"), "table: {shorts}");
    assert!(!shorts.contains("(short)"), "the short hint is silenced: {shorts}");
    assert!(shorts.contains(LOOPING_HINT), "the looping hint remains: {shorts}");
    // Both switches list the whole disc and print no hint at all.
    assert_eq!(table_row_lines(&both).len(), 4, "table: {both}");
    assert!(both.contains("4   1      00003.MPLS     00:00:10"), "table: {both}");
    assert!(!both.contains("Hidden by filters"), "nothing is withheld: {both}");
}

#[test]
fn the_short_playlist_cutoff_moves_what_the_table_hides_and_what_the_hint_names() {
    let root = filtered_bd("cutoff");
    // Below every playlist's length: nothing is short any more, so the 10 s
    // 00002.MPLS joins the table and the short hint disappears. 00003.MPLS is
    // 10 s too but still loops, so the looping hint is unchanged.
    let lowered = listing(&root, &["--short-playlist-seconds", "5"]);
    // Above every playlist's length: all four are short, so the table empties
    // and the hint names them longest first.
    let raised = listing(&root, &["--short-playlist-seconds", "121"]);
    let _ = std::fs::remove_dir_all(&root).is_ok();

    assert_eq!(table_row_lines(&lowered).len(), 2, "table: {lowered}");
    assert!(lowered.contains("2   1      00002.MPLS     00:00:10"), "table: {lowered}");
    assert!(!lowered.contains("(short)"), "nothing is short at 5 s: {lowered}");
    assert!(lowered.contains(LOOPING_HINT), "the looping hint is unaffected: {lowered}");

    assert!(table_row_lines(&raised).is_empty(), "table: {raised}");
    assert!(
        raised.contains(
            "Hidden by filters (short): 00001.MPLS, 00000.MPLS, 00002.MPLS and 1 more - rerun \
             with --show-short-playlists"
        ),
        "the hint judges against the given cutoff: {raised}"
    );
}

#[test]
fn a_zero_cutoff_classifies_nothing_as_short() {
    let root = filtered_bd("cutoffzero");
    let stdout = listing(&root, &["--short-playlist-seconds", "0"]);
    let _ = std::fs::remove_dir_all(&root).is_ok();

    // Nothing is strictly shorter than zero seconds: the 10 s playlist joins
    // the table, the short hint disappears, and only the looping rule still
    // withholds.
    assert_eq!(table_row_lines(&stdout).len(), 2, "table: {stdout}");
    assert!(stdout.contains("2   1      00002.MPLS     00:00:10"), "table: {stdout}");
    assert!(!stdout.contains("(short)"), "nothing is short at 0 s: {stdout}");
    assert!(stdout.contains(LOOPING_HINT), "the looping hint is unaffected: {stdout}");
}

#[test]
fn the_cutoff_ceiling_matches_the_core_constant() {
    // `src/cli.rs` spells the 86_400 ceiling as a literal (`build.rs`
    // `include!`s that file without the library), so this is the check that
    // keeps the two in step: the constant's own value parses…
    let max = bdinfo_rs_core::bdrom::order::MAX_SHORT_PLAYLIST_SECONDS;
    let root = filtered_bd("cutoffceiling");
    let accepted = listing(&root, &["--short-playlist-seconds", &max.to_string()]);
    let _ = std::fs::remove_dir_all(&root).is_ok();
    assert!(table_row_lines(&accepted).is_empty(), "a day-long cutoff hides everything");

    // …and one past it fails at argument parsing: exit 2 (an invalid
    // argument), before any path is touched.
    let over = u64::from(max).checked_add(1).expect("86_401 fits a u64");
    let output = bdinfo_rs()
        .args(["X", "--short-playlist-seconds", &over.to_string()])
        .output()
        .expect("spawn bdinfo-rs");
    assert_eq!(output.status.code(), Some(2), "an out-of-range cutoff is a usage error");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("short-playlist-seconds") && stderr.contains(&over.to_string()),
        "the error names the flag and the rejected value: {stderr}"
    );
}

#[test]
fn showing_short_playlists_overrides_the_cutoff() {
    let root = filtered_bd("cutoffshow");
    // The cutoff classifies; the switch decides whether the classification
    // withholds anything. With both, every playlist is short and every one is
    // still listed — only the looping rule withholds.
    let stdout = listing(&root, &["--short-playlist-seconds", "121", "--show-short-playlists"]);
    let _ = std::fs::remove_dir_all(&root).is_ok();

    assert_eq!(table_row_lines(&stdout).len(), 2, "table: {stdout}");
    assert!(!stdout.contains("(short)"), "the short hint is silenced: {stdout}");
    assert!(stdout.contains(LOOPING_HINT), "the looping hint remains: {stdout}");
}

#[test]
fn mpls_mode_prints_no_table_and_no_hint() {
    let root = filtered_bd("mplshint");
    let path = root.to_string_lossy().into_owned();
    let named = bdinfo_rs().args([&path, "--mpls", "00001"]).output().expect("spawn bdinfo-rs");
    let named_report = std::fs::read(report_file(&root)).expect("read the report");
    let _ = std::fs::remove_file(report_file(&root)).is_ok();
    // The default table selects exactly 00000.MPLS, so `--whole` and
    // `-m 00000` must write the identical report — the hint is console
    // narration and never reaches the file.
    let whole = bdinfo_rs().args([&path, "--whole"]).output().expect("spawn bdinfo-rs");
    let whole_report = std::fs::read(report_file(&root)).expect("read the report");
    let _ = std::fs::remove_file(report_file(&root)).is_ok();
    let single = bdinfo_rs().args([&path, "--mpls", "00000"]).output().expect("spawn bdinfo-rs");
    let single_report = std::fs::read(report_file(&root)).expect("read the report");
    let _ = std::fs::remove_dir_all(&root).is_ok();

    // `--mpls` reaches a playlist the table hides, without a table or a hint.
    assert!(named.status.success());
    let stdout = String::from_utf8_lossy(&named.stdout);
    assert!(!stdout.contains("Playlist File"), "no table in mpls mode: {stdout}");
    assert!(!stdout.contains("Hidden by filters"), "no hint in mpls mode: {stdout}");
    assert!(String::from_utf8_lossy(&named_report).contains("PLAYLIST: 00001.MPLS"));
    assert!(whole.status.success());
    assert!(single.status.success());
    assert_eq!(whole_report, single_report, "the table path and -m write the same bytes");
    assert!(!String::from_utf8_lossy(&whole_report).contains("Hidden by filters"));
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
    // The session header greets a terminal only: piped here, so the picker
    // session opens on the scan notice like every other mode.
    assert!(stdout.starts_with("Please wait while we scan the disc..."), "opening: {stdout}");
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
// The two fixtures under `tests/fixtures/` are tiny but REAL BD-ROM discs: a ~30 s
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

/// Scan `disc` with `-m 00000` into a fresh temp dest and return the report it
/// writes as `BDINFO.{label}.txt`. `label` is the disc label the scan derives: a
/// folder takes its directory name, an `.iso` its UDF volume label.
fn scan_report(disc: &Path, label: &str, tag: &str) -> String {
    scan_report_with(disc, label, tag, &[])
}

/// [`scan_report`] with `switches` appended to the command line.
#[expect(
    clippy::expect_used,
    reason = "end-to-end test driver; a failed spawn / read / decode should abort the test loudly"
)]
fn scan_report_with(disc: &Path, label: &str, tag: &str, switches: &[&str]) -> String {
    let dest = std::env::temp_dir().join(format!("bdinfo-rs-real-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dest).expect("create dest");
    let output = bdinfo_rs()
        .args([disc.as_os_str(), dest.as_os_str(), "-m".as_ref(), "00000".as_ref()])
        .args(switches)
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

// The two report-section switches change the report bytes by design, so each
// gets a golden of its own beside the full one. Each golden is the full report
// minus exactly its own section — the pair pins that the switches subtract and
// never reword.

#[test]
fn no_stream_diagnostics_omits_exactly_that_section() {
    let got = scan_report_with(
        &real_fixture("BigBuckBunny"),
        "BigBuckBunny",
        "nodiag",
        &["--no-stream-diagnostics"],
    );
    assert_eq!(
        got,
        include_str!("fixtures/golden/folder-no-stream-diagnostics.txt"),
        "the trimmed report drifted from golden"
    );
    assert!(!got.contains("STREAM DIAGNOSTICS:"), "the section is gone: {got}");
    assert!(got.contains("QUICK SUMMARY:"), "the other section stays: {got}");
}

#[test]
fn no_quick_summary_omits_exactly_that_block() {
    let got = scan_report_with(
        &real_fixture("BigBuckBunny"),
        "BigBuckBunny",
        "nosummary",
        &["--no-quick-summary"],
    );
    assert_eq!(
        got,
        include_str!("fixtures/golden/folder-no-quick-summary.txt"),
        "the trimmed report drifted from golden"
    );
    assert!(!got.contains("QUICK SUMMARY:"), "the block is gone: {got}");
    assert!(got.contains("STREAM DIAGNOSTICS:"), "the other section stays: {got}");
}

#[test]
fn a_real_iso_scan_matches_the_golden_byte_for_byte() {
    let got = scan_report(&real_fixture("BigBuckBunny.iso"), "Blu-Ray", "iso");
    assert_eq!(got, include_str!("fixtures/golden/iso.txt"), "iso report drifted from golden");
}

// The clip is ~30 s — past the default 20 s playlist filter — so the default
// presentation path (no `-m`) is exercisable on a real disc, which the old ~5 s
// clip could not reach. The filtered table here is exactly `[00000.MPLS]`, so
// `--whole` selects the same lone playlist `-m 00000` does and must produce the
// identical bytes, and `--list` shows that table and exits.

#[test]
fn a_real_whole_folder_scan_matches_the_golden_byte_for_byte() {
    let disc = real_fixture("BigBuckBunny");
    let dest = std::env::temp_dir().join(format!("bdinfo-rs-real-whole-{}", std::process::id()));
    std::fs::create_dir_all(&dest).expect("create dest");
    let output = bdinfo_rs()
        .args([disc.as_os_str(), dest.as_os_str(), "-w".as_ref()])
        .output()
        .expect("spawn bdinfo-rs");
    let report = std::fs::read(dest.join("BDINFO.BigBuckBunny.txt")).expect("read the report");
    let _ = std::fs::remove_dir_all(&dest).is_ok();
    assert!(output.status.success(), "scan failed: {}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8(report).expect("the report is valid UTF-8"),
        include_str!("fixtures/golden/folder.txt"),
        "the default `--whole` selection drifted from the `-m 00000` golden"
    );
}

// --- AACS-encrypted discs: the refusal, `--list`, and the decrypted-rip pin ---

/// Copies the committed `BigBuckBunny` folder fixture to a fresh temp disc
/// root still named `BigBuckBunny` — the folder-derived disc label, and with
/// it the report's name and bytes, must not change. Returns the copy's root
/// and the parent to remove for cleanup.
#[expect(
    clippy::expect_used,
    reason = "end-to-end test driver; a failed copy should abort the test loudly"
)]
fn copy_fixture(tag: &str) -> (PathBuf, PathBuf) {
    fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).expect("create copy dir");
        for entry in std::fs::read_dir(from).expect("list fixture") {
            let entry = entry.expect("fixture entry");
            let target = to.join(entry.file_name());
            if entry.file_type().expect("entry type").is_dir() {
                copy_tree(&entry.path(), &target);
            } else {
                std::fs::copy(entry.path(), &target).expect("copy fixture file");
            }
        }
    }
    let parent = std::env::temp_dir().join(format!("bdinfo-rs-aacs-{tag}-{}", std::process::id()));
    let root = parent.join("BigBuckBunny");
    copy_tree(&real_fixture("BigBuckBunny"), &root);
    (root, parent)
}

/// Stamps the copied disc with the AACS marker: an *empty*
/// `AACS/Unit_Key_RO.inf`, zero bytes so the report's disc-size line — and
/// with it every report byte — is unchanged.
#[expect(
    clippy::expect_used,
    reason = "end-to-end test driver; a failed write should abort the test loudly"
)]
fn add_aacs_marker(root: &Path) {
    std::fs::create_dir_all(root.join("AACS")).expect("create AACS");
    std::fs::write(root.join("AACS").join("Unit_Key_RO.inf"), []).expect("write key file");
}

/// Rewrites the stream file the way AACS encryption leaves it: every
/// 6144-byte Aligned Unit keeps its first 16 bytes cleartext, the rest
/// becomes ciphertext-like filler.
#[expect(
    clippy::expect_used,
    reason = "end-to-end test driver; a failed rewrite should abort the test loudly"
)]
fn encrypt_stream(root: &Path) {
    let path = root.join("BDMV").join("STREAM").join("00000.m2ts");
    let mut bytes = std::fs::read(&path).expect("read the stream file");
    for unit in bytes.chunks_mut(6144) {
        for byte in unit.iter_mut().skip(16) {
            *byte = 0xAA;
        }
    }
    std::fs::write(&path, bytes).expect("write the encrypted stream");
}

#[test]
fn a_decrypted_rip_with_a_leftover_aacs_folder_matches_the_golden_byte_for_byte() {
    // The feature's safety pin at the report level: the AACS key file over
    // clear streams (a decrypted rip) changes not one byte of the report.
    let (root, parent) = copy_fixture("rip");
    add_aacs_marker(&root);
    let got = scan_report(&root, "BigBuckBunny", "ripdest");
    let _ = std::fs::remove_dir_all(&parent).is_ok();
    assert_eq!(
        got,
        include_str!("fixtures/golden/folder.txt"),
        "a decrypted rip's report drifted from the clear-disc golden"
    );
}

#[test]
fn an_encrypted_disc_refuses_the_scan_with_exit_4_and_a_notice() {
    let (root, parent) = copy_fixture("enc");
    add_aacs_marker(&root);
    encrypt_stream(&root);
    let output =
        bdinfo_rs().args([root.as_os_str(), "-w".as_ref()]).output().expect("spawn bdinfo-rs");
    let wrote_report = root.join("BDINFO.BigBuckBunny.txt").exists();
    let _ = std::fs::remove_dir_all(&parent).is_ok();
    assert_eq!(output.status.code(), Some(4), "the refusal has its own exit code");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("AACS-encrypted"), "the notice names the cause: {stderr}");
    assert!(stderr.contains("--list"), "the notice points at --list: {stderr}");
    assert!(!wrote_report, "a refused scan writes no report file");
}

#[test]
fn list_on_an_encrypted_disc_prints_the_table_with_the_notice() {
    let (root, parent) = copy_fixture("enclist");
    add_aacs_marker(&root);
    encrypt_stream(&root);
    let output =
        bdinfo_rs().args([root.as_os_str(), "--list".as_ref()]).output().expect("spawn bdinfo-rs");
    let _ = std::fs::remove_dir_all(&parent).is_ok();
    assert_eq!(output.status.code(), Some(0), "--list keeps its own exit rules");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("1   1      00000.MPLS"), "the table still lists: {stdout}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("AACS-encrypted"), "the notice still prints: {stderr}");
}

// --- the multi-playlist disc: three playlists over three distinct clips ---
//
// `BigBuckBunny` is a one-playlist, one-clip disc, so nothing about it can
// distinguish a selection from the whole disc — `--list`, `-m 00000` and
// `--whole` all name the same lone row. `MultiPlaylist` is the disc where the
// listing, the selection and the per-playlist accounting can disagree: three
// playlists, three clip stems, one playlist spanning two of them. It is
// deliberately NOT pinned to a golden — the report's byte contract stays
// BigBuckBunny's one golden per switch — so this checks the flow, not the format.

#[test]
fn the_multi_playlist_disc_lists_three_playlists_and_scans_them_clean() {
    let disc = real_fixture("MultiPlaylist");

    let list =
        bdinfo_rs().args([disc.as_os_str(), "--list".as_ref()]).output().expect("spawn bdinfo-rs");
    assert!(list.status.success(), "list failed: {}", String::from_utf8_lossy(&list.stderr));
    let stdout = String::from_utf8_lossy(&list.stdout);
    // Longest playlist first, and `00001.MPLS` in a group of its own because it
    // is the one playlist sharing no clip with the others.
    for row in [
        "1   1      00002.MPLS     00:00:50",
        "2   1      00000.MPLS     00:00:30",
        "3   2      00001.MPLS     00:00:25",
    ] {
        assert!(stdout.contains(row), "the table is missing {row:?}: {stdout}");
    }
    assert!(!stdout.contains("\n4   "), "the table lists exactly three playlists: {stdout}");

    let dest = std::env::temp_dir().join(format!("bdinfo-rs-multi-{}", std::process::id()));
    std::fs::create_dir_all(&dest).expect("create dest");
    let output = bdinfo_rs()
        .args([disc.as_os_str(), dest.as_os_str(), "-w".as_ref()])
        .output()
        .expect("spawn bdinfo-rs");
    let report = std::fs::read(dest.join("BDINFO.MultiPlaylist.txt")).expect("read the report");
    let _ = std::fs::remove_dir_all(&dest).is_ok();
    // Exit 0, not the resilient scan's 3: every file on this disc reads clean.
    assert_eq!(output.status.code(), Some(0), "scan: {}", String::from_utf8_lossy(&output.stderr));

    let text = String::from_utf8(report).expect("the report is valid UTF-8");
    for section in ["PLAYLIST: 00000.MPLS", "PLAYLIST: 00001.MPLS", "PLAYLIST: 00002.MPLS"] {
        assert!(text.contains(section), "the report is missing {section:?}: {text}");
    }
    for clip in ["00011.M2TS", "00022.M2TS", "00033.M2TS"] {
        assert!(text.contains(clip), "the report is missing clip {clip:?}: {text}");
    }
    assert!(!text.contains("WARNING"), "an intact disc reports no warnings: {text}");

    // The stream files carry real transport packets, so the video row's rate is
    // measured rather than the flat zero a zero-filled `*.m2ts` would leave. The
    // value itself is not pinned — only that the measurement happened.
    let video = text
        .lines()
        .find(|line| line.starts_with("MPEG-4 AVC Video"))
        .expect("the report names the video codec");
    let rate: u32 = video
        .split_whitespace()
        .nth(3)
        .and_then(|kbps| kbps.replace(',', "").parse().ok())
        .expect("the video row carries a bitrate");
    assert!(rate > 0, "the scan measured a video bitrate: {video}");
}

#[test]
fn a_real_list_shows_the_filtered_table_without_writing_a_report() {
    let disc = real_fixture("BigBuckBunny");
    let output =
        bdinfo_rs().args([disc.as_os_str(), "--list".as_ref()]).output().expect("spawn bdinfo-rs");
    assert!(output.status.success(), "scan failed: {}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("#   Group  Playlist File  Length    Estimated Bytes Measured Bytes"),
        "table header: {stdout}"
    );
    assert!(stdout.contains("1   1      00000.MPLS     00:00:30"), "the 30 s row: {stdout}");
    assert!(
        !disc.join("BDINFO.BigBuckBunny.txt").exists(),
        "--list writes no report file into the disc folder"
    );
}
