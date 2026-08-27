//! Benchmark: HTML to Markdown, and the two AI-sanitisation passes.
//!
//! This is the conversion a RAG pipeline runs over every page it ingests, so
//! the interesting question is not "how fast is the converter" on its own but
//! how the cost divides between the three things `Response::markdown()`
//! actually does: build a tree, rebuild it twice for sanitisation, and walk it
//! emitting Markdown. The four groups below take those apart.
//!
//! * `html_to_markdown` — the string-in/string-out entry point, which parses
//!   internally. Swept across three document sizes.
//! * `convert` — [`to_markdown_default`] over an already-parsed tree, i.e. the
//!   same work with the parse subtracted.
//! * `sanitize` — the two passes on their own. Each *rebuilds* the tree rather
//!   than editing it in place, which is why they are not free.
//! * `document` — [`Document::markdown`], the whole pipeline, with and without
//!   a narrowing selector.
//!
//! [`to_markdown_default`]: browser::markdown::to_markdown_default
//! [`Document::markdown`]: browser::parser::document::Document::markdown
//!
//! ```text
//! cargo bench -p browser-rs --bench markdown --features markdown
//! ```

use std::hint::black_box;

use browser::markdown::{sanitize_for_ai, strip_noise_tags, to_markdown_default};
use browser::Document;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

/// A documentation page: `sections` of prose, each with a heading, a list, a
/// table row block, inline emphasis and links — plus the noise a real page
/// carries, because the sanitiser's cost is a function of how much there is to
/// remove.
fn article(sections: usize) -> String {
    let mut html = String::with_capacity(sections * 640 + 512);
    html.push_str(
        "<!doctype html><html><head><title>Guide</title>\
         <style>.a{color:red}</style></head><body>\
         <nav><a href=\"/\">home</a> <a href=\"/docs\">docs</a></nav>\
         <article><h1>The guide</h1>",
    );
    for index in 0..sections {
        html.push_str(&format!(
            "<h2>Section {index}</h2>\
             <p>Some <strong>emphasised</strong> prose with a <a href=\"/p/{index}\">link</a> \
             and a little <code>inline_code()</code> in it, long enough that the wrapper has \
             something to measure.</p>\
             <ul><li>first point</li><li>second point</li><li>third point</li></ul>\
             <blockquote>A quoted remark about section {index}.</blockquote>\
             <table><tr><th>Key</th><th>Value</th></tr>\
             <tr><td>alpha</td><td>{index}</td></tr>\
             <tr><td>beta</td><td>{doubled}</td></tr></table>\
             <pre><code>let x = {index};</code></pre>\
             <p style=\"display:none\">hidden note {index}</p>\
             <script>track({index})</script>",
            doubled = index * 2,
        ));
    }
    html.push_str("</article><footer>&copy; nobody</footer></body></html>");
    html
}

/// The string-in/string-out entry point, across three sizes.
///
/// Throughput is in bytes of *input* HTML, which is the number a pipeline
/// budgets against — it knows how big the page it fetched was, not how many
/// nodes it turned into.
fn html_to_markdown(c: &mut Criterion) {
    let mut group = c.benchmark_group("html_to_markdown");
    for sections in [10_usize, 100, 500] {
        let html = article(sections);
        group.throughput(Throughput::Bytes(html.len() as u64));
        group.bench_with_input(BenchmarkId::from_parameter(sections), &html, |b, html| {
            b.iter(|| browser::markdown::html_to_markdown(black_box(html)));
        });
    }
    group.finish();
}

/// The conversion walk with the parse subtracted.
///
/// The gap between this and `html_to_markdown/100` is what `html5ever` costs on
/// the same document.
fn convert(c: &mut Criterion) {
    let html = article(100);
    let tree = browser::dom::Document::parse(&html);

    let mut group = c.benchmark_group("convert");
    group.throughput(Throughput::Bytes(html.len() as u64));
    group.bench_function("to_markdown_default", |b| {
        b.iter(|| to_markdown_default(black_box(&tree).root()));
    });
    group.finish();
}

/// The two sanitisation passes, separately and together.
///
/// `strip_noise_tags` drops `<script>`, `<style>` and friends; `sanitize_for_ai`
/// removes hidden and prompt-injection content and the zero-width characters.
/// Both build a new tree, so they are measured against a freshly parsed one
/// each time rather than being chained in the loop.
fn sanitize(c: &mut Criterion) {
    let html = article(100);
    let tree = browser::dom::Document::parse(&html);
    let stripped = strip_noise_tags(&tree);

    let mut group = c.benchmark_group("sanitize");
    group.throughput(Throughput::Bytes(html.len() as u64));
    group.bench_function("strip_noise_tags", |b| {
        b.iter(|| strip_noise_tags(black_box(&tree)));
    });
    group.bench_function("sanitize_for_ai", |b| {
        b.iter(|| sanitize_for_ai(black_box(&stripped)));
    });
    // What `Document::markdown` runs internally: both passes, in order.
    group.bench_function("clean_for_ai", |b| {
        b.iter(|| browser::markdown::clean_for_ai(black_box(&tree)));
    });
    group.finish();
}

/// The whole pipeline as a caller sees it.
fn document(c: &mut Criterion) {
    let html = article(100);
    let doc = Document::parse(&html);

    let mut group = c.benchmark_group("document");
    group.throughput(Throughput::Bytes(html.len() as u64));
    group.bench_function("markdown", |b| {
        b.iter(|| black_box(&doc).markdown());
    });
    // Narrowing to `<article>` skips the nav and the footer. It is *not* the
    // cheaper option: the selector runs over the sanitised tree, on top of
    // everything the unnarrowed call already did.
    group.bench_function("markdown_with_selector", |b| {
        b.iter(|| {
            black_box(&doc)
                .markdown_with(Some("article"), true)
                .expect("valid selector")
        });
    });
    // Parse included, which is what a `Response` pays on first access.
    group.bench_function("parse_then_markdown", |b| {
        b.iter(|| Document::parse(black_box(&html)).markdown());
    });
    group.finish();
}

criterion_group!(benches, html_to_markdown, convert, sanitize, document);
criterion_main!(benches);
