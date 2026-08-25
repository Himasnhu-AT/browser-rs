//! `testdata/u14_browser_golden.json`: what **real the reference implementation on real Chrome**
//! did over the fixture.
//!
//! The rows are the *expectations* the live tests replay. Ports were
//! normalised out at generation time (`:1234` became `:{PORT}`), so a row's
//! URLs are re-expanded against whatever port this run's fixture got.

use std::collections::HashMap;

use serde::Deserialize;

/// One scenario's observations.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Observed {
    /// `Response.status`.
    pub status: u16,
    /// `Response.reason`.
    pub reason: String,
    /// `Response.url`, i.e. `page.url`.
    pub url: String,
    /// `Response.encoding`.
    pub encoding: String,
    /// `[[url, status], …]`, oldest hop first.
    pub history: Vec<serde_json::Value>,
    /// Each hop's reason phrase.
    #[serde(default)]
    pub history_reasons: Vec<String>,
    /// Each hop's response header names, lower-cased and sorted.
    #[serde(default)]
    pub history_header_names: Vec<Vec<String>>,
    /// Probe name to `css(...).get()`.
    pub css: HashMap<String, Option<String>>,
    /// Whether the body starts a `<html` document.
    pub body_is_html: bool,
    /// `len(Response.body)`.
    pub body_len: usize,
    /// `Response.cookies`, reduced to name → value.
    pub cookies: HashMap<String, String>,
    /// `//h1[@id='title']/text()`.
    pub xpath_title: Option<String>,
    /// `//p[@class='price']/text()`.
    pub xpath_price: Option<String>,
    /// The URLs `capture_xhr` matched.
    #[serde(default)]
    pub captured_xhr: Vec<String>,
    /// Their bodies.
    #[serde(default)]
    pub captured_xhr_bodies: Vec<String>,
    /// Their statuses.
    #[serde(default)]
    pub captured_xhr_status: Vec<u16>,
    /// Lower-cased names of `Response.headers` — the **first** response's.
    #[serde(default)]
    pub header_names: Vec<String>,
    /// That response's `content-type`.
    #[serde(default)]
    pub content_type: Option<String>,
    /// Lower-cased names of `Response.request_headers`.
    #[serde(default)]
    pub request_header_names: Vec<String>,
    /// The `Referer` the first request carried.
    #[serde(default)]
    pub request_referer: Option<String>,
}

impl Observed {
    /// One CSS probe, or `None` when the selector matched nothing.
    pub fn css(&self, probe: &str) -> Option<&str> {
        self.css.get(probe).and_then(Option::as_deref)
    }

    /// A CSS probe that must have matched.
    ///
    /// # Panics
    ///
    /// If the corpus recorded no value for `probe`.
    pub fn expect_css(&self, probe: &str) -> &str {
        self.css(probe)
            .unwrap_or_else(|| panic!("the corpus recorded a value for {probe:?}"))
    }

    /// The recorded `#clock` value, i.e. roughly when the page was serialised.
    ///
    /// # Panics
    ///
    /// If the row did not record a clock.
    pub fn clock_ms(&self) -> u64 {
        self.expect_css("clock")
            .parse()
            .expect("the clock probe is a whole number of milliseconds")
    }

    /// `[(url, status), …]`.
    ///
    /// # Panics
    ///
    /// If a history entry is not the `[url, status]` pair the generator writes.
    pub fn history_pairs(&self) -> Vec<(String, u16)> {
        self.history
            .iter()
            .map(|entry| {
                let pair = entry.as_array().expect("a [url, status] pair");
                let url = pair[0].as_str().expect("the hop url").to_owned();
                let status =
                    u16::try_from(pair[1].as_u64().expect("the hop status")).expect("a u16 status");
                (url, status)
            })
            .collect()
    }
}

/// One row of the corpus.
#[derive(Clone, Debug, Deserialize)]
pub struct Row {
    /// The scenario name.
    pub name: String,
    /// `"sequence"` for the multi-fetch rows, absent for the rest.
    #[serde(default)]
    pub kind: Option<String>,
    /// The error Python raised, if it raised one.
    pub error: Option<String>,
    /// What the single fetch produced.
    #[serde(default)]
    pub observed: Option<Observed>,
    /// What each fetch of a `sequence` row produced.
    #[serde(default)]
    pub observations: Vec<Observed>,
    /// `get_pool_stats()` after a `sequence` row.
    #[serde(default)]
    pub pool_stats: Option<HashMap<String, i64>>,
    /// `(host, path)` pairs the server received, de-duplicated and sorted.
    pub received: Vec<Vec<String>>,
    /// Every request in arrival order, for the `sequence` rows.
    #[serde(default)]
    pub received_order: Vec<Vec<String>>,
}

impl Row {
    /// The single fetch's observations.
    ///
    /// # Panics
    ///
    /// If the row recorded an error instead.
    pub fn observed(&self) -> &Observed {
        self.observed
            .as_ref()
            .unwrap_or_else(|| panic!("row {:?} recorded a response", self.name))
    }

    /// `received`, as sorted `(host, path)` pairs with the favicon dropped.
    ///
    /// Chrome fetches a favicon for every top-level page, but whether it lands
    /// before the tab is snapshotted is a race — the corpus shows it appearing
    /// and disappearing between otherwise identical runs — so neither side
    /// asserts on it.
    pub fn received_pairs(&self) -> Vec<(String, String)> {
        let mut pairs: Vec<(String, String)> = self
            .received
            .iter()
            .filter_map(|pair| Some((pair.first()?.clone(), pair.get(1)?.clone())))
            .filter(|(_, path)| path != "/favicon.ico")
            .collect();
        pairs.sort();
        pairs.dedup();
        pairs
    }
}

/// The whole corpus.
#[derive(Clone, Debug, Deserialize)]
pub struct Corpus {
    /// Every scenario.
    pub rows: Vec<Row>,
}

impl Corpus {
    /// The corpus shipped in `testdata/`.
    ///
    /// # Panics
    ///
    /// If the file is missing or malformed.
    pub fn load() -> Self {
        let raw = include_str!("../../testdata/u14_browser_golden.json");
        serde_json::from_str(raw).expect("testdata/u14_browser_golden.json parses")
    }

    /// One row by name.
    ///
    /// # Panics
    ///
    /// If no row carries that name — a renamed scenario must fail loudly
    /// rather than silently skip.
    pub fn row(&self, name: &str) -> &Row {
        self.rows
            .iter()
            .find(|row| row.name == name)
            .unwrap_or_else(|| panic!("the corpus has a row named {name:?}"))
    }
}

/// The corpus, loaded once per process.
///
/// # Panics
///
/// If the corpus cannot be parsed.
pub fn corpus() -> &'static Corpus {
    static CORPUS: std::sync::OnceLock<Corpus> = std::sync::OnceLock::new();
    CORPUS.get_or_init(Corpus::load)
}

/// Put this run's port back into a corpus string.
pub fn with_port(value: &str, port: u16) -> String {
    value.replace("{PORT}", &port.to_string())
}
