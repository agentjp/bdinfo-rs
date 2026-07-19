//! The regeneration tie: the committed packaging assets must equal what
//! `assets()` builds from the current mark, byte for byte, so an edit to the
//! renderer (or an encoder bump changing output bytes) fails here instead of
//! shipping a stale or irreproducible icon. On a mismatch, regenerate with
//! `cargo run -p bdinfo-rs-gui-assets-gen` and commit the result.

use std::fs;
use std::path::Path;

#[test]
fn committed_assets_match_regeneration_byte_for_byte() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("packaging");
    for (rel, bytes) in bdinfo_rs_gui_assets_gen::assets() {
        let committed = fs::read(root.join(&rel)).ok();
        assert!(
            committed.is_some(),
            "{rel} is missing — regenerate via `cargo run -p bdinfo-rs-gui-assets-gen` and commit"
        );
        assert!(
            committed == Some(bytes),
            "{rel} drifted from the generator — \
             regenerate via `cargo run -p bdinfo-rs-gui-assets-gen` and commit"
        );
    }
}
