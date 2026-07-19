//! Writes the generated assets into `crates/bdinfo-rs-gui/packaging/` — the
//! regeneration entry point (`cargo run -p bdinfo-rs-gui-assets-gen`); the tie
//! test (`tests/tie.rs`) holds the committed copies to these exact bytes.
#![forbid(unsafe_code)]

use std::fs;
use std::path::Path;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("packaging");
    for (rel, bytes) in bdinfo_rs_gui_assets_gen::assets() {
        let path = root.join(&rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create the asset directory");
        }
        fs::write(&path, bytes).expect("write the asset");
        println!("wrote {}", path.display());
    }
}
