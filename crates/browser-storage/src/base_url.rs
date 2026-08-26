//! `registrable_domain()` via `psl`, with the `fix_protocol` prepend and the
//! `fld -> domain -> "default"` fallback chain.
//!
//! # What this reproduces
//!
//! The reference implementation namespaces every stored fingerprint by the *registrable domain* of
//! the page it came from, so that `example.com` and `other.com` can both store a
//! `"div.price"` identifier in the same `elements_storage.db` without colliding.
//! The Python side is two functions:
//!
//! ```python
//! # StorageSystemMixin.__init__
//! self.url = url.lower() if (url and isinstance(url, str)) else None
//!
//! # StorageSystemMixin._get_base_url  (memoised, LRU 64)
//! if not self.url:
//!     return default_value
//! extracted = tld.get_tld(self.url, as_object=True, fail_silently=True, fix_protocol=True)
//! if not extracted:
//!     return default_value
//! return extracted.fld or extracted.domain or default_value
//! ```
//!
//! [`normalise_url`] is the constructor half and [`registrable_domain`] the
//! `_get_base_url` half. There is no cache: `ARCHITECTURE.md` §1.5 folds
//! Python's LRU away by computing the value **once in the constructor**, which
//! is observationally identical because `self.url` never changes afterwards.
//!
//! # Why the URL is parsed by hand
//!
//! `tld` splits the URL with CPython's [`urllib.parse.urlsplit`] and looks the
//! host up in the Public Suffix List. `urlsplit` is a *syntactic* splitter: it
//! does no IDNA, no percent-decoding and no host validation, so
//! `https://例え.jp/` yields the host `例え.jp` and `tld` returns `例え.jp`. The
//! `url` crate is a WHATWG parser and would answer `xn--r8jz45g.jp` — a
//! different namespace key, which would make a database written by the Python
//! implementation unreadable from here for every internationalised domain. So
//! `split_host` (private) is a direct transcription of `urlsplit` +
//! `SplitResult.hostname` instead.
//!
//! [`urllib.parse.urlsplit`]: https://docs.python.org/3/library/urllib.parse.html#urllib.parse.urlsplit
//!
//! # Fidelity notes
//!
//! * The suffix list is `psl`'s compiled-in snapshot, `tld`'s is its own bundled
//!   `effective_tld_names.dat.txt`. Both include the *private* section (so
//!   `a.blogspot.com` is its own registrable domain), but the two snapshots are
//!   taken on different days; a suffix added or removed in between is the one
//!   way this function can disagree with Python.
//!
//!   That is not hypothetical, and it is measured rather than assumed:
//!   `tests/psl_sweep.rs` runs **all 10 153 rules** of `tld`'s file through both
//!   branches of the label arithmetic and finds **40 of 20 306 rows** disagree,
//!   every one of them a rule the PSL has retired since `tld` 0.13.2 was cut
//!   (`biz.at`, `deta.app`, `cloudapps.digital`, `ac.tj`, …). `psl` is the more
//!   current of the two, so this crate is right about the modern web and
//!   *different* from a Python process pinned to `tld==0.13.2`. The affected
//!   hosts are listed row by row in `testdata/psl_sweep_divergences.jsonl` and
//!   the test insists the disagreement set is exactly that — a `psl` bump that
//!   moves any other suffix fails immediately. Re-run
//!   `tools/gen_psl_sweep.py` after bumping `psl`.
//! * `tld` treats the *parent* of an exception rule (`ck`, from `!www.ck`) as a
//!   suffix in its own right, where `psl` implements the PSL algorithm and does
//!   not. That is an algorithmic difference rather than a vintage one, and it is
//!   bridged by `EXCEPTION_RULE_PARENTS`; `tests/base_url_exceptions.rs` pins
//!   the eight entries against both libraries.
//! * `tld` fails when the host's suffix is not in the list, which is why
//!   `localhost`, bare IPv4/IPv6 hosts and `example.invalidtld` all fall back to
//!   `"default"`. [`registrable_domain`] reproduces that by rejecting a
//!   [`psl::Suffix`] whose [`typ`](psl::Suffix::typ) is `None`.

use std::borrow::Cow;

/// The namespace used when the URL is missing, empty or unresolvable.
///
/// Python's `_get_base_url(default_value="default")`. Callers never pass
/// anything else, but [`registrable_domain_with_default`] keeps the parameter.
pub const DEFAULT_BASE_URL: &str = "default";

/// Characters accepted in a URL scheme by CPython's `urlsplit`
/// (`string.ascii_letters + string.digits + "+-."`).
fn is_scheme_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.')
}

