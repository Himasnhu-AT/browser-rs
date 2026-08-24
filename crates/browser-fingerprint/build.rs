//! Build script for `browser-fingerprint`.
//!
//! Work unit 9 compiles `assets/ad_domains.txt` (~3 500 lines) into a
//! `phf::Set` and validates `assets/header_profiles.json`, writing both
//! products to `$OUT_DIR`. The lists ship as assets, never as string
//! literals in a `.rs` file.

use std::env;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::Path;

fn main() {
    println!("cargo::rerun-if-changed=assets/ad_domains.txt");
    println!("cargo::rerun-if-changed=assets/header_profiles.json");
    println!("cargo::rerun-if-changed=build.rs");

    let out_dir = env::var_os("OUT_DIR").expect("OUT_DIR is always set for build scripts");
    let out_dir = Path::new(&out_dir);

    build_ad_domains(out_dir);
    validate_header_profiles(out_dir);
}

/// Turn the newline-separated ad-domain asset into a compile-time `phf::Set`.
///
/// Lines starting with `#` are provenance comments; blank lines are ignored.
/// Duplicate entries are dropped, because `phf_codegen` rejects them.
fn build_ad_domains(out_dir: &Path) {
    let raw = fs::read_to_string("assets/ad_domains.txt")
        .expect("assets/ad_domains.txt must exist and be UTF-8");

    let mut seen = std::collections::BTreeSet::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        seen.insert(line.to_owned());
    }
    assert!(
        seen.len() > 3_000,
        "assets/ad_domains.txt looks truncated: {} entries",
        seen.len()
    );

    let mut set = phf_codegen::Set::new();
    for domain in &seen {
        set.entry(domain.as_str());
    }

    let path = out_dir.join("ad_domains.rs");
    let file = fs::File::create(&path).expect("failed to create $OUT_DIR/ad_domains.rs");
    let mut file = BufWriter::new(file);
    writeln!(
        &mut file,
        "/// Bare ad/tracker domains, generated from `assets/ad_domains.txt` by `build.rs`.\n\
         pub static AD_DOMAINS: ::phf::Set<&'static str> = {};",
        set.build()
    )
    .expect("failed to write $OUT_DIR/ad_domains.rs");
    writeln!(
        &mut file,
        "/// Number of entries in [`AD_DOMAINS`].\n\
         pub const AD_DOMAIN_COUNT: usize = {};",
        seen.len()
    )
    .expect("failed to write $OUT_DIR/ad_domains.rs");
}

/// Parse `assets/header_profiles.json` so a malformed asset fails the build
/// rather than the first request, and export its profile count as a constant.
///
/// The profile table itself is deserialised lazily at runtime (see
/// `headers.rs`): 800-odd ordered header sets as generated Rust source would
/// cost far more compile time than one `serde_json` parse costs at startup,
/// and the parse only happens on the `impersonate = None` code path.
fn validate_header_profiles(out_dir: &Path) {
    let raw = fs::read_to_string("assets/header_profiles.json")
        .expect("assets/header_profiles.json must exist and be UTF-8");
    let doc: serde_json::Value =
        serde_json::from_str(&raw).expect("assets/header_profiles.json must be valid JSON");

    let profiles = doc
        .get("profiles")
        .and_then(serde_json::Value::as_array)
        .expect("assets/header_profiles.json must have a `profiles` array");
    assert!(
        !profiles.is_empty(),
        "assets/header_profiles.json contains no profiles"
    );

    for (index, profile) in profiles.iter().enumerate() {
        for key in ["family", "os", "device", "version"] {
            assert!(
                profile
                    .get(key)
                    .and_then(serde_json::Value::as_str)
                    .is_some(),
                "profile #{index} is missing the string field `{key}`"
            );
        }
        assert!(
            profile
                .get("major")
                .and_then(serde_json::Value::as_u64)
                .is_some(),
            "profile #{index} is missing the integer field `major`"
        );
        let headers = profile
            .get("headers")
            .and_then(serde_json::Value::as_array)
            .unwrap_or_else(|| panic!("profile #{index} is missing the `headers` array"));
        assert!(
            !headers.is_empty(),
            "profile #{index} has an empty `headers` array"
        );
        for pair in headers {
            let pair = pair
                .as_array()
                .unwrap_or_else(|| panic!("profile #{index} has a non-array header entry"));
            assert!(
                pair.len() == 2 && pair.iter().all(|v| v.is_string()),
                "profile #{index} has a header entry that is not a [name, value] string pair"
            );
        }
    }

    let path = out_dir.join("header_profiles.rs");
    fs::write(
        &path,
        format!(
            "/// Number of precomputed header profiles in `assets/header_profiles.json`.\n\
             pub const HEADER_PROFILE_COUNT: usize = {};\n",
            profiles.len()
        ),
    )
    .expect("failed to write $OUT_DIR/header_profiles.rs");
}
