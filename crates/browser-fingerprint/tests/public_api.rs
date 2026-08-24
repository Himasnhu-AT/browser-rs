//! Cross-module smoke tests over the public surface, exercised the way the
//! HTTP and browser engines will use it.

use std::collections::HashSet;

use browser_fingerprint::ad_domains::{is_ad_domain, is_domain_blocked, DomainBlocklist};
use browser_fingerprint::domains::{
    is_extra_resource, DEFAULT_ARGS, EXTRA_RESOURCES, HARMFUL_ARGS, STEALTH_ARGS,
};
use browser_fingerprint::error::FingerprintError;
use browser_fingerprint::headers::{
    generate_headers, generate_headers_for, headers_for_impersonation, host_os, BrowserMode,
    Device, OsName,
};
use browser_fingerprint::impersonate::{BrowserFamily, ImpersonateProfile};
use browser_fingerprint::status::StatusText;
use browser_fingerprint::useragent::{default_browser_ua, default_chrome_ua, default_ua};

/// The `_headers_job` shape: session headers, then per-request headers, then
/// generated ones only for keys the caller did not already set.
#[test]
fn generated_headers_fill_only_missing_keys() {
    let user: Vec<(String, String)> = vec![
        ("User-Agent".to_owned(), "mine/1.0".to_owned()),
        ("referer".to_owned(), "https://www.google.com/".to_owned()),
    ];
    let taken: HashSet<String> = user.iter().map(|(k, _)| k.to_lowercase()).collect();

    let mut final_headers = user.clone();
    for (name, value) in generate_headers(BrowserMode::Http) {
        if !taken.contains(&name.to_lowercase()) {
            final_headers.push((name, value));
        }
    }

    assert_eq!(final_headers[0].1, "mine/1.0", "user UA must win");
    assert!(final_headers.len() > user.len(), "extras must be appended");
    // Order of the generated tail is preserved relative to the profile.
    let generated: Vec<&str> = final_headers[user.len()..]
        .iter()
        .map(|(k, _)| k.as_str())
        .collect();
    let mut seen = HashSet::new();
    for name in &generated {
        assert!(seen.insert(name.to_lowercase()), "duplicate {name}");
        assert!(
            !taken.contains(&name.to_lowercase()),
            "{name} overrode the user"
        );
    }
}

#[test]
fn the_impersonation_surface_is_usable_from_outside() {
    let profile: ImpersonateProfile = "chrome".parse().expect("alias parses");
    assert_eq!(profile, ImpersonateProfile::Chrome150);
    assert_eq!(profile.family(), BrowserFamily::Chrome);
    assert_eq!(profile.major_version(), 150);
    assert!(!profile.is_mobile());

    let error = ImpersonateProfile::parse("safari999").unwrap_err();
    assert!(matches!(
        error,
        FingerprintError::UnknownImpersonateProfile { .. }
    ));
    assert!(error.to_string().contains("safari260_ios"));

    let headers = headers_for_impersonation(ImpersonateProfile::Safari184Ios)
        .expect("safari ios headers exist");
    let ua = headers.user_agent().unwrap_or_default();
    assert!(ua.contains("iPhone"), "{ua}");
}

#[test]
fn defaults_and_modes_agree_with_the_host() {
    assert!(!default_ua().is_empty());
    assert!(default_browser_ua().contains("Chrome/"));
    assert!(default_chrome_ua().contains("Chrome/"));

    if let Some(os) = host_os() {
        let browser = generate_headers_for(Some(os), BrowserMode::Browser, None);
        let ua = browser.user_agent().unwrap_or_default();
        let marker = match os {
            OsName::Windows => "Windows NT",
            OsName::Macos => "Mac OS X",
            OsName::Linux => "Linux",
            OsName::Android => "Android",
            OsName::Ios => "iPhone",
        };
        assert!(ua.contains(marker), "{os}: {ua}");
        assert_eq!(
            os.device(),
            if ua.contains("Mobile") {
                Device::Mobile
            } else {
                Device::Desktop
            }
        );
    }
}

#[test]
fn interception_decision_matches_the_python_handler() {
    // Resource-type blocking is checked first and short-circuits the domain
    // check; when both are inactive the request continues.
    let blocklist = DomainBlocklist::from_domains(["tracker.test"]).with_ads(true);

    let decide = |resource_type: &str, host: &str, disable_resources: bool| -> bool {
        if disable_resources && is_extra_resource(resource_type) {
            return true;
        }
        !blocklist.is_empty() && blocklist.blocks(host)
    };

    assert!(decide("stylesheet", "example.com", true));
    assert!(!decide("stylesheet", "example.com", false));
    assert!(decide("document", "cdn.tracker.test", false));
    assert!(decide("document", "doubleclick.net", false));
    assert!(!decide("document", "example.com", false));
}

#[test]
fn blocklist_helpers_agree_with_the_built_in_table() {
    let manual: HashSet<String> = ["doubleclick.net".to_owned()].into_iter().collect();
    assert!(is_domain_blocked("ads.doubleclick.net", &manual));
    assert!(is_ad_domain("ads.doubleclick.net"));
    assert!(!is_ad_domain("rust-lang.org"));
}

#[test]
fn status_phrases_are_reachable() {
    assert_eq!(StatusText::get(200), "OK");
    assert_eq!(StatusText::get(599), "Unknown Status Code");
}

#[test]
fn launch_flag_tables_are_reachable_and_disjoint() {
    assert_eq!(EXTRA_RESOURCES.len(), 10);
    let stealth: HashSet<&&str> = STEALTH_ARGS.iter().collect();
    for flag in DEFAULT_ARGS {
        assert!(!stealth.contains(flag), "{flag} is in both arg tables");
    }
    for flag in HARMFUL_ARGS {
        assert!(!stealth.contains(flag), "{flag} is removed and added");
    }
}