/// `StorageSystemMixin.__init__`'s URL normalisation.
///
/// Lowercases the whole URL — path, query and fragment included, exactly as
/// Python does — and collapses `None` and `""` to `None`.
///
/// The lowercasing is not cosmetic: it works around [tld#124], where a mixed
/// case host is not found in the suffix list. The reference implementation's test suite pins the
/// observable consequence (`storage.url == "https://www.example.com/page"` after
/// constructing with `"https://WWW.EXAMPLE.COM/Page"`), so the stored URL is
/// lowercased here rather than only inside the lookup.
///
/// [tld#124]: https://github.com/barseghyanartur/tld/issues/124
///
/// ```
/// use browser_storage::base_url::normalise_url;
///
/// assert_eq!(
///     normalise_url(Some("https://WWW.EXAMPLE.COM/Page")).as_deref(),
///     Some("https://www.example.com/page"),
/// );
/// assert_eq!(normalise_url(Some("")), None);
/// assert_eq!(normalise_url(None), None);
/// ```
#[must_use]
pub fn normalise_url(url: Option<&str>) -> Option<String> {
    match url {
        Some(url) if !url.is_empty() => Some(url.to_lowercase()),
        _ => None,
    }
}

/// `tld`'s `fix_protocol=True`: prepend `https://` unless the URL already
/// carries a `…//` prefix.
///
/// The test is `tld.utils.protocol_re`, `^[a-z0-9.+:-]*//`, applied to the
/// lowercased URL — deliberately looser than a scheme match, so that the
/// scheme-relative `//example.com/x` is left alone.
fn fix_protocol(url: &str) -> Cow<'_, str> {
    let lowered = url.to_lowercase();
    let prefix_len = lowered
        .char_indices()
        .find(|(_, c)| !is_scheme_char(*c) && *c != ':')
        .map_or(lowered.len(), |(i, _)| i);
    // `^[a-z0-9.+:-]*//`: `/` is not in the character class, so the only place
    // the `//` can sit is immediately after the maximal run of class characters.
    if lowered[prefix_len..].starts_with("//") {
        Cow::Borrowed(url)
    } else {
        Cow::Owned(format!("https://{url}"))
    }
}

/// CPython's `urlsplit(url).hostname`, transcribed.
///
/// Returns `None` for every input `urlsplit` would leave without a host, and
/// for the `Invalid IPv6 URL` `ValueError` (which `tld` swallows into a
/// `None` result when `fail_silently=True`).
///
/// The steps, in `urlsplit`'s order:
///
/// 1. strip leading C0 controls and spaces, and remove every `\t`, `\r` and
///    `\n` anywhere in the URL;
/// 2. take a scheme off the front if the text before the first `:` is a
///    non-empty run of scheme characters starting with an ASCII letter;
/// 3. the rest must start with `//`; the authority runs to the first `/`, `?`
///    or `#`;
/// 4. if the authority carries brackets, validate it as a whole
///    ([`check_bracketed_netloc`]) — note that this splits the userinfo off
///    *first*, so a `@` after the bracket rejects the URL outright;
/// 5. reject a non-ASCII authority whose NFKC form grows a URL delimiter
///    ([`check_netloc`]);
/// 6. drop everything up to and including the **last** `@` (userinfo);
/// 7. a `[…]` bracketed host wins, otherwise the host runs to the first `:`;
/// 8. lowercase the host, but leave an IPv6 zone (`%eth0`) alone.
fn split_host(url: &str) -> Option<String> {
    let stripped: String = url
        .trim_start_matches(|c: char| c <= '\u{20}')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\r' | '\n'))
        .collect();

    // 2. Optional scheme.
    let mut rest = stripped.as_str();
    if let Some(colon) = rest.find(':') {
        let scheme = &rest[..colon];
        if !scheme.is_empty()
            && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme.chars().all(is_scheme_char)
        {
            rest = &rest[colon + 1..];
        }
    }

    // 3. Authority.
    let after_slashes = rest.strip_prefix("//")?;
    let end = after_slashes
        .find(['/', '?', '#'])
        .unwrap_or(after_slashes.len());
    let netloc = &after_slashes[..end];
    // `urlsplit` raises `ValueError("Invalid IPv6 URL")` on an unbalanced
    // bracket; `tld(fail_silently=True)` turns that into `None`.
    if netloc.contains('[') != netloc.contains(']') {
        return None;
    }

    // 4. A netloc that carries brackets at all is validated as a whole.
    if netloc.contains('[') && netloc.contains(']') && !check_bracketed_netloc(netloc) {
        return None;
    }

    // 5. A non-ASCII netloc must survive the NFKC trap.
    if !check_netloc(netloc) {
        return None;
    }

    // 6. Userinfo.
    let hostinfo = netloc.rsplit_once('@').map_or(netloc, |(_, host)| host);

    // 7. Host, without the port.
    let host = match hostinfo.split_once('[') {
        Some((_, bracketed)) => bracketed.split_once(']').map_or(bracketed, |(h, _)| h),
        None => hostinfo.split_once(':').map_or(hostinfo, |(h, _)| h),
    };
    if host.is_empty() {
        return None;
    }

    // 8. Lowercase, preserving any IPv6 zone identifier.
    Some(match host.split_once('%') {
        Some((host, zone)) => format!("{}%{zone}", host.to_lowercase()),
        None => host.to_lowercase(),
    })
}

