//! Benchmark: the adaptive round trip — save a fingerprint, then relocate the
//! element after the page has been redesigned.
//!
//! This is the feature that costs something at *query* time rather than at
//! parse time, and the cost has a shape worth knowing — one that is easy to
//! guess wrong twice over.
//!
//! [`Document::relocate`] scores the stored fingerprint against every candidate
//! element in the tree, so the first guess is that it is linear in document
//! size. It is not: an [`ElementFingerprint`] records the element's *sibling tag
//! list*, and `similarity_score` compares two of those with a `SequenceMatcher`
//! ratio, which is superlinear in their length. On a page whose cards are all
//! siblings, growing the page grows both the number of candidates and the length
//! of every comparison, and the two multiply.
//!
//! The second guess — that the curve is therefore smooth — is also wrong, and
//! that is what [`autojunk`] measures. CPython's `difflib` purges "popular"
//! elements from the sequence it indexes, but **only when that sequence has 200
//! or more entries** — `AUTOJUNK_THRESHOLD` in `browser-core`'s `similarity`
//! module, which reproduces the heuristic exactly. A page of
//! 201 identical cards therefore has `"div"` purged and matches almost
//! instantly, while a page of 200 does the full quadratic comparison. Measured
//! here, crossing that one-card boundary is a **33× step change** in the
//! opposite direction to the trend — 26.9 ms at 200 cards, 0.81 ms at 201. It is faithful behaviour, not a bug — the
//! reference implementation inherits it from `difflib` — but it means a
//! size sweep that straddles 200 cannot be read as a slope, and the `relocate`
//! group below deliberately stays above the boundary so that it can be.
//!
//! [`Document::save`] and [`Document::retrieve`] are separated out because they
//! are a different kind of cost entirely — one SQLite statement each, and the
//! only I/O anywhere in the parser. They are benchmarked against an in-memory
//! database, so what they report is the SQLite and serialisation cost with the
//! filesystem taken out; a real `SqliteStorage::open` on a disk file is slower
//! by whatever that machine's fsync costs.
//!
//! ```text
//! cargo bench -p browser-rs --bench relocate --features adaptive
//! ```

use std::hint::black_box;
use std::sync::Arc;

use browser::storage::{SqliteStorage, StorageSystem};
use browser::{Document, DocumentConfig, ElementFingerprint};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

/// The URL the fingerprints are stored under. `SqliteStorage` keys by domain,
/// so every document in this file has to agree on it.
const URL: &str = "https://shop.test/catalogue";

/// The page **before** the redesign: `items` cards, one of which — the one at
/// `target` — is what a caller would have matched and saved.
fn before(items: usize, target: usize) -> String {
    let mut html = String::with_capacity(items * 128 + 128);
    html.push_str("<html><body><main><div class=\"grid\">");
    for index in 0..items {
        let marker = if index == target {
            " id=\"target\""
        } else {
            ""
        };
        html.push_str(&format!(
            "<div class=\"card\"{marker} data-sku=\"sku-{index}\">\
             <h3>Item {index}</h3><span class=\"price\">${index}.00</span>\
             <a href=\"/p/{index}\">details</a></div>",
        ));
    }
    html.push_str("</div></main></body></html>");
    html
}

/// The page **after** the redesign: the `card` class is gone and the wrapper
/// around the cards has changed from `<main><div class="grid">` to
/// `<section class="products">`. No stored *selector* survives that — `.card`
/// matches nothing — while the structure, the text and the remaining attributes
/// do, and those are what the fingerprint is made of.
///
/// The redesign is deliberately of a realistic size rather than a maximal one.
/// [`browser_core::fingerprint::similarity_score`] weighs the element's own tag
/// name heavily, so a redesign that *also* renames `<div>` to `<article>` scores
/// 36% against this fingerprint and clears no threshold anyone would set. A
/// benchmark of a relocation that finds nothing measures the scan and not the
/// feature, so this one keeps the tag and scores 64% — comfortably above the
/// 40% the round trip is run at, and stable across all three document sizes.
fn after(items: usize, target: usize) -> String {
    let mut html = String::with_capacity(items * 128 + 128);
    html.push_str("<html><body><section class=\"products\">");
    for index in 0..items {
        let marker = if index == target {
            " id=\"target\""
        } else {
            ""
        };
        html.push_str(&format!(
            "<div{marker} data-sku=\"sku-{index}\">\
             <h3>Item {index}</h3><span class=\"price\">${index}.00</span>\
             <a href=\"/p/{index}\">details</a></div>",
        ));
    }
    html.push_str("</section></body></html>");
    html
}

/// An adaptive [`DocumentConfig`] over a fresh in-memory database.
fn adaptive_config() -> DocumentConfig {
    let storage: Arc<dyn StorageSystem> = Arc::new(
        SqliteStorage::in_memory(Some(URL)).expect("an in-memory database is always available"),
    );
    DocumentConfig {
        url: URL.to_owned(),
        adaptive: true,
        storage: Some(storage),
        ..DocumentConfig::new()
    }
}

