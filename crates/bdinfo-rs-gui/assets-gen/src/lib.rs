//! Derives every packaging icon asset from the single procedural mark.
//!
//! The app's icon exists once, as code (`bdinfo_rs_gui::icon` renders it to
//! RGBA, byte-identically on every shipped arch); the packaging formats each
//! want their own container. This library builds them all in memory:
//!
//! - `bdinfo-rs-gui.ico` — the WiX installer's Apps-list icon ([`bdinfo_rs_gui::winres::ico`] over
//!   the same DIB ladder the executable embeds),
//! - `bdinfo-rs-gui.icns` — the macOS `.app` bundle icon,
//! - `icons/hicolor/<S>x<S>/apps/bdinfo-rs-gui.png` — the freedesktop hicolor set for the Linux
//!   packages (the AppImage root icon is the 256 px entry, copied at packaging time).
//!
//! The outputs are COMMITTED under `crates/bdinfo-rs-gui/packaging/` and tied
//! back by `tests/tie.rs`, which regenerates and byte-compares them — so the
//! release lane packages known bytes and a drifted asset fails the gate
//! instead of shipping. Regenerate with `cargo run -p bdinfo-rs-gui-assets-gen`
//! after changing the mark.
#![forbid(unsafe_code)]

use bdinfo_rs_gui::{icon, winres};

/// The freedesktop hicolor ladder the Linux packages install.
const HICOLOR_SIZES: [u32; 8] = [16, 24, 32, 48, 64, 128, 256, 512];

/// The `.icns` ladder: every size the format's PNG-payload icon types cover
/// (`icp4`/`icp5`, `ic07`–`ic10` plus the `@2x` aliases the encoder picks).
const ICNS_SIZES: [u32; 7] = [16, 32, 64, 128, 256, 512, 1024];

/// Encodes an RGBA buffer as a PNG (8-bit RGBA, the encoder's deterministic
/// default filter/compression — the tie test holds the exact bytes).
fn encode_png(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, size, size);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(rgba).expect("png image data");
    writer.finish().expect("png finish");
    out
}

/// Every packaging asset as `(path relative to packaging/, bytes)`, in a
/// stable order (the `.ico`, the `.icns`, then the hicolor ladder ascending).
#[must_use]
pub fn assets() -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();

    let dibs: Vec<(u32, Vec<u8>)> = winres::SIZES
        .iter()
        .map(|&s| (s, winres::dib(s, &icon::rgba(s)).expect("shipped icon size")))
        .collect();
    let ico = winres::ico(&dibs).expect("the shipped icon set is valid");
    out.push(("bdinfo-rs-gui.ico".to_owned(), ico));

    let mut family = icns::IconFamily::new();
    for size in ICNS_SIZES {
        let image = icns::Image::from_data(icns::PixelFormat::RGBA, size, size, icon::rgba(size))
            .expect("RGBA buffer of the stated size");
        family.add_icon(&image).expect("a size the format covers");
    }
    let mut icns_bytes = Vec::new();
    family.write(&mut icns_bytes).expect("write to memory");
    out.push(("bdinfo-rs-gui.icns".to_owned(), icns_bytes));

    for size in HICOLOR_SIZES {
        let path = format!("icons/hicolor/{size}x{size}/apps/bdinfo-rs-gui.png");
        out.push((path, encode_png(size, &icon::rgba(size))));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::assets;

    #[test]
    fn the_asset_list_is_complete_and_stably_ordered() {
        let names: Vec<String> = assets().into_iter().map(|(path, _)| path).collect();
        let expected = [
            "bdinfo-rs-gui.ico",
            "bdinfo-rs-gui.icns",
            "icons/hicolor/16x16/apps/bdinfo-rs-gui.png",
            "icons/hicolor/24x24/apps/bdinfo-rs-gui.png",
            "icons/hicolor/32x32/apps/bdinfo-rs-gui.png",
            "icons/hicolor/48x48/apps/bdinfo-rs-gui.png",
            "icons/hicolor/64x64/apps/bdinfo-rs-gui.png",
            "icons/hicolor/128x128/apps/bdinfo-rs-gui.png",
            "icons/hicolor/256x256/apps/bdinfo-rs-gui.png",
            "icons/hicolor/512x512/apps/bdinfo-rs-gui.png",
        ];
        assert_eq!(names, expected);
    }

    #[test]
    fn every_asset_leads_with_its_format_magic() {
        for (path, bytes) in assets() {
            let magic: &[u8] = if path.ends_with(".ico") {
                &[0, 0, 1, 0]
            } else if path.ends_with(".icns") {
                b"icns"
            } else {
                b"\x89PNG\r\n\x1a\n"
            };
            assert_eq!(bytes.get(..magic.len()), Some(magic), "{path}");
        }
    }
}