/// CPython's `urllib.parse._check_bracketed_netloc`, as a predicate.
///
/// ```python
/// def _check_bracketed_netloc(netloc):
///     hostname_and_port = netloc.rpartition('@')[2]
///     before_bracket, have_open_br, bracketed = hostname_and_port.partition('[')
///     if have_open_br:
///         if before_bracket:                       # no data before a bracket
///             raise ValueError("Invalid IPv6 URL")
///         hostname, _, port = bracketed.partition(']')
///         if port and not port.startswith(":"):    # nothing between ] and :
///             raise ValueError("Invalid IPv6 URL")
///     else:
///         hostname, _, port = hostname_and_port.partition(':')
///     _check_bracketed_host(hostname)
/// ```
///
/// The subtle half is the `else` branch, and it is worth spelling out because
/// it changed in CPython 3.11.4 / 3.12 and an implementation written against
/// the older code gets it backwards. The check runs whenever the *netloc*
/// contains brackets, but it splits off the userinfo **first**. So when the
/// last `@` sits after the bracket — `https://user@[::1]@evil.com` — the
/// bracket ends up inside the userinfo, `hostname_and_port` is the plain
/// `evil.com`, and `_check_bracketed_host("evil.com")` runs
/// `ipaddress.ip_address("evil.com")`, which raises. The whole URL is therefore
/// rejected, even though `SplitResult.hostname` would happily have answered
/// `evil.com`.
///
/// That is not a curiosity: it is the difference between filing a row under
/// `evil.com` and filing it under `default`, for a URL whose *own* host is a
/// perfectly ordinary domain. The fuzz corpus contains fourteen such URLs.
fn check_bracketed_netloc(netloc: &str) -> bool {
    let hostname_and_port = netloc.rsplit_once('@').map_or(netloc, |(_, rest)| rest);

    let hostname = match hostname_and_port.split_once('[') {
        Some((before_bracket, bracketed)) => {
            // No data is allowed before the bracket.
            if !before_bracket.is_empty() {
                return false;
            }
            let (hostname, port) = match bracketed.split_once(']') {
                Some((hostname, port)) => (hostname, port),
                // `"a[b".partition(']')` yields `("a[b", "", "")`: no closing
                // bracket means an empty port, not a failure. The caller has
                // already rejected unbalanced brackets anyway.
                None => (bracketed, ""),
            };
            // No data is allowed after the bracket but before the port.
            if !port.is_empty() && !port.starts_with(':') {
                return false;
            }
            hostname
        }
        None => hostname_and_port
            .split_once(':')
            .map_or(hostname_and_port, |(hostname, _)| hostname),
    };

    check_bracketed_host(hostname)
}

/// CPython's `urllib.parse._checknetloc`, as a predicate.
///
/// ```python
/// def _checknetloc(netloc):
///     if not netloc or netloc.isascii():
///         return
///     n = netloc.replace('@', '').replace(':', '').replace('#', '').replace('?', '')
///     netloc2 = unicodedata.normalize('NFKC', n)
///     if n == netloc2:
///         return
///     for c in '/?#@:':
///         if c in netloc2:
///             raise ValueError(...)
/// ```
///
/// The trap it closes is real: `U+FF20 FULLWIDTH COMMERCIAL AT` NFKC-normalises
/// to `@`, so `https://example.com＠evil.com` reaches an IDNA-normalising
/// resolver as `evil.com` while a naive splitter reads the whole thing as one
/// host. CPython refuses to parse such a URL at all, `tld` swallows the
/// `ValueError`, and the row lands in the `"default"` namespace.
///
/// Rather than depend on a normalisation crate for a nineteen-character
/// question, [`NFKC_DELIMITER_EXPANSIONS`] enumerates every code point whose
/// NFKC expansion contains one of `/?#@:`. The two formulations agree exactly:
/// a code point can only introduce a delimiter by decomposing (NFKC
/// *composition* only ever produces precomposed letters, never ASCII
/// punctuation), and any such code point also makes `n != netloc2`, which is
/// the guard Python tests first.
fn check_netloc(netloc: &str) -> bool {
    if netloc.is_ascii() {
        return true;
    }
    !netloc
        .chars()
        .any(|c| NFKC_DELIMITER_EXPANSIONS.contains(&c))
}

