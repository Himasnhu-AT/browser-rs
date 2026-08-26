//! Every rule in the Public Suffix List, through both branches of `tld`.
//!
//! `testdata/psl_sweep.jsonl` is 20 306 rows — two per rule in `tld` 0.13.2's
//! bundled `res/effective_tld_names.dat.txt` — each carrying the answer
//! The reference implementation's `StorageSystemMixin._get_base_url()` actually gave under CPython
//! 3.14. `tools/gen_psl_sweep.py` regenerates it.
//!
//! # Why sweep the whole list
//!
//! [`registrable_domain`] is a faithful port of `tld`'s *algorithm*, but it
//! consults a different copy of the *data*: the `psl` crate compiles its own
//! snapshot of the list in. Everything else about the two implementations is
//! pinned by the hand-written corpora; this is the only gate that can catch the
//! failure mode where a `psl` version bump silently moves one namespace.
//!
//! # The known divergences
//!
//! 40 of the 20 306 rows disagree, and every one has the same shape: a rule
//! `tld`'s bundled file still carries and `psl`'s snapshot has dropped, so
//! `psl` resolves one label short. They fall into two groups:
//!
//! * **private-section entries retired upstream** — `deta.app`, `deta.dev`,
//!   `cloudapps.digital`, `onfabrica.com`, `api.stdlib.com`, `hzc.io`,
//!   `pagexl.com`, `protonet.io`, `storj.farm`, `mayfirst.org`,
//!   `bookonline.app`, `reserve-online.com`, `vps.hrsn.au`, `uk0.bigv.io`,
//!   `vm.bytemark.co.uk`, `dh.bytemark.co.uk`, `on.crisp.email`,
//!   `*.cryptonomic.net`, `*.on-acorn.io`;
//! * **public entries retired upstream** — `biz.at`, `info.at`, `ac.tj`,
//!   `cc.nd.us`, `lib.nd.us`, `aquila.it`, `valdaosta.it`,
//!   `trentinosudtirol.it`, `audnedaln.no`, `frei.no`.
//!
//! `tld`'s file is the older snapshot in every case, so this crate is not
//! *wrong* — it agrees with today's PSL — but it is not *identical* to the
//! reference either, and a database shared with a Python process pinned to
//! `tld==0.13.2` would split those 39 hosts across two namespaces. The
//! divergences are therefore listed row by row in
//! `testdata/psl_sweep_divergences.jsonl` rather than tolerated in bulk: the
//! test below insists the disagreements are *exactly* that set, so a `psl` bump
//! that shifts any other suffix fails loudly.
//!
//! Closing the gap for real would mean vendoring `tld`'s snapshot verbatim,
//! which trades agreement-with-Python for agreement-with-reality on ~10 000
//! other rules. See the note in [`base_url`].
//!
//! [`base_url`]: browser_storage::base_url

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use browser_storage::base_url::registrable_domain;

fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

fn lines(name: &str) -> Vec<serde_json::Value> {
    let path = testdata(name);
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
        .map(|(lineno, line)| {
            serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("{}:{}: {e}", path.display(), lineno + 1))
        })
        .collect()
}

/// `url -> (tld's answer, psl's answer)` for the rows known to disagree.
fn known_divergences() -> HashMap<String, (String, String)> {
    lines("psl_sweep_divergences.jsonl")
        .into_iter()
        .map(|row| {
            (
                row["url"].as_str().expect("url").to_owned(),
                (
                    row["tld"].as_str().expect("tld").to_owned(),
                    row["psl"].as_str().expect("psl").to_owned(),
                ),
            )
        })
        .collect()
}

#[test]
fn the_whole_suffix_list_agrees_except_for_the_listed_rows() {
    let sweep = lines("psl_sweep.jsonl");
    assert!(
        sweep.len() >= 20_306,
        "the sweep lost rows: {} left — regenerate with tools/gen_psl_sweep.py, \
         never trim it to make this pass",
        sweep.len(),
    );

    let divergences = known_divergences();
    let mut unexpected = Vec::new();
    let mut healed = Vec::new();
    let mut agreed = 0usize;

    for row in &sweep {
        let url = row["url"].as_str().expect("url");
        let expected = row["expected"].as_str().expect("expected");
        let got = registrable_domain(Some(url));

        match divergences.get(url) {
            None if got == expected => agreed += 1,
            None => unexpected.push(format!("  {url}\n    rust = {got}\n    tld  = {expected}")),
            Some((tld, psl)) => {
                assert_eq!(
                    expected, tld,
                    "{url}: the divergence file disagrees with the sweep about what tld says",
                );
                if got == expected {
                    healed.push(url.to_owned());
                } else if &got != psl {
                    unexpected.push(format!(
                        "  {url}\n    rust     = {got}\n    tld      = {tld}\n    \
                         recorded = {psl}  (the divergence moved, it did not go away)"
                    ));
                }
            }
        }
    }

    assert!(
        unexpected.is_empty(),
        "{} row(s) of {} disagree with the reference implementation in a way that is not recorded in \
         testdata/psl_sweep_divergences.jsonl.\nThis is almost certainly a `psl` \
         version bump moving a public suffix; re-read the module docs before \
         widening the allowlist.\n{}",
        unexpected.len(),
        sweep.len(),
        unexpected.join("\n"),
    );
    assert!(
        healed.is_empty(),
        "{} recorded divergence(s) now agree with the reference implementation — regenerate \
         testdata/psl_sweep_divergences.jsonl so the allowlist stays exact:\n  {}",
        healed.len(),
        healed.join("\n  "),
    );

    assert_eq!(
        agreed,
        sweep.len() - divergences.len(),
        "accounting error: every row is either an agreement or a recorded divergence",
    );
    assert_eq!(
        divergences.len(),
        40,
        "the number of known suffix-list divergences changed",
    );
}

/// Every recorded divergence is the *same* failure — `psl` resolving one label
/// short because its snapshot dropped a rule `tld` still has — and none of them
/// changes the shape of the answer (e.g. into `"default"`).
///
/// If a future `psl` bump introduces a divergence of a different kind, this
/// catches it even though the row count might happen to stay the same.
#[test]
fn every_recorded_divergence_is_a_dropped_suffix_rule() {
    for (url, (tld, psl)) in known_divergences() {
        assert_ne!(psl, "default", "{url}: a dropped rule should not unresolve");
        assert!(
            tld.ends_with(&psl),
            "{url}: tld={tld} psl={psl} — expected psl's answer to be a suffix of tld's, \
             which is what 'psl resolved one label short' looks like",
        );
        assert!(
            tld.len() > psl.len(),
            "{url}: tld={tld} psl={psl} — expected tld to keep strictly more labels",
        );
    }
}
