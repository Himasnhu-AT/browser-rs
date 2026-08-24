//! Adversarial probe of the routing helpers against values captured from the
//! reference interpreter (`_is_domain_blocked` and `urlparse(...).hostname`).

use std::collections::HashSet;

use browser_cdp::route::{host_of, RouteDecision, RoutePolicy};

fn policy() -> RoutePolicy {
    RoutePolicy::new(
        false,
        Some(HashSet::from([
            "doubleclick.net".to_owned(),
            "ads.example.com".to_owned(),
            "example".to_owned(),
            "a.b.c.d".to_owned(),
        ])),
    )
}

#[test]
fn domain_suffix_walk_matches_python() {
    // (hostname, blocked) captured from `_is_domain_blocked`.
    let cases: &[(&str, bool)] = &[
        ("doubleclick.net", true),
        ("tracker.ads.doubleclick.net", true),
        ("x.doubleclick.net.evil.com", false),
        ("example", true),
        // A parent suffix without a dot is never consulted, so "sub.example"
        // does not match the dotless entry "example".
        ("sub.example", false),
        ("ads.example.com", true),
        ("deep.ads.example.com", true),
        ("a.b.c.d", true),
        ("z.a.b.c.d", true),
        ("b.c.d", false),
        ("", false),
        (".doubleclick.net", true),
        // Python does no case folding here; `urlparse` already lower-cased.
        ("DOUBLECLICK.NET", false),
    ];
    let policy = policy();
    for (hostname, blocked) in cases {
        assert_eq!(
            policy.decide("document", hostname) == RouteDecision::Abort,
            *blocked,
            "{hostname}",
        );
    }
}

/// Every pair captured from `urlparse(url).hostname or ""` under the reference
/// interpreter.
#[test]
fn host_of_matches_urlparse() {
    let cases: &[(&str, &str)] = &[
        ("https://User:p@Host.Example.COM:8080/x", "host.example.com"),
        ("http://[::1]:80/", "::1"),
        ("not a url", ""),
        ("", ""),
        ("file:///tmp/x", ""),
        ("//h/x", "h"),
        ("https://h", "h"),
        ("http://ADS.example.com", "ads.example.com"),
        ("https://a.b.c/d?e=f#g", "a.b.c"),
        ("https://user@h", "h"),
        ("https://:pw@h", "h"),
        ("http://h:", "h"),
        ("http://:8080", ""),
        ("ws://h/x", "h"),
        ("data:text/html,<p>x</p>", ""),
        ("about:blank", ""),
        ("chrome-extension://abc/def", "abc"),
        ("blob:https://h/uuid", ""),
        ("https://[2001:db8::1]/p", "2001:db8::1"),
        ("https://h.test/#frag", "h.test"),
        ("https://h.test?q=1", "h.test"),
        ("HTTPS://H.TEST/", "h.test"),
        ("https://xn--n3h.example/", "xn--n3h.example"),
        ("http://h..test/", "h..test"),
        ("http://.h.test/", ".h.test"),
        ("http://h.test.:80/", "h.test."),
        ("https://user:p%40w@h.test/", "h.test"),
        ("//UPPER.test", "upper.test"),
        ("http://[fe80::1%25eth0]:9/", "fe80::1%25eth0"),
        ("javascript:void(0)", ""),
    ];
    for (url, want) in cases {
        assert_eq!(host_of(url), *want, "{url}");
    }
}

#[test]
fn resource_type_blocking_precedes_domain_blocking() {
    let policy = RoutePolicy::new(true, Some(HashSet::from(["safe.test".to_owned()])));
    // An image from a non-blocked host is still aborted.
    assert_eq!(policy.decide("image", "cdn.test"), RouteDecision::Abort);
    // A document from a blocked host is aborted too.
    assert_eq!(policy.decide("document", "safe.test"), RouteDecision::Abort);
    // Neither: continue.
    assert_eq!(
        policy.decide("document", "cdn.test"),
        RouteDecision::Continue
    );
}
