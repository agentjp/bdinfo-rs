//! `bdinfo-rs-core` — a memory-safe Blu-ray disc analyzer library.
//!
//! This is the library the `bdinfo-rs` CLI drives: it reads a Blu-ray disc
//! structure (a BDMV folder tree or a UDF `.iso` image), parses the index,
//! playlist, and clip-information files, demuxes the transport streams, and
//! reports every elementary stream's codec details.
//!
//! Design rules:
//! - `#![forbid(unsafe_code)]` — memory safety is the point.
//! - Every read is bounds-checked and fallible; we never index raw or panic on input-derived bytes
//!   (the `indexing_slicing` / `unwrap_used` gate enforces this).
//! - All multi-byte integers are big-endian and host-independent.
#![forbid(unsafe_code)]
// The library never writes to stdout/stderr — emitting output is the caller's
// job. Workspace lints can't target a single crate, so this is crate-scoped on
// top of `[lints] workspace = true`; the `bdinfo-rs` CLI keeps `println!`.
#![deny(clippy::print_stdout, clippy::print_stderr)]

pub mod bdrom;
pub mod bitstream;
pub mod bytes;
pub mod codec;
pub mod discovery;
pub mod error;
pub mod index;
pub mod language_codes;
pub mod primitives;
pub mod report;
pub mod stream;
pub mod vfs;

/// Returns the version of the `bdinfo-rs-core` library crate (its `CARGO_PKG_VERSION`).
#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::version;

    #[test]
    fn version_matches_cargo_pkg_version() {
        assert_eq!(version(), env!("CARGO_PKG_VERSION"));
    }
}
