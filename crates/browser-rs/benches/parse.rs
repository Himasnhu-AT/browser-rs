//! Benchmark: parsing a document and querying it.
//!
//! `ARCHITECTURE.md` §7 pins one number for this crate: **parse 5000 nested
//! `div.item`, then `css(".item")`, in under 0.5 s**. That budget is the
//! `budget/parse_then_css_5000` benchmark at the bottom of this file, and it is
//! deliberately written as one benchmark rather than as the sum of two — the
//! spec measures the round trip a caller actually performs, and a caller who
//! parses and then queries pays for the tree *once*.
//!
//! The other two groups are diagnostic. `parse` sweeps the document size so a
//! regression shows up as a change of slope rather than as one number moving,
//! and `query` runs the three query engines over the *same* parsed tree, because
//! CSS, XPath and `find_all` share node identity and only differ in matcher.
//!
//! ```text
//! cargo bench -p browser-rs --bench parse
//! ```

use std::hint::black_box;
use std::time::Duration;

use browser::{CssOpts, Document, FindQuery};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

/// The document under test: `items` product cards inside a six-deep wrapper.
///
/// "Nested" is the load-bearing word in the spec, and it is what makes this a
/// realistic input rather than a flat list — every `.item` sits under five
/// ancestors and carries three element children of its own, so a selector has
/// real ancestor walks to do and `get_all_text` has real recursion to do. The
/// wrapper depth is fixed and the card count is what varies; a 5000-*deep*
/// tree would be measuring recursion limits, not parsing.
fn catalogue(items: usize) -> String {
    // ~128 bytes per card is close enough to avoid a dozen reallocations.
    let mut html = String::with_capacity(items * 128 + 256);
    html.push_str(
        "<!doctype html><html><head><title>Catalogue</title></head><body>\
         <div id=\"page\"><main><section class=\"grid\"><div class=\"col\"><ul class=\"list\">",
    );
    for index in 0..items {
        html.push_str(&format!(
            "<li><div class=\"item\" data-sku=\"sku-{index}\">\
             <h3 class=\"name\">Item {index}</h3>\
             <span class=\"price\">${dollars}.{cents:02}</span>\
             <a class=\"link\" href=\"/p/{index}\">details</a>\
             </div></li>",
            dollars = index % 100,
            cents = index % 100,
        ));
    }
    html.push_str("</ul></div></section></main></div></body></html>");
    html
}

/// Parsing on its own, across three sizes.
fn parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse");
    for items in [1_000_usize, 5_000, 20_000] {
        let html = catalogue(items);
        // Every card is five elements — `li > div > (h3, span, a)` — so this
        // reports time per DOM element and the three sizes become directly
        // comparable.
        group.throughput(Throughput::Elements(items as u64 * 5));
        group.bench_with_input(BenchmarkId::from_parameter(items), &html, |b, html| {
            b.iter(|| Document::parse(black_box(html)));
        });
    }
    group.finish();
}

/// The query paths, over one already-parsed 5000-card tree.
///
/// Three engines — CSS, XPath and `find_all` — asked the same question five
/// ways, so the numbers are directly comparable.
fn query(c: &mut Criterion) {
    let doc = Document::parse(&catalogue(5_000));

    let mut group = c.benchmark_group("query");
    group.throughput(Throughput::Elements(5_000));

    group.bench_function("css_class", |b| {
        b.iter(|| black_box(&doc).css(".item").expect("valid selector").len());
    });
    group.bench_function("css_with_opts", |b| {
        let opts = CssOpts::default();
        b.iter(|| {
            black_box(&doc)
                .css_with(".item", &opts)
                .expect("valid selector")
                .len()
        });
    });
    // `::text` walks into every match, so this is the selector cost *plus* the
    // text extraction the previous two skip.
    group.bench_function("css_text_pseudo", |b| {
        b.iter(|| {
            black_box(&doc)
                .css(".item .price::text")
                .expect("valid selector")
                .texts()
                .len()
        });
    });
    group.bench_function("xpath_predicate", |b| {
        b.iter(|| {
            black_box(&doc)
                .xpath("//div[@class='item']")
                .expect("valid expression")
                .len()
        });
    });
    group.bench_function("find_all", |b| {
        b.iter(|| {
            black_box(&doc)
                .find_all(&FindQuery::new().tag("div").attr("class", "item"))
                .expect("valid query")
                .len()
        });
    });

    group.finish();
}

/// The number `ARCHITECTURE.md` §7 pins: parse 5000 `div.item`, then select
/// them.
///
/// The assertion inside the loop is not measurement noise worth removing — it
/// is what stops the benchmark silently becoming "parse 5000 cards and select
/// nothing" if a selector regresses.
fn budget(c: &mut Criterion) {
    let html = catalogue(5_000);

    let mut group = c.benchmark_group("budget");
    // The whole point is a wall-clock comparison against 0.5 s, so give it long
    // enough to be a measurement rather than a guess.
    group.measurement_time(Duration::from_secs(10));
    group.throughput(Throughput::Elements(5_000));
    group.bench_function("parse_then_css_5000", |b| {
        b.iter(|| {
            let doc = Document::parse(black_box(&html));
            let items = doc.css(".item").expect("valid selector");
            assert_eq!(items.len(), 5_000);
            items.len()
        });
    });
    group.finish();
}

criterion_group!(benches, parse, query, budget);
criterion_main!(benches);