/// Every code point whose NFKC expansion contains `/`, `?`, `#`, `@` or `:`.
///
/// Enumerated over the whole of Unicode 16.0.0 (the version CPython 3.14
/// ships); there are exactly nineteen, and they are what
/// [`check_netloc`] rejects. Re-derive with:
///
/// ```python
/// [chr(c) for c in range(0x80, 0x110000)
///  if not unicodedata.is_normalized("NFKC", chr(c))
///  and any(d in unicodedata.normalize("NFKC", chr(c)) for d in "/?#@:")]
/// ```
const NFKC_DELIMITER_EXPANSIONS: [char; 19] = [
    '\u{2047}', // ⁇  DOUBLE QUESTION MARK            -> "??"
    '\u{2048}', // ⁈  QUESTION EXCLAMATION MARK       -> "?!"
    '\u{2049}', // ⁉  EXCLAMATION QUESTION MARK       -> "!?"
    '\u{2100}', // ℀  ACCOUNT OF                      -> "a/c"
    '\u{2101}', // ℁  ADDRESSED TO THE SUBJECT        -> "a/s"
    '\u{2105}', // ℅  CARE OF                         -> "c/o"
    '\u{2106}', // ℆  CADA UNA                        -> "c/u"
    '\u{2a74}', // ⩴  DOUBLE COLON EQUAL              -> "::="
    '\u{fe13}', // ︓ VERTICAL COLON                  -> ":"
    '\u{fe16}', // ︖ VERTICAL QUESTION MARK          -> "?"
    '\u{fe55}', // ﹕ SMALL COLON                     -> ":"
    '\u{fe56}', // ﹖ SMALL QUESTION MARK             -> "?"
    '\u{fe5f}', // ﹟ SMALL NUMBER SIGN               -> "#"
    '\u{fe6b}', // ﹫ SMALL COMMERCIAL AT             -> "@"
    '\u{ff03}', // ＃ FULLWIDTH NUMBER SIGN           -> "#"
    '\u{ff0f}', // ／ FULLWIDTH SOLIDUS               -> "/"
    '\u{ff1a}', // ： FULLWIDTH COLON                 -> ":"
    '\u{ff1f}', // ？ FULLWIDTH QUESTION MARK         -> "?"
    '\u{ff20}', // ＠ FULLWIDTH COMMERCIAL AT         -> "@"
];

/// CPython's `urllib.parse._check_bracketed_host`, as a predicate.
///
/// ```python
/// def _check_bracketed_host(hostname):
///     if hostname.startswith('v'):
///         if not re.match(r"\Av[a-fA-F0-9]+\..+\Z", hostname):
///             raise ValueError("IPvFuture address is invalid")
///     else:
///         ip = ipaddress.ip_address(hostname)   # ValueError if neither v4 nor v6
///         if isinstance(ip, ipaddress.IPv4Address):
///             raise ValueError("An IPv4 address cannot be in brackets")
/// ```
///
/// Returning `false` stands in for the raise, which `tld(fail_silently=True)`
/// swallows into `"default"`.
///
/// Skipping this check is not harmless, and not merely a matter of rejecting
/// junk. `https://[example.com]/x` has a *perfectly good* registrable domain if
/// the brackets are ignored, so a port without the check answers `example.com`
/// where Python answers `default` — two namespaces for one URL. The IPvFuture
/// branch cuts the other way: `\Av[a-fA-F0-9]+\..+\Z` happily matches
/// `v1.example.com`, so Python really does return `example.com` there, and
/// `[vff.co.uk]` really does resolve to `vff.co.uk`.
///
/// The IPv6 branch, by contrast, can only ever end in `"default"` — every valid
/// IPv6 literal either has no dots at all or ends in the decimal last octet of
/// an embedded IPv4 address, and neither is a public suffix. It is implemented
/// faithfully anyway so that [`split_host`] keeps its contract of *being*
/// `urlsplit(...).hostname`.
fn check_bracketed_host(hostname: &str) -> bool {
    if let Some(rest) = hostname.strip_prefix('v') {
        // `\Av[a-fA-F0-9]+\..+\Z`: at least one hex digit, a dot, then at least
        // one more character. `.` does not match a newline, but `urlsplit` has
        // already removed every `\n`.
        let Some((version, tail)) = rest.split_once('.') else {
            return false;
        };
        return !version.is_empty()
            && version.bytes().all(|b| b.is_ascii_hexdigit())
            && !tail.is_empty();
    }

    // `ipaddress.ip_address` tries IPv4 first, then IPv6; an IPv4 hit is
    // rejected outright, so only a valid IPv6 address survives.
    if hostname.parse::<std::net::Ipv4Addr>().is_ok() {
        return false;
    }
    // `ipaddress` accepts a scope id (`fe80::1%eth0`) and Rust's parser does
    // not, so it is split off first — under Python's rule that the scope must
    // be non-empty and hold no further `%`.
    let address = match hostname.split_once('%') {
        Some((_, zone)) if zone.is_empty() || zone.contains('%') => return false,
        Some((address, _)) => address,
        None => hostname,
    };
    address.parse::<std::net::Ipv6Addr>().is_ok()
}

