//! Differential test of [`browser_cdp::route`]'s CDP↔Playwright
//! resource-type table against the node driver that browser-rs 0.4.15 actually
//! drives.
//!
//! `testdata/resource_types_golden.jsonl` is produced by
//! `gen_resource_types_golden.py` (kept outside the repo), which parses
//! `toResourceType` out of the bundled
//! `playwright/driver/package/lib/coreBundle.js` for all three backends and
//! pairs it with `browser-rs.engines.constants.EXTRA_RESOURCES`.
//!
//! This is the one part of the crate whose ground truth is not reachable by
//! calling Python: the mapping runs inside the driver process, on a live
//! browser, and is only observable by driving a real Chromium. Reading the
//! shipped switch is the closest thing to running it.

use std::collections::BTreeSet;

use browser_cdp::route::{
    cdp_resource_type, effective_blocked_resource_types, is_chromium_unreachable,
    playwright_resource_type, RoutePolicy, CHROMIUM_FALLBACK_RESOURCE_TYPE,
    DEAD_EVERYWHERE_RESOURCE_TYPES, PLAYWRIGHT_ONLY_RESOURCE_TYPES, RESOURCE_TYPE_TABLE,
};
use browser_fingerprint::domains::EXTRA_RESOURCES;
use serde_json::Value;

const GOLDEN: &str = include_str!("../testdata/resource_types_golden.jsonl");

fn rows() -> Vec<Value> {
    GOLDEN
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("a golden row"))
        .collect()
}

fn values(row: &Value) -> BTreeSet<String> {
    row["values"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|v| v.as_str().expect("a string").to_owned())
        .collect()
}

fn find<'a>(rows: &'a [Value], kind: &str) -> &'a Value {
    rows.iter()
        .find(|r| r["kind"] == kind)
        .unwrap_or_else(|| panic!("golden has no `{kind}` row"))
}

fn set_of<I: IntoIterator<Item = S>, S: AsRef<str>>(items: I) -> BTreeSet<String> {
    items
        .into_iter()
        .map(|s| s.as_ref().to_owned())
        .collect::<BTreeSet<_>>()
}

/// Every CDP `Network.ResourceType` in the protocol maps the way the driver's
/// `switch` maps it, including the four that collapse into `other`.
#[test]
fn chromium_resource_type_mapping_matches_the_driver() {
    let rows = rows();
    let mut checked = 0_usize;
    let mut failures = Vec::new();
    let mut table = BTreeSet::new();

    for row in rows.iter().filter(|r| r["kind"] == "chromium") {
        let cdp = row["cdp"].as_str().expect("a cdp type");
        let want = row["playwright"].as_str().expect("a playwright type");
        checked += 1;
        table.insert(cdp.to_owned());
        let got = playwright_resource_type(cdp);
        if got != want {
            failures.push(format!("{cdp}: want {want:?}, got {got:?}"));
        }
    }

    // The table must cover the whole protocol enum, not just the rows that
    // happen to be exercised.
    let covered: BTreeSet<String> = RESOURCE_TYPE_TABLE
        .iter()
        .map(|(cdp, _)| (*cdp).to_owned())
        .collect();
    assert_eq!(covered, table, "table and protocol enum diverge");

    assert_eq!(
        find(&rows, "chromium_default")["playwright"]
            .as_str()
            .expect("a default"),
        CHROMIUM_FALLBACK_RESOURCE_TYPE,
    );
    // The `default:` arm catches anything the enum grows later.
    assert_eq!(
        playwright_resource_type("SomethingChromiumAddsIn2027"),
        CHROMIUM_FALLBACK_RESOURCE_TYPE,
    );

    assert!(checked >= 19, "corpus shrank to {checked} CDP types");
    assert!(
        failures.is_empty(),
        "{}/{checked} CDP types disagree:\n{}",
        failures.len(),
        failures.join("\n"),
    );
}