/// Save one element's fingerprint under `identifier` and hand it back.
fn store_target(config: &DocumentConfig, items: usize, target: usize) -> ElementFingerprint {
    let doc = Document::parse_with(&before(items, target), config);
    let node = doc
        .css_first("#target")
        .expect("valid selector")
        .expect("the marked card is in the document");
    doc.save(&node, "product-card").expect("the save succeeds");
    doc.retrieve("product-card")
        .expect("the read succeeds")
        .expect("what was just saved is there")
}

/// Relocation across three document sizes.
///
/// All three are **above** the autojunk boundary described in the module docs,
/// so the three numbers are comparable with one another. See [`autojunk`] for
/// what happens below it.
fn relocate(c: &mut Criterion) {
    let mut group = c.benchmark_group("relocate");
    for items in [500_usize, 1_000, 5_000] {
        let target = items / 2;
        let config = adaptive_config();
        let fingerprint = store_target(&config, items, target);
        let redesigned = Document::parse_with(&after(items, target), &config);

        // The scan is over candidate elements, not over cards, and each card is
        // four elements.
        group.throughput(Throughput::Elements(items as u64 * 4));
        group.bench_with_input(BenchmarkId::from_parameter(items), &items, |b, _| {
            b.iter(|| {
                let found = black_box(&redesigned).relocate(black_box(&fingerprint), 40);
                // Not noise: without it the benchmark cannot tell "relocation
                // is fast" from "relocation stopped finding anything". It is
                // `!is_empty` rather than `== 1` because `relocate` returns
                // *every* candidate tied at the best score, and a page of
                // near-identical cards can legitimately tie.
                assert!(!found.is_empty());
                found.len()
            });
        });
    }
    group.finish();
}

/// The `difflib` autojunk cliff, pinned to the single card that causes it.
///
/// `siblings` excludes the element itself, so a page of `n` cards gives every
/// candidate an `n - 1`-entry sibling list. At 200 cards that is 199 — one short
/// of `AUTOJUNK_THRESHOLD` — and `SequenceMatcher` indexes all 199 identical
/// `"div"`s and does the full comparison. At 201 cards it is 200, `"div"` is
/// purged as popular, and the comparison collapses.
///
/// Two benchmarks one card apart, therefore, and the *larger* input is the fast
/// one. Anyone reading a relocation profile around a couple of hundred siblings
/// needs to know this before concluding anything about the trend.
fn autojunk(c: &mut Criterion) {
    let mut group = c.benchmark_group("relocate_autojunk");
    for items in [200_usize, 201] {
        let target = items / 2;
        let config = adaptive_config();
        let fingerprint = store_target(&config, items, target);
        let redesigned = Document::parse_with(&after(items, target), &config);

        group.throughput(Throughput::Elements(items as u64 * 4));
        group.bench_with_input(
            BenchmarkId::from_parameter(format!("{items}_cards")),
            &items,
            |b, _| {
                b.iter(|| {
                    let found = black_box(&redesigned).relocate(black_box(&fingerprint), 40);
                    assert!(!found.is_empty());
                    found.len()
                });
            },
        );
    }
    group.finish();
}

/// What the similarity threshold costs — and the answer is *nothing*, which is
/// the reason to measure it.
///
/// `relocate` scores every candidate, takes the best score, and returns the
/// nodes tied at it only if that best clears `percentage`. So the threshold
/// gates the **result**, never the work: 10 and 40 both return the one element
/// and 80 returns none (the redesign above scores 64%), and all three walk the
/// whole tree. A caller cannot buy speed by asking for less confidence.
fn threshold(c: &mut Criterion) {
    let items = 1_000;
    let target = items / 2;
    let config = adaptive_config();
    let fingerprint = store_target(&config, items, target);
    let redesigned = Document::parse_with(&after(items, target), &config);

    let mut group = c.benchmark_group("relocate_threshold");
    group.throughput(Throughput::Elements(items as u64 * 4));
    for percentage in [10_u8, 40, 80] {
        group.bench_with_input(
            BenchmarkId::from_parameter(percentage),
            &percentage,
            |b, &percentage| {
                b.iter(|| {
                    black_box(&redesigned)
                        .relocate(black_box(&fingerprint), percentage)
                        .len()
                });
            },
        );
    }
    group.finish();
}

/// The two storage statements on their own, against an in-memory database.
fn storage(c: &mut Criterion) {
    let items = 1_000;
    let target = items / 2;
    let config = adaptive_config();
    let doc = Document::parse_with(&before(items, target), &config);
    let node = doc
        .css_first("#target")
        .expect("valid selector")
        .expect("the marked card is in the document");

    let mut group = c.benchmark_group("adaptive_storage");
    group.bench_function("save", |b| {
        b.iter(|| {
            black_box(&doc)
                .save(black_box(&node), "product-card")
                .expect("the save succeeds");
        });
    });

    doc.save(&node, "product-card").expect("the save succeeds");
    group.bench_function("retrieve", |b| {
        b.iter(|| {
            black_box(&doc)
                .retrieve("product-card")
                .expect("the read succeeds")
                .expect("what was saved is there")
        });
    });
    group.finish();
}

criterion_group!(benches, relocate, autojunk, threshold, storage);
criterion_main!(benches);