/// The registrable domain of `url`, or [`DEFAULT_BASE_URL`].
///
/// This is `StorageSystemMixin._get_base_url()` with its default argument: the
/// value written into (and matched against) the `url` column of the `storage`
/// table.
///
/// ```
/// use browser_storage::base_url::registrable_domain;
///
/// assert_eq!(registrable_domain(Some("https://www.example.com/page")), "example.com");
/// assert_eq!(registrable_domain(Some("example.com")), "example.com");   // fix_protocol
/// assert_eq!(registrable_domain(Some("https://sub.a.co.uk/x")), "a.co.uk");
/// assert_eq!(registrable_domain(Some("http://localhost")), "default");  // unknown suffix
/// assert_eq!(registrable_domain(None), "default");
/// ```
#[must_use]
pub fn registrable_domain(url: Option<&str>) -> String {
    registrable_domain_with_default(url, DEFAULT_BASE_URL)
}

/// [`registrable_domain`] with an explicit fallback, mirroring
/// `_get_base_url(default_value=…)`.
///
/// Every failure mode collapses to `default_value`: no URL, an empty URL, a URL
/// with no host, or a host whose public suffix is not in the list.
#[must_use]
pub fn registrable_domain_with_default(url: Option<&str>, default_value: &str) -> String {
    let Some(url) = normalise_url(url) else {
        return default_value.to_owned();
    };

    let fixed = fix_protocol(&url);
    let Some(raw_host) = split_host(&fixed) else {
        return default_value.to_owned();
    };
    // `tld` strips *every* trailing dot before the trie walk:
    // `https://github.com........./x`.
    let host = raw_host.trim_end_matches('.');
    if host.is_empty() {
        return default_value.to_owned();
    }

    match first_level_domain(host, &raw_host) {
        // `extracted.fld or extracted.domain or default_value`. `fld` is never
        // empty when the lookup succeeds — it is at worst the suffix itself —
        // so the two later arms of the chain are unreachable, but they are kept
        // so the code reads like the Python it replaces.
        Some(fld) if !fld.is_empty() => fld,
        _ => default_value.to_owned(),
    }
}

/// The suffixes `tld` accepts and [`psl::suffix`] does not: the parents of the
/// Public Suffix List's exception rules.
///
/// This is an *algorithmic* difference between the two libraries, not a list
/// vintage one, and it is easy to miss because it only shows on a host that is
/// exactly the parent.
///
/// `tld` builds a trie by walking each rule's labels in reverse and marking the
/// node it stops on as a leaf. For an exception rule it stops early — the `!`
/// label sets `node.exception` and `break`s — so `!www.ck` marks the **`ck`**
/// node a leaf, and `!city.kawasaki.jp` marks **`kawasaki.jp`**:
///
/// ```python
/// for part in reversed(tld.split(".")):
///     if part.startswith("!"):
///         node.exception = part[1:]
///         break
///     ...
/// node.leaf = True          # <- `ck`, not `www.ck`
/// ```
///
/// So `get_tld("https://ck")` answers `ck`, and `https://kawasaki.jp.` answers
/// `kawasaki.jp.` (the un-stripped hostname, via the `hostname == tld` branch).
/// `psl` follows the PSL algorithm instead, under which `ck` matches no rule at
/// all — `psl::suffix(b"ck")` comes back `is_known() == false` — and
/// `kawasaki.jp` is an ordinary `.jp` domain. Without this table those hosts
/// land in the wrong namespace: `default` instead of `ck`, or `kawasaki.jp`
/// instead of `kawasaki.jp.`.
///
/// The list is every `!` line of the PSL with its leading label removed. It has
/// held these eight entries for over a decade;
/// `tests/base_url_exceptions.rs` pins it against both `psl`'s snapshot and
/// `tld`'s bundled file, so a rule added upstream fails loudly rather than
/// silently splitting a namespace.
const EXCEPTION_RULE_PARENTS: [&str; 8] = [
    "ck",            // !www.ck
    "kawasaki.jp",   // !city.kawasaki.jp
    "kitakyushu.jp", // !city.kitakyushu.jp
    "kobe.jp",       // !city.kobe.jp
    "nagoya.jp",     // !city.nagoya.jp
    "sapporo.jp",    // !city.sapporo.jp
    "sendai.jp",     // !city.sendai.jp
    "yokohama.jp",   // !city.yokohama.jp
];

