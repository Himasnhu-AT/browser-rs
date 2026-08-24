//! [`AD_DOMAINS`] as a build-time `phf::Set`, plus [`is_domain_blocked`].
//!
//! The list is Peter Lowe's ad/tracking server list as vendored by
//! `reference/engines/toolbelt/ad_domains.py`; it ships as
//! `assets/ad_domains.txt` and `build.rs` compiles it into a perfect hash.
//!
//! `block_ads = true` **merges** [`AD_DOMAINS`] with any user-supplied
//! blocklist and never mutates the caller's set — see [`DomainBlocklist`].

use std::collections::HashSet;

include!(concat!(env!("OUT_DIR"), "/ad_domains.rs"));

/// Suffix-chain domain matching, ported literally from
/// `toolbelt/navigation.py::_is_domain_blocked`.
///
/// For `tracker.ads.doubleclick.net` this tests, in order,
/// `tracker.ads.doubleclick.net`, `ads.doubleclick.net`, `doubleclick.net`.
///
/// Two properties are load bearing and must not be "improved":
///
/// * the `suffix.contains('.')` guard means a bare TLD (`net`) is **never**
///   consulted, so a blocklist entry can never take out a whole TLD;
/// * matching is exact on label boundaries, so `example.com` does **not**
///   block `notexample.com`.
///
/// A public-suffix-aware implementation would be *more* correct and would
/// disagree with the reference implementation, so this stays a literal port. Case is not
/// normalised — Python feeds it `urlparse().hostname`, which is already
/// lowercase; callers holding a raw `Host` header should lowercase first.
///
/// # Examples
///
/// ```
/// use std::collections::HashSet;
/// use browser_fingerprint::ad_domains::is_domain_blocked;
///
/// let blocked: HashSet<String> = ["doubleclick.net".to_owned()].into_iter().collect();
/// assert!(is_domain_blocked("tracker.ads.doubleclick.net", &blocked));
/// assert!(!is_domain_blocked("notdoubleclick.net", &blocked));
/// ```
#[must_use]
pub fn is_domain_blocked(hostname: &str, domains: &HashSet<String>) -> bool {
    if domains.is_empty() {
        return false;
    }
    is_domain_blocked_by(hostname, |candidate| domains.contains(candidate))
}

/// [`is_domain_blocked`] against the built-in [`AD_DOMAINS`] table.
///
/// # Examples
///
/// ```
/// use browser_fingerprint::ad_domains::is_ad_domain;
///
/// assert!(is_ad_domain("doubleclick.net"));
/// assert!(is_ad_domain("stats.g.doubleclick.net"));
/// assert!(!is_ad_domain("example.com"));
/// ```
#[must_use]
pub fn is_ad_domain(hostname: &str) -> bool {
    is_domain_blocked_by(hostname, |candidate| AD_DOMAINS.contains(candidate))
}

/// The shared suffix walk, generic over the membership test.
///
/// Byte indexing is safe here because `.` is ASCII, so `find` can only ever
/// return a char boundary even for IDN hostnames.
fn is_domain_blocked_by(hostname: &str, contains: impl Fn(&str) -> bool) -> bool {
    if contains(hostname) {
        return true;
    }
    let mut start = 0usize;
    while let Some(offset) = hostname[start..].find('.') {
        let idx = start + offset;
        let suffix = &hostname[idx + 1..];
        if suffix.contains('.') && contains(suffix) {
            return true;
        }
        start = idx + 1;
    }
    false
}

/// A resolved blocklist: the user's domains, optionally merged with
/// [`AD_DOMAINS`].
///
/// Mirrors `PlaywrightConfig.__post_init__`, which does
/// `blocked_domains | set(AD_DOMAINS)` — a *new* set, so the caller's own
/// collection is never mutated. Keeping the ad list as the compiled `phf::Set`
/// rather than copying 3 500 `String`s into a `HashSet` is the whole point of
/// the build-time table.
#[derive(Debug, Clone, Default)]
pub struct DomainBlocklist {
    extra: HashSet<String>,
    block_ads: bool,
}

