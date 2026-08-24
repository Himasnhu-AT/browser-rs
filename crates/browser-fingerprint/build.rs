//! Build script for `browser-fingerprint`.
//!
//! Work unit 9 compiles `assets/ad_domains.txt` (~3 500 lines) into a
//! `phf::Set` and `assets/header_profiles.json` into a static profile table,
//! both written to `$OUT_DIR`. The lists ship as assets, never as string
//! literals in a `.rs` file.

fn main() {
    println!("cargo::rerun-if-changed=assets/ad_domains.txt");
    println!("cargo::rerun-if-changed=assets/header_profiles.json");
    println!("cargo::rerun-if-changed=build.rs");
}