/// The reverse lookup only ever names a CDP type the forward table agrees with.
#[test]
fn reverse_lookup_is_consistent() {
    for (playwright, cdp) in RESOURCE_TYPE_TABLE
        .iter()
        .filter_map(|(_cdp, pw)| cdp_resource_type(pw).map(|found| (*pw, found)))
    {
        assert_eq!(
            playwright_resource_type(cdp),
            playwright,
            "cdp_resource_type({playwright:?}) named {cdp:?}, which maps elsewhere",
        );
    }
    assert_eq!(cdp_resource_type("other"), Some("Other"));
    assert_eq!(cdp_resource_type("cspreport"), Some("CSPViolationReport"));
    assert_eq!(cdp_resource_type("not-a-resource-type"), None);
}

/// `EXTRA_RESOURCES` is browser-rs's, verbatim.
#[test]
fn extra_resources_matches_browser-rs() {
    let rows = rows();
    assert_eq!(
        values(find(&rows, "extra_resources")),
        set_of(EXTRA_RESOURCES)
    );
}

/// The four `EXTRA_RESOURCES` entries Chromium can never produce, and the three
/// that no backend produces at all.
#[test]
fn dead_resource_types_match_the_driver() {
    let rows = rows();

    let want_dead = values(find(&rows, "dead_on_chromium"));
    let got_dead = set_of(
        EXTRA_RESOURCES
            .iter()
            .filter(|name| is_chromium_unreachable(name)),
    );
    assert_eq!(want_dead, got_dead, "dead-on-Chromium set");
    assert_eq!(
        want_dead,
        set_of(PLAYWRIGHT_ONLY_RESOURCE_TYPES.iter().map(|(name, _)| *name)),
        "PLAYWRIGHT_ONLY_RESOURCE_TYPES must be exactly the dead-on-Chromium set",
    );

    assert_eq!(
        values(find(&rows, "dead_everywhere")),
        set_of(DEAD_EVERYWHERE_RESOURCE_TYPES),
        "dead-everywhere set",
    );

    // `beacon` is the one that is real, just not on Chromium.
    let firefox = values(find(&rows, "firefox_emitted"));
    let webkit = values(find(&rows, "webkit_emitted"));
    assert!(firefox.contains("beacon") && webkit.contains("beacon"));
    for name in DEAD_EVERYWHERE_RESOURCE_TYPES {
        assert!(
            !firefox.contains(*name) && !webkit.contains(*name),
            "{name} is emitted by a backend after all",
        );
    }
}

/// What `disable_resources = true` actually aborts on Chromium.
#[test]
fn effective_blocking_matches_the_driver() {
    let rows = rows();
    let want = values(find(&rows, "effective_blocked"));
    assert_eq!(want, set_of(effective_blocked_resource_types()));

    // Drive it through the real decision function, one CDP type at a time.
    let policy = RoutePolicy::new(true, None);
    let mut aborted = BTreeSet::new();
    for row in rows.iter().filter(|r| r["kind"] == "chromium") {
        let cdp = row["cdp"].as_str().expect("a cdp type");
        if policy.decide_cdp(cdp, "example.com").is_abort() {
            aborted.insert(row["playwright"].as_str().expect("a type").to_owned());
        }
    }
    assert_eq!(want, aborted, "decide_cdp over the whole protocol enum");

    // The lenient variant additionally catches the CDP types that carry the
    // dead `EXTRA_RESOURCES` names: `CSPViolationReport` (spelled `csp_report`
    // by browser-rs, `cspreport` by the driver) and `Ping` (which is how
    // Chromium reports `navigator.sendBeacon`, spelled `beacon` elsewhere).
    // `object` and `imageset` add nothing: their nearest CDP types are `Other`
    // and `Image`, and `Image` is already blocked.
    let mut lenient = BTreeSet::new();
    for row in rows.iter().filter(|r| r["kind"] == "chromium") {
        let cdp = row["cdp"].as_str().expect("a cdp type");
        if policy.decide_cdp_lenient(cdp, "example.com").is_abort() {
            lenient.insert(cdp.to_owned());
        }
    }
    let mut want_lenient: BTreeSet<String> = want
        .iter()
        .filter_map(|pw| cdp_resource_type(pw).map(str::to_owned))
        .collect();
    want_lenient.insert("CSPViolationReport".to_owned());
    want_lenient.insert("Ping".to_owned());
    want_lenient.insert("Other".to_owned()); // via the `object` alias
    assert_eq!(want_lenient, lenient, "decide_cdp_lenient");
}