impl DomainBlocklist {
    /// An empty blocklist that blocks nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A blocklist over `domains`, with the built-in ad list off.
    #[must_use]
    pub fn from_domains<I, S>(domains: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            extra: domains.into_iter().map(Into::into).collect(),
            block_ads: false,
        }
    }

    /// Merge (or un-merge) the built-in [`AD_DOMAINS`] table.
    #[must_use]
    pub fn with_ads(mut self, block_ads: bool) -> Self {
        self.block_ads = block_ads;
        self
    }

    /// Whether this blocklist would abort a request to `hostname`.
    #[must_use]
    pub fn blocks(&self, hostname: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        let block_ads = self.block_ads;
        let extra = &self.extra;
        is_domain_blocked_by(hostname, |candidate| {
            extra.contains(candidate) || (block_ads && AD_DOMAINS.contains(candidate))
        })
    }

    /// Whether the blocklist can never match, in which case interception need
    /// not be installed at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.block_ads && self.extra.is_empty()
    }

    /// The caller-supplied domains, without the built-in ad list.
    #[must_use]
    pub fn user_domains(&self) -> &HashSet<String> {
        &self.extra
    }

    /// Whether the built-in ad list is merged in.
    #[must_use]
    pub fn blocks_ads(&self) -> bool {
        self.block_ads
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn the_built_in_table_is_complete() {
        // ad_domains.py binds ~3 526 entries; toolbelt.md §8.1 says the same.
        assert_eq!(AD_DOMAINS.len(), AD_DOMAIN_COUNT);
        assert_eq!(AD_DOMAIN_COUNT, 3_526);
    }

    #[test]
    fn known_entries_are_present() {
        for domain in [
            "101com.com",
            "2mdn.net",
            "doubleclick.net",
            "googlesyndication.com",
            "zpu.samsungelectronics.com",
            "zzhc.vnet.cn",
        ] {
            assert!(AD_DOMAINS.contains(domain), "{domain} missing");
        }
    }

    #[test]
    fn suffix_chain_walks_every_registrable_parent() {
        let blocked = set(&["doubleclick.net"]);
        assert!(is_domain_blocked("doubleclick.net", &blocked));
        assert!(is_domain_blocked("ads.doubleclick.net", &blocked));
        assert!(is_domain_blocked("tracker.ads.doubleclick.net", &blocked));
    }

    #[test]
    fn a_bare_tld_is_never_consulted() {
        // The `"." in suffix` guard: even with "net" in the set, ads.net must
        // only match via the full host or a multi-label suffix.
        let blocked = set(&["net"]);
        assert!(!is_domain_blocked("ads.doubleclick.net", &blocked));
        assert!(is_domain_blocked("net", &blocked));
    }

    #[test]
    fn matching_is_exact_on_label_boundaries() {
        let blocked = set(&["example.com"]);
        assert!(!is_domain_blocked("notexample.com", &blocked));
        assert!(!is_domain_blocked("example.com.evil.org", &blocked));
        assert!(is_domain_blocked("a.b.example.com", &blocked));
    }

    #[test]
    fn an_empty_blocklist_blocks_nothing() {
        let blocked = HashSet::new();
        assert!(!is_domain_blocked("doubleclick.net", &blocked));
        assert!(!is_domain_blocked("", &blocked));
    }

    #[test]
    fn degenerate_hostnames_do_not_panic() {
        let blocked = set(&["a.b", "..", ".b"]);
        for host in ["", ".", "..", "...", "a.", ".a", "a.b", "a..b"] {
            let _ = is_domain_blocked(host, &blocked);
        }
        assert!(is_domain_blocked("x.a.b", &blocked));
    }

    #[test]
    fn non_ascii_hostnames_do_not_panic() {
        let blocked = set(&["münchen.example", "例え.テスト"]);
        assert!(is_domain_blocked("www.münchen.example", &blocked));
        assert!(is_domain_blocked("ドメイン.例え.テスト", &blocked));
        assert!(!is_domain_blocked("münchen.other", &blocked));
    }

    #[test]
    fn the_built_in_helper_walks_the_same_chain() {
        assert!(is_ad_domain("doubleclick.net"));
        assert!(is_ad_domain("stats.g.doubleclick.net"));
        assert!(!is_ad_domain("example.com"));
        assert!(!is_ad_domain(""));
    }

    #[test]
    fn blocklist_merges_without_mutating_the_caller_set() {
        let user = set(&["internal.test"]);
        let list = DomainBlocklist::from_domains(user.iter().cloned()).with_ads(true);
        assert!(list.blocks("internal.test"));
        assert!(list.blocks("cdn.internal.test"));
        assert!(list.blocks("doubleclick.net"));
        assert!(!list.blocks("example.com"));
        // The caller's own set is untouched.
        assert_eq!(user.len(), 1);
        assert_eq!(list.user_domains(), &user);
        assert!(list.blocks_ads());
    }

    #[test]
    fn block_ads_off_leaves_the_user_list_alone() {
        let list = DomainBlocklist::from_domains(["internal.test"]);
        assert!(list.blocks("internal.test"));
        assert!(!list.blocks("doubleclick.net"));
        assert!(!list.is_empty());
    }

    #[test]
    fn an_empty_blocklist_short_circuits() {
        let list = DomainBlocklist::new();
        assert!(list.is_empty());
        assert!(!list.blocks("doubleclick.net"));
        assert!(DomainBlocklist::new()
            .with_ads(true)
            .blocks("doubleclick.net"));
    }
}
