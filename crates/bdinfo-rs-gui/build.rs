//! Embeds the application icon and version info into the Windows executable.
//!
//! Renders the aperture mark at every size in [`winres::SIZES`] with the same
//! procedural renderer the runtime window icon uses (`src/icon.rs`, shared via
//! `#[path]` — a build script cannot depend on the library it builds), packs
//! the set into a `.res` icon container (`src/winres.rs`), appends a
//! `VS_VERSIONINFO` resource carrying the crate version straight from
//! `CARGO_PKG_VERSION` (so the Details tab can never drift from Cargo.toml),
//! and passes the file to the MSVC linker, which lays it into the executable's
//! `.rsrc` section — what Explorer, pinned taskbar shortcuts, Alt-Tab, and the
//! file-Properties Details tab display. Pure Rust end to end: no resource
//! compiler, no image encoder, no new dependency, no C.
//!
//! On every non-`windows-msvc` target this is a no-op (the shipped Windows
//! targets are both MSVC; ELF and Mach-O binaries have no resource section —
//! their icons arrive with packaging: `.desktop` + hicolor on Linux, the
//! `.icns` inside a macOS `.app` bundle).
#![forbid(unsafe_code)]

#[path = "src/icon.rs"]
mod icon;
#[path = "src/winres.rs"]
// The lib consumes the module's full surface; this compilation uses only the
// .res half (ico() is the packaging-asset form, assets-gen's job).
#[expect(dead_code, reason = "shared source; the .ico packer is unused here")]
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
    let mut res = winres::res(&icons).expect("the shipped icon set is valid");

    // FILEVERSION quad: major.minor.patch from the crate version plus a zero
    // fourth part (the crate never carries one).
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets CARGO_PKG_VERSION");
    let mut parts = version.split('.').map(|p| p.parse::<u16>().expect("numeric version part"));
    let quad = [
        parts.next().expect("major"),
        parts.next().expect("minor"),
        parts.next().expect("patch"),
        0,
    ];
    let info = winres::version_info(
        quad,
        &[
            ("CompanyName", "agentjp"),
            ("FileDescription", "bdinfo-rs GUI"),
            ("FileVersion", &version),
            ("InternalName", "bdinfo-rs-gui"),
            ("LegalCopyright", "bdinfo-rs contributors, LGPL-2.1-or-later"),
            ("OriginalFilename", "bdinfo-rs-gui.exe"),
            ("ProductName", "bdinfo-rs GUI"),
            ("ProductVersion", &version),
        ],
    )
    .expect("the shipped version strings are well-formed");
    res.extend_from_slice(&winres::entry(winres::RT_VERSION, 1, &info));

    let out_dir = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let path = std::path::Path::new(&out_dir).join("bdinfo-rs-gui.res");
    std::fs::write(&path, res).expect("write the resource file");
    // link.exe takes a .res as a plain input and emits the .rsrc section.
    println!("cargo:rustc-link-arg-bins={}", path.display());
}
