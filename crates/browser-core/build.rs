//! Build script for `browser-core`.
//!
//! Work unit 2 turns `assets/html4_entities.txt` into a `phf::Map` of HTML4
//! named entities, written to `$OUT_DIR/html4_entities.rs`.

fn main() {
    println!("cargo::rerun-if-changed=assets/html4_entities.txt");
    println!("cargo::rerun-if-changed=build.rs");
}
