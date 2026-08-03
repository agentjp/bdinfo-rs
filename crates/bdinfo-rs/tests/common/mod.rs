//! Launcher and fixture plumbing shared by the `bdinfo-rs` integration tests.
//!
//! `tests/cli.rs` proves each switch on its own and `tests/matrix.rs` sweeps
//! them in combination; both drive the same built binary against the same
//! committed fixtures, so the two entry points live here rather than once per
//! test binary. A `tests/common/` directory is not itself a test target, so
//! nothing here is compiled as a test.
//!
//! Both test crates declare this as `pub mod common;` and both items below are
//! `pub` + `#[must_use]`. That is the one spelling three enforced lints all
//! accept: `unreachable_pub` rejects `pub` inside a private module,
//! `redundant_pub_crate` rejects `pub(crate)` there, and `must_use_candidate`
//! then applies because the items have become crate-root-reachable.

use std::path::PathBuf;
use std::process::Command;

/// A `Command` for the binary under test (path injected by cargo).
#[must_use]
pub fn bdinfo_rs() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bdinfo-rs"))
}

/// Absolute path to a committed real-disc fixture under `tests/fixtures/`.
#[must_use]
pub fn real_fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests").join("fixtures").join(name)
}
