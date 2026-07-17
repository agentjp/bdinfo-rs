//! Embeds the application icon into the Windows executable.
//!
//! Renders the aperture mark at every size in [`winres::SIZES`] with the same
//! procedural renderer the runtime window icon uses (`src/icon.rs`, shared via
//! `#[path]` — a build script cannot depend on the library it builds), packs
//! the set into a `.res` icon container (`src/winres.rs`), and passes the file
//! to the MSVC linker, which lays it into the executable's `.rsrc` section —
//! what Explorer, pinned taskbar shortcuts, and Alt-Tab display. Pure Rust end
//! to end: no resource compiler, no image encoder, no new dependency, no C.
//!
//! On every non-`windows-msvc` target this is a no-op (the shipped Windows
//! targets are both MSVC; ELF and Mach-O binaries have no resource section —
//! their icons arrive with packaging: `.desktop` + hicolor on Linux, the
//! `.icns` inside a macOS `.app` bundle).
#![forbid(unsafe_code)]

#[path = "src/icon.rs"]
mod icon;
#[path = "src/winres.rs"]
mod winres;

fn main() {
    // Cargo tracks build.rs itself; the two shared sources need declaring.
    println!("cargo:rerun-if-changed=src/icon.rs");
    println!("cargo:rerun-if-changed=src/winres.rs");

    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if os != "windows" || env != "msvc" {
        return;
    }

    let icons: Vec<(u32, Vec<u8>)> = winres::SIZES
        .iter()
        .map(|&size| (size, winres::dib(size, &icon::rgba(size)).expect("shipped icon size")))
        .collect();
    let res = winres::res(&icons).expect("the shipped icon set is valid");

    let out_dir = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let path = std::path::Path::new(&out_dir).join("bdinfo-rs-gui.res");
    std::fs::write(&path, res).expect("write the icon resource");
    // link.exe takes a .res as a plain input and emits the .rsrc section.
    println!("cargo:rustc-link-arg-bins={}", path.display());
}
