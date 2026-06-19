//! Build script: generate the shell completions + man page for the `bdinfo-rs`
//! CLI, derived from the same `clap::Command` the binary parses.
//!
//! WHERE THE ARTIFACTS LAND (the contract the release packaging — prompt 04 —
//! relies on; do not change without updating it):
//!
//! Every generated file is written into a single `assets/` directory rooted at
//! this build script's `OUT_DIR`:
//!
//! ```text
//! $OUT_DIR/assets/bdinfo-rs.bash      bash completion
//! $OUT_DIR/assets/_bdinfo-rs          zsh completion   (clap's zsh name)
//! $OUT_DIR/assets/bdinfo-rs.fish      fish completion
//! $OUT_DIR/assets/_bdinfo-rs.ps1      PowerShell completion
//! $OUT_DIR/assets/bdinfo-rs.1         roff man page (section 1)
//! ```
//!
//! `OUT_DIR` is chosen by Cargo and carries a build hash, e.g.
//! `target/<profile>/build/bdinfo-rs-<hash>/out`. Release packaging locates the
//! directory deterministically by globbing
//! `target/<profile>/build/bdinfo-rs-*/out/assets/` (this crate has exactly one
//! such build directory per profile). Writing under `OUT_DIR` — never into the
//! source tree — keeps the build reproducible and the working tree clean.
//!
//! The CLI definition is shared with `src/main.rs` via `include!`: a build
//! script cannot depend on its own crate, so we rebuild the same `clap::Command`
//! from the same `src/cli.rs` the binary parses. That guarantees the completions
//! and the man page can never drift from the shipped flags, help text, or
//! defaults.
#![forbid(unsafe_code)]

use std::fs;
use std::io::Error;
use std::path::PathBuf;

use clap::CommandFactory;
use clap_complete::{Shell, generate_to};

// Pull in the exact `Cli` the binary uses (brings `use clap::Parser;` with it).
include!("src/cli.rs");

fn main() -> Result<(), Error> {
    // Only regenerate when the CLI surface or this script changes.
    println!("cargo:rerun-if-changed=src/cli.rs");
    println!("cargo:rerun-if-changed=build.rs");

    let out_dir = std::env::var_os("OUT_DIR").ok_or_else(|| Error::other("OUT_DIR is not set"))?;
    let assets = PathBuf::from(out_dir).join("assets");
    fs::create_dir_all(&assets)?;

    let mut cmd = Cli::command();
    let bin = "bdinfo-rs";

    // Completion scripts for the four shells distro packages ship.
    for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell] {
        generate_to(shell, &mut cmd, bin, &assets)?;
    }

    // Man page (section 1, user commands), rendered from the same command.
    let mut page = Vec::new();
    clap_mangen::Man::new(cmd).render(&mut page)?;
    fs::write(assets.join("bdinfo-rs.1"), page)?;

    Ok(())
}