/// `tld.Result.fld` for an already-normalised host, or `None` when the host's
/// public suffix is unknown (`TldDomainNotFound`, swallowed by
/// `fail_silently=True`).
///
/// `tld` walks its trie from the right and then slices the label list:
///
/// ```text
/// if len(domain_parts) == tld_length:  non_zero_i = -1        # host == suffix
/// else:                                non_zero_i = max(1, len(domain_parts) - tld_length)
///
/// domain = domain_parts[non_zero_i - 1]        # one label
/// tld    = ".".join(domain_parts[non_zero_i:])
/// fld    = f"{domain}.{tld}" if domain else tld
/// ```
///
/// The label arithmetic is reproduced rather than simply calling
/// [`psl::domain`] because of that last line: for a host that *is* a public
/// suffix (`com`, `co.uk`, `blogspot.com`) `psl::domain` returns `None` while
/// `tld` returns the suffix, and for a host with an empty leading label
/// (`https://.com`) `tld` returns the suffix too.
///
/// `raw_host` is the host *before* trailing dots are stripped, and it is not
/// decoration. `get_tld(as_object=True)` takes the `non_zero_i < 0` branch when
/// the host is exactly the suffix, and that branch reads `parsed_url.hostname`
/// — the un-stripped host — rather than the `domain_parts` the trie walked:
///
/// ```text
/// if non_zero_i < 0:            # hostname == tld
///     domain = ""
///     _tld   = parsed_url.hostname     # <- NOT rstrip(".")ed
/// ```
///
/// So `https://com.` namespaces its rows under `"com."`, dot included, while
/// `https://www.example.com.` (which takes the other branch, off the stripped
/// labels) uses `"example.com"`. The quirk is load-bearing, not cosmetic: it is
/// the `url` column value, and getting it wrong hides every row a Python run
/// wrote for such a URL.
fn first_level_domain(host: &str, raw_host: &str) -> Option<String> {
    // `tld` treats the parent of an exception rule as a suffix in its own
    // right; `psl` does not. See `EXCEPTION_RULE_PARENTS`.
    if EXCEPTION_RULE_PARENTS.contains(&host) {
        return Some(raw_host.to_owned());
    }

    let suffix = psl::suffix(host.as_bytes())?;
    // An unknown suffix is `psl`'s way of saying "not in the list"; `tld`
    // raises `TldDomainNotFound` for the same input.
    if !suffix.is_known() {
        return None;
    }
    let suffix = std::str::from_utf8(suffix.trim().as_bytes()).ok()?;

    let labels: Vec<&str> = host.split('.').collect();
    let suffix_len = suffix.split('.').count();
    if labels.len() <= suffix_len {
        // The host is exactly the public suffix: `fld == tld`, and `tld` sources
        // that `tld` from the *raw* hostname. See the note above.
        return Some(raw_host.to_owned());
    }

    let boundary = labels.len() - suffix_len; // == max(1, …); the `<=` arm covers the rest.
    let domain_label = labels[boundary - 1];
    let tld = labels[boundary..].join(".");
    if domain_label.is_empty() {
        Some(tld)
    } else {
        Some(format!("{domain_label}.{tld}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_url_lowercases_the_whole_url() {
        assert_eq!(
            normalise_url(Some("HTTPS://WWW.EXAMPLE.COM/Page?Q=A#F")).as_deref(),
            Some("https://www.example.com/page?q=a#f"),
        );
    }

    #[test]
    fn normalise_url_collapses_empty_to_none() {
        assert_eq!(normalise_url(Some("")), None);
        assert_eq!(normalise_url(None), None);
    }

    #[test]
    fn fix_protocol_prepends_https_only_when_needed() {
        assert_eq!(fix_protocol("example.com"), "https://example.com");
        assert_eq!(fix_protocol("https://example.com"), "https://example.com");
        // Scheme-relative URLs already match `protocol_re`.
        assert_eq!(fix_protocol("//example.com/x"), "//example.com/x");
        // A `//` further into the path does not count.
        assert_eq!(fix_protocol("example.com/a//b"), "https://example.com/a//b");
        // Any scheme, not just http(s).
        assert_eq!(fix_protocol("ftp://example.org/a"), "ftp://example.org/a");
    }

    #[test]
    fn split_host_matches_urlsplit() {
        assert_eq!(
            split_host("https://example.com/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            split_host("https://EXAMPLE.com").as_deref(),
            Some("example.com")
        );
        assert_eq!(
            split_host("https://user:pass@www.example.co.jp:8443/p?q=1#f").as_deref(),
            Some("www.example.co.jp"),
        );
        assert_eq!(split_host("https://[::1]:443/x").as_deref(), Some("::1"));
        assert_eq!(
            split_host("//example.com/x").as_deref(),
            Some("example.com")
        );
        assert_eq!(split_host("https://").as_deref(), None);
        assert_eq!(split_host("http:///path").as_deref(), None);
        assert_eq!(split_host("/just/a/path").as_deref(), None);
        // Unbalanced bracket: `urlsplit` raises, `tld` swallows.
        assert_eq!(split_host("https://[::1").as_deref(), None);
        // Control characters are removed, not rejected.
        assert_eq!(
            split_host("  https://exa\tmp\nle.com/x").as_deref(),
            Some("example.com"),
        );
        // A digit-led "scheme" is not a scheme.
        assert_eq!(split_host("3://example.com").as_deref(), None);
    }

    #[test]
    fn a_bracket_inside_the_userinfo_rejects_the_whole_url() {
        // CPython validates the netloc *after* stripping userinfo, so the
        // bracket lands in the userinfo and `_check_bracketed_host` runs on the
        // plain host, where `ipaddress.ip_address` raises.
        assert_eq!(split_host("https://user@[::1]@evil.com/x"), None);
        assert_eq!(split_host("https://[::1]:80@evil.com/x"), None);
        assert_eq!(split_host("https://[v1.example.com]@evil.com"), None);
        // Without the trailing `@` the same netloc is a perfectly good URL.
        assert_eq!(
            split_host("https://user@[::1]:80/x").as_deref(),
            Some("::1")
        );
        // No data before the bracket…
        assert_eq!(split_host("https://a[::1]/x"), None);
        // …and none between `]` and the port.
        assert_eq!(split_host("https://[::1]x:80/y"), None);
        assert_eq!(split_host("https://[::1]:80/y").as_deref(), Some("::1"));
    }

    #[test]
    fn a_netloc_that_nfkc_expands_into_a_delimiter_is_rejected() {
        // `U+FF20 FULLWIDTH COMMERCIAL AT` normalises to `@`, so CPython
        // refuses to parse the URL at all rather than let the host be read two
        // ways.
        assert_eq!(split_host("https://example.com\u{ff20}evil.com/x"), None);
        assert_eq!(split_host("https://a\u{ff0f}b.com"), None);
        assert_eq!(split_host("https://a\u{2100}b.com"), None);
        assert_eq!(split_host("https://a\u{fe6b}b.com"), None);
        // Non-ASCII that does not expand into a delimiter is fine.
        assert_eq!(
            split_host("https://\u{4f8b}\u{3048}.jp/x").as_deref(),
            Some("\u{4f8b}\u{3048}.jp")
        );
        assert_eq!(
            split_host("https://caf\u{e9}.com").as_deref(),
            Some("caf\u{e9}.com")
        );
        // The delimiters only matter in the netloc; a path may hold anything.
        assert_eq!(
            split_host("https://example.com/\u{ff20}x").as_deref(),
            Some("example.com"),
        );
    }

    #[test]
    fn the_parent_of_an_exception_rule_is_a_suffix_of_its_own() {
        // `!www.ck` makes `ck` a leaf in `tld`'s trie; `psl` disagrees.
        assert_eq!(registrable_domain(Some("https://ck")), "ck");
        assert_eq!(
            registrable_domain(Some("https://kawasaki.jp")),
            "kawasaki.jp"
        );
        // The `hostname == tld` branch reads the un-stripped hostname.
        assert_eq!(registrable_domain(Some("https://ck.")), "ck.");
        assert_eq!(
            registrable_domain(Some("https://kawasaki.jp...")),
            "kawasaki.jp...",
        );
        // Everything around them is unchanged.
        assert_eq!(registrable_domain(Some("https://www.ck")), "www.ck");
        assert_eq!(registrable_domain(Some("https://foo.ck")), "foo.ck");
        assert_eq!(registrable_domain(Some("https://a.b.ck")), "a.b.ck");
        assert_eq!(
            registrable_domain(Some("https://city.kawasaki.jp")),
            "city.kawasaki.jp",
        );
        // A wildcard TLD with no exception rule stays unknown, as in `tld`.
        assert_eq!(registrable_domain(Some("https://er")), "default");
        assert_eq!(registrable_domain(Some("https://np")), "default");
    }

    #[test]
    fn check_bracketed_host_mirrors_cpython() {
        // Valid IPv6, with and without a zone.
        assert!(check_bracketed_host("::1"));
        assert!(check_bracketed_host("2001:db8::1"));
        assert!(check_bracketed_host("0:0:0:0:0:0:0:1"));
        assert!(check_bracketed_host("::ffff:127.0.0.1"));
        assert!(check_bracketed_host("fe80::1%eth0"));
        // Malformed IPv6.
        assert!(!check_bracketed_host("::00001"));
        assert!(!check_bracketed_host("12345::1"));
        assert!(!check_bracketed_host(""));
        // A bare IPv4 is explicitly rejected rather than merely unparsed.
        assert!(!check_bracketed_host("1.2.3.4"));
        // Zone ids: must be non-empty and hold no second `%`.
        assert!(!check_bracketed_host("fe80::1%"));
        assert!(!check_bracketed_host("::1%a%b"));
        // IPvFuture: `v<hex>+.<anything>`.
        assert!(check_bracketed_host("v1.example.com"));
        assert!(check_bracketed_host("vff.co.uk"));
        assert!(check_bracketed_host("v1.a+b"));
        assert!(!check_bracketed_host("v.x"), "the hex run may not be empty");
        assert!(!check_bracketed_host("vz.x"), "z is not a hex digit");
        assert!(!check_bracketed_host("v1"), "a dot is required");
        assert!(
            !check_bracketed_host("v1."),
            "something must follow the dot"
        );
        // A hostname that only *looks* fine unbracketed.
        assert!(!check_bracketed_host("example.com"));
        assert!(!check_bracketed_host("localhost"));
    }

    #[test]
    fn a_bracketed_domain_is_not_a_host() {
        // Without CPython's `_check_bracketed_host`, these would resolve to a
        // perfectly plausible — and completely wrong — namespace.
        for url in [
            "https://[example.com]/x",
            "https://[example.com]:8080/x",
            "https://user@[example.co.uk]/x",
            "https://[localhost]/x",
            "https://[1.2.3.4]/x",
        ] {
            assert_eq!(registrable_domain(Some(url)), "default", "{url}");
        }
        // But an IPvFuture literal really does carry a registrable domain.
        assert_eq!(
            registrable_domain(Some("https://[v1.example.com]/x")),
            "example.com",
        );
        assert_eq!(
            registrable_domain(Some("https://[vff.co.uk]/x")),
            "vff.co.uk"
        );
    }

    #[test]
    fn split_host_keeps_an_ipv6_zone_verbatim() {
        assert_eq!(
            split_host("https://[FE80::1%eth0]/x").as_deref(),
            Some("fe80::1%eth0"),
        );
    }

    #[test]
    fn registrable_domain_pins_the_python_test_suite() {
        // tests/core/test_storage_core.py::TestGetBaseUrl
        assert_eq!(registrable_domain(None), "default");
        assert_eq!(registrable_domain(Some("")), "default");
        assert_eq!(
            registrable_domain(Some("https://www.example.com/page")),
            "example.com",
        );
    }

    #[test]
    fn registrable_domain_handles_multi_label_suffixes() {
        assert_eq!(registrable_domain(Some("https://sub.a.co.uk/x")), "a.co.uk");
        assert_eq!(
            registrable_domain(Some("https://a.b.c.example.co.uk")),
            "example.co.uk",
        );
    }

    #[test]
    fn registrable_domain_honours_the_private_section() {
        // `blogspot.com` is a *private* public suffix, so each blog is its own
        // registrable domain — the namespaces of two blogs must not merge.
        assert_eq!(
            registrable_domain(Some("https://a.blogspot.com")),
            "a.blogspot.com",
        );
        assert_eq!(
            registrable_domain(Some("https://x.y.blogspot.com")),
            "y.blogspot.com",
        );
    }

    #[test]
    fn registrable_domain_returns_the_suffix_when_the_host_is_one() {
        assert_eq!(registrable_domain(Some("https://com")), "com");
        assert_eq!(registrable_domain(Some("https://co.uk")), "co.uk");
        // An empty leading label leaves `tld`'s `domain` empty, so `fld` is the
        // suffix alone.
        assert_eq!(registrable_domain(Some("https://.com")), "com");
    }

    #[test]
    fn registrable_domain_strips_trailing_dots() {
        assert_eq!(
            registrable_domain(Some("https://www.example.com...")),
            "example.com",
        );
    }

    #[test]
    fn registrable_domain_falls_back_for_unknown_suffixes() {
        for url in [
            "http://localhost",
            "localhost",
            "https://example.invalidtld/",
            "http://127.0.0.1:8080/x",
            "https://[::1]/x",
            "not a url",
            "https://",
            " ",
        ] {
            assert_eq!(registrable_domain(Some(url)), "default", "{url}");
        }
    }

    #[test]
    fn registrable_domain_does_not_punycode_idn_hosts() {
        // The `url` crate would answer `xn--r8jz45g.jp` here; `tld` does not,
        // and the namespace key has to match a Python-written database.
        assert_eq!(registrable_domain(Some("https://例え.jp/")), "例え.jp");
    }

    #[test]
    fn registrable_domain_with_default_uses_the_supplied_fallback() {
        assert_eq!(
            registrable_domain_with_default(None, "fallback"),
            "fallback"
        );
        assert_eq!(
            registrable_domain_with_default(Some("https://example.com"), "fallback"),
            "example.com",
        );
    }
}
