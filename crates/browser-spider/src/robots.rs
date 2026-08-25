//! `RobotsTxtManager`: a netloc-keyed cache, the "any failure degrades to fully permissive"
//! rule, and the `Request-rate` pre-scan.
//!
//! Port of `reference/spiders/robotstxt.py` (spec: `docs/specs/spider-support.md` §6),
//! **including** the `protego` parser it delegates to, because that parser's exact
//! behaviour is the observable contract (§6.5) and no Rust crate reproduces it.
//!
//! # Why the parser is ported rather than borrowed
//!
//! `texting_robots` — the obvious candidate, and still a workspace dependency for the
//! sitemap templates — diverges from `protego` in ways this subsystem would notice:
//!
//! | Behaviour | `protego` (what the reference implementation does) | `texting_robots` |
//! |---|---|---|
//! | `Request-rate` | parsed, with `s`/`m`/`h`/`d` units and an optional time window | not parsed at all |
//! | `Disallow: /robots.txt` | honoured | `/robots.txt` is hard-coded as always allowed |
//! | `Allow: /a/index.html` | also allows `/a/` exactly | no such rule |
//! | Rule precedence | longest **quoted** pattern wins, `Allow` breaks ties | longest raw pattern |
//! | `$` in a pattern | matched literally *and* as an end anchor (`%24` twin rule) | end anchor only |
//! | Agent groups | `*`-stripped aliases also register (`Googlebot-*` ⇒ `googlebot-`) | exact match only |
//!
//! [`RobotsRules`] is therefore a direct port of `protego/_protego.py`,
//! `protego/_ruleset.py`, `protego/_urlpattern.py` and `protego/_utils.py`, checked against
//! the real thing by `tests/u16_robots_golden.rs`.
//!
//! # The fetch seam
//!
//! Python hands `RobotsTxtManager` a `fetch_fn: Callable[[str, str], Awaitable]`. Work unit
//! 12's HTTP client does not exist yet and work unit 17 owns the wiring, so the manager
//! depends on the [`RobotsFetcher`] trait instead — one method, `(url, sid) -> FetchedResponse`.
//! The engine will implement it over `SessionManager`; tests implement it over a closure
//! with [`fetcher_fn`].

use std::future::Future;
use std::sync::Arc;

use async_trait::async_trait;
use dashmap::DashMap;
use indexmap::IndexMap;

use crate::canonicalize::urlparse;
use crate::error::Result;
use crate::session::FetchedResponse;
use crate::throttle::{python_float, python_int, python_max, python_strip};

// ---------------------------------------------------------------------------
// Directive vocabulary — protego/_protego.py
// ---------------------------------------------------------------------------

/// `protego`'s tolerated misspellings of `Disallow`. Reproduced verbatim: a site that
/// writes `Dissallow:` really is obeyed.
const DISALLOW_DIRECTIVE: &[&str] = &[
    "disallow",
    "dissallow",
    "dissalow",
    "disalow",
    "diasllow",
    "disallaw",
];
const ALLOW_DIRECTIVE: &[&str] = &["allow"];
const USER_AGENT_DIRECTIVE: &[&str] = &["user-agent", "useragent", "user agent"];
const SITEMAP_DIRECTIVE: &[&str] = &["sitemap", "sitemaps", "site-map"];
const CRAWL_DELAY_DIRECTIVE: &[&str] = &["crawl-delay", "crawl delay"];
const REQUEST_RATE_DIRECTIVE: &[&str] = &["request-rate", "request rate"];
const VISIT_TIME_DIRECTIVE: &[&str] = &["visit-time", "visit time"];
const HOST_DIRECTIVE: &[&str] = &["host"];

/// `protego._is_valid_directive_field` — note that `visit-time` is deliberately **not** in
/// this list, so the whitespace-separated fallback (`"Disallow /admin"`) never recognises it.
fn is_valid_directive_field(field: &str) -> bool {
    DISALLOW_DIRECTIVE.contains(&field)
        || ALLOW_DIRECTIVE.contains(&field)
        || USER_AGENT_DIRECTIVE.contains(&field)
        || SITEMAP_DIRECTIVE.contains(&field)
        || CRAWL_DELAY_DIRECTIVE.contains(&field)
        || REQUEST_RATE_DIRECTIVE.contains(&field)
        || HOST_DIRECTIVE.contains(&field)
}

// ---------------------------------------------------------------------------
// Value types
// ---------------------------------------------------------------------------

/// An hour/minute pair, `protego`'s use of `datetime.time`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TimeOfDay {
    /// Hour, `0..=23`.
    pub hour: u8,
    /// Minute, `0..=59`.
    pub minute: u8,
}

/// A `Request-rate` directive: `requests` requests per `seconds` seconds.
///
/// `Request-rate: 1/10` is `RequestRate { requests: 1, seconds: 10, .. }`; the unit suffixes
/// `m`, `h` and `d` are normalised into seconds, so `1/5m` is `seconds: 300`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestRate {
    /// How many requests are permitted in the window. May be zero or negative: `protego`
    /// stores whatever `int()` returned, and the engine guards with `requests > 0`.
    pub requests: i64,
    /// The window, in seconds.
    pub seconds: i64,
    /// Start of the optional time-of-day window (`Request-rate: 1/10 0600-0845`).
    pub start_time: Option<TimeOfDay>,
    /// End of the optional time-of-day window.
    pub end_time: Option<TimeOfDay>,
}

/// A `Visit-time` directive. Parsed for completeness; the reference implementation never reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VisitTime {
    /// Start of the window.
    pub start_time: TimeOfDay,
    /// End of the window.
    pub end_time: TimeOfDay,
}

// ---------------------------------------------------------------------------
// URL patterns — protego/_urlpattern.py
// ---------------------------------------------------------------------------

/// One `Allow:` / `Disallow:` pattern, precompiled the way `protego._URLPattern` is.
#[derive(Clone, Debug)]
struct UrlPattern {
    pattern: String,
    /// `len(pattern)` — the sort key that decides which rule wins.
    priority: usize,
    contains_asterisk: bool,
    contains_dollar: bool,
    /// Everything before the first `*`; the URL must start with it.
    before_asterisk: String,
    /// `pattern[:-1]`, only meaningful when there is a `$` and no `*`.
    before_dollar: String,
    /// The pattern with runs of `*` collapsed and the trailing `$` stripped, split on `*`.
    wildcard_parts: Vec<String>,
    wildcard_ends_with_dollar: bool,
}

impl UrlPattern {
    fn new(pattern: String) -> Self {
        let priority = pattern.chars().count();
        let contains_asterisk = pattern.contains('*');
        let contains_dollar = pattern.ends_with('$');
        let before_asterisk = match pattern.find('*') {
            Some(index) => pattern[..index].to_owned(),
            None => String::new(),
        };
        let before_dollar = if contains_dollar {
            pattern[..pattern.len() - 1].to_owned()
        } else {
            String::new()
        };
        let (wildcard_parts, wildcard_ends_with_dollar) = prepare_wildcard_parts(&pattern);
        Self {
            pattern,
            priority,
            contains_asterisk,
            contains_dollar,
            before_asterisk,
            before_dollar,
            wildcard_parts,
            wildcard_ends_with_dollar,
        }
    }

    fn matches(&self, url: &str) -> bool {
        if !self.contains_asterisk {
            if !self.contains_dollar {
                return url.starts_with(&self.pattern);
            }
            return url == self.before_dollar;
        }
        if !url.starts_with(&self.before_asterisk) {
            return false;
        }
        self.match_wildcard(url)
    }

    /// `protego._URLPattern._match_wildcard`, byte-for-byte.
    ///
    /// Both the pattern and the URL are pure ASCII by the time they get here — `quote()`
    /// percent-encodes everything else — so Python's character indices and Rust's byte
    /// indices agree.
    fn match_wildcard(&self, url: &str) -> bool {
        let parts = &self.wildcard_parts;
        if parts.is_empty() {
            return true;
        }
        let mut pos = parts[0].len();
        if pos > url.len() {
            return false;
        }

        // Python's `parts[1:-1]` is empty rather than a panic when there are fewer than
        // two elements.
        let middle = if parts.len() > 1 {
            &parts[1..parts.len() - 1]
        } else {
            &[][..]
        };
        for part in middle {
            match url.get(pos..).and_then(|rest| rest.find(part.as_str())) {
                Some(index) => pos += index + part.len(),
                None => return false,
            }
        }

        let last = &parts[parts.len() - 1];
        if self.wildcard_ends_with_dollar {
            let Some(end_pos) = url.len().checked_sub(last.len()) else {
                return false;
            };
            return end_pos >= pos && url.get(end_pos..) == Some(last.as_str());
        }
        if last.is_empty() {
            return true;
        }
        url.get(pos..)
            .is_some_and(|rest| rest.contains(last.as_str()))
    }
}

/// `protego._URLPattern._prepare_wildcard_parts`: strip a trailing `$`, collapse `*+` to a
/// single `*`, split on `*`.
fn prepare_wildcard_parts(pattern: &str) -> (Vec<String>, bool) {
    let ends_with_dollar = pattern.ends_with('$');
    let body = if ends_with_dollar {
        &pattern[..pattern.len() - 1]
    } else {
        pattern
    };
    let mut collapsed = String::with_capacity(body.len());
    let mut previous_was_star = false;
    for ch in body.chars() {
        if ch == '*' {
            if previous_was_star {
                continue;
            }
            previous_was_star = true;
        } else {
            previous_was_star = false;
        }
        collapsed.push(ch);
    }
    (
        collapsed.split('*').map(str::to_owned).collect(),
        ends_with_dollar,
    )
}

// ---------------------------------------------------------------------------
// Percent-encoding helpers — protego/_utils.py
// ---------------------------------------------------------------------------

/// `protego._hexescape`.
fn hexescape(ch: char) -> String {
    format!("%{:02X}", ch as u32)
}

/// `protego._unquote(url, ignore, errors="replace")`.
///
/// Decodes `%xy` escapes back to bytes, leaving alone the escapes whose hex code names a
/// character in `ignore`, then decodes the result as UTF-8 with replacement. An escape that
/// is not two hex digits is preserved verbatim, `%` included.
fn protego_unquote(url: &str, ignore: &str) -> String {
    if !url.contains('%') {
        return url.to_owned();
    }
    let ignore_set: Vec<String> = ignore
        .chars()
        .map(|ch| format!("{:02X}", ch as u32))
        .collect();

    let mut parts = url.split('%');
    let mut out: Vec<u8> = parts.next().unwrap_or_default().as_bytes().to_vec();

    for part in parts {
        let head: String = part.chars().take(2).collect();
        let is_escape = head.chars().count() == 2 && head.chars().all(|c| c.is_ascii_hexdigit());
        if is_escape {
            let hexcode = head.to_ascii_uppercase();
            let leftover: String = part.chars().skip(2).collect();
            if !ignore_set.contains(&hexcode) {
                let byte = u8::from_str_radix(&hexcode, 16).unwrap_or(b'%');
                out.push(byte);
                out.extend_from_slice(leftover.as_bytes());
                continue;
            }
            out.push(b'%');
            out.extend_from_slice(hexcode.as_bytes());
            out.extend_from_slice(leftover.as_bytes());
            continue;
        }
        out.push(b'%');
        out.extend_from_slice(part.as_bytes());
    }

    String::from_utf8_lossy(&out).into_owned()
}

/// CPython's `urllib.parse.quote(string, safe=...)`.
///
/// The always-safe set is ASCII alphanumerics plus `_.-~`; `safe` adds to it. Escapes are
/// upper-case hex over the UTF-8 encoding.
fn quote(text: &str, safe: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(text.len());
    for &byte in text.as_bytes() {
        let always_safe = byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-' | b'~');
        if always_safe || (byte < 128 && safe.as_bytes().contains(&byte)) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[usize::from(byte >> 4)] as char);
            out.push(HEX[usize::from(byte & 0x0f)] as char);
        }
    }
    out
}

/// `urllib.parse.urlunparse(ParseResult("", "", path, params, query, fragment))`.
///
/// With an empty scheme and netloc the only surprise is that a path already starting with
/// `//` gets a second `//` so it cannot be re-read as an authority.
fn urlunparse_pathonly(path: &str, params: &str, query: &str, fragment: &str) -> String {
    let mut url = if params.is_empty() {
        path.to_owned()
    } else {
        format!("{path};{params}")
    };
    if url.starts_with("//") {
        url.insert_str(0, "//");
    }
    if !query.is_empty() {
        url.push('?');
        url.push_str(query);
    }
    if !fragment.is_empty() {
        url.push('#');
        url.push_str(fragment);
    }
    url
}

/// `protego._quote_path` — the form a URL is put into before it is matched.
fn quote_path(path: &str) -> String {
    let parts = urlparse(path);
    let unquoted = protego_unquote(&parts.path, "/%");
    let quoted = quote(&unquoted, "/%");
    let rebuilt = urlunparse_pathonly(&quoted, &parts.params, &parts.query, &parts.fragment);
    if rebuilt.is_empty() {
        "/".to_owned()
    } else {
        rebuilt
    }
}

/// `protego._quote_pattern` — the form an `Allow:`/`Disallow:` value is stored in.
///
/// Returns an empty string for a pattern that quotes away to nothing, which the caller
/// treats as "drop this rule".
fn quote_pattern(pattern: &str) -> String {
    let mut pattern = pattern.to_owned();
    if pattern.starts_with("https://") || pattern.starts_with("http://") {
        pattern.insert(0, '/');
    }
    if pattern.starts_with("//") {
        pattern.insert_str(0, "//");
    }

    // Corner case for query-only (`/abc?`), param-only (`/abc;`) and anchored (`/abc$`)
    // patterns: `urlparse` would eat the last character, so it is saved and re-appended.
    let mut last_char = String::new();
    if let Some(ch) = pattern.chars().next_back() {
        if matches!(ch, '?' | ';' | '$') {
            last_char.push(ch);
            pattern.truncate(pattern.len() - ch.len_utf8());
        }
    }

    let parts = urlparse(&pattern);
    let unquoted = protego_unquote(&parts.path, "/*$%");
    let quoted = quote(&unquoted, "/*%=");
    urlunparse_pathonly(
        &format!("{quoted}{last_char}"),
        &parts.params,
        &parts.query,
        &parts.fragment,
    )
}

/// `protego._parse_time_period`.
///
/// `time(int(s[:2]), int(s[-2:]))` — note that a one- or two-character half reuses the same
/// digits for hour and minute, which is `protego`'s behaviour, not a typo.
fn parse_time_period(period: &str, separator: char) -> Option<(TimeOfDay, TimeOfDay)> {
    let halves: Vec<&str> = period.split(separator).collect();
    if halves.len() != 2 {
        return None;
    }
    let read = |half: &str| -> Option<TimeOfDay> {
        let chars: Vec<char> = half.chars().collect();
        let head: String = chars.iter().take(2).collect();
        let tail: String = chars
            .iter()
            .skip(chars.len().saturating_sub(2))
            .collect::<String>();
        let hour = python_int(&head)?;
        let minute = python_int(&tail)?;
        if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) {
            return None;
        }
        Some(TimeOfDay {
            hour: u8::try_from(hour).ok()?,
            minute: u8::try_from(minute).ok()?,
        })
    };
    Some((read(halves[0])?, read(halves[1])?))
}

/// `protego._RuleSet.request_rate` setter — the `Request-rate` pre-scan.
///
/// `"1/10"`, `"1/5m"`, `"1/10 0600-0845"`. A malformed value is ignored outright, exactly
/// like the `except Exception: return` in `protego`.
fn parse_request_rate(value: &str) -> Option<RequestRate> {
    let fields: Vec<&str> = value.split_whitespace().collect();
    let (rate, time_period) = match fields.len() {
        2 => (fields[0], fields[1]),
        0 => return None,
        _ => (fields[0], ""),
    };

    let (requests_str, seconds_str) = {
        let mut halves = rate.split('/');
        let first = halves.next()?;
        let second = halves.next()?;
        if halves.next().is_some() {
            // Python's `a, b = rate.split("/")` raises on three or more parts.
            return None;
        }
        (first, second)
    };

    let unit = seconds_str.chars().next_back()?.to_ascii_lowercase();
    let (unit, seconds_digits) = if matches!(unit, 's' | 'm' | 'h' | 'd') {
        let trimmed = &seconds_str[..seconds_str.len() - unit.len_utf8()];
        (unit, trimmed)
    } else {
        ('s', seconds_str)
    };

    let mut seconds = python_int(seconds_digits)?;
    let requests = python_int(requests_str)?;
    seconds = match unit {
        'm' => seconds.checked_mul(60)?,
        'h' => seconds.checked_mul(3600)?,
        'd' => seconds.checked_mul(86400)?,
        _ => seconds,
    };

    let (start_time, end_time) = if time_period.is_empty() {
        (None, None)
    } else {
        let (start, end) = parse_time_period(time_period, '-')?;
        (Some(start), Some(end))
    };

    Some(RequestRate {
        requests,
        seconds,
        start_time,
        end_time,
    })
}

/// CPython's `str.splitlines`, which breaks on far more than `\n` and `\r\n`.
///
/// `robots.txt` in the wild does contain lone `\r`s and the odd `\x0c`, and `protego` sees
/// them as line breaks, so `str::lines` would merge two directives into one.
fn python_splitlines(text: &str) -> Vec<&str> {
    const BREAKS: &[char] = &[
        '\n', '\r', '\u{0b}', '\u{0c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}',
        '\u{2029}',
    ];
    let mut lines = Vec::new();
    let mut start = 0usize;
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < text.len() {
        let Some(ch) = text[index..].chars().next() else {
            break;
        };
        let width = ch.len_utf8();
        if BREAKS.contains(&ch) {
            lines.push(&text[start..index]);
            index += width;
            // `\r\n` is one break, not two.
            if ch == '\r' && bytes.get(index) == Some(&b'\n') {
                index += 1;
            }
            start = index;
            continue;
        }
        index += width;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

// ---------------------------------------------------------------------------
// Rule sets — protego/_ruleset.py
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuleField {
    Allow,
    Disallow,
}

#[derive(Clone, Debug)]
struct Rule {
    field: RuleField,
    value: UrlPattern,
}

#[derive(Clone, Debug)]
struct RuleSet {
    user_agent: String,
    rules: Vec<Rule>,
    crawl_delay: Option<f64>,
    request_rate: Option<RequestRate>,
    visit_time: Option<VisitTime>,
}

impl RuleSet {
    fn new(user_agent: String) -> Self {
        Self {
            user_agent,
            rules: Vec::new(),
            crawl_delay: None,
            request_rate: None,
            visit_time: None,
        }
    }

    /// `protego._RuleSet.applies_to` — `*` scores 1, an exact-or-substring agent scores its
    /// own length, anything else scores 0.
    fn applies_to(&self, robot_name: &str) -> usize {
        let robot_name = python_strip(robot_name).to_lowercase();
        if self.user_agent == "*" {
            return 1;
        }
        if robot_name.contains(&self.user_agent) {
            return self.user_agent.chars().count();
        }
        0
    }

    fn allow(&mut self, pattern: &str) {
        if pattern.contains('$') {
            let escaped = pattern.replace('$', &hexescape('$'));
            self.allow(&escaped);
        }
        let quoted = quote_pattern(pattern);
        if quoted.is_empty() {
            return;
        }
        // Allowing `/a/index.html` is read as allowing `/a/` exactly. `[:-10]` keeps the
        // trailing slash: `"/a/index.html"[:-10] == "/a/"`.
        let directory = quoted
            .strip_suffix("index.html")
            .filter(|_| quoted.ends_with("/index.html"))
            .map(str::to_owned);
        self.rules.push(Rule {
            field: RuleField::Allow,
            value: UrlPattern::new(quoted),
        });
        if let Some(directory) = directory {
            self.allow(&format!("{directory}$"));
        }
    }

    fn disallow(&mut self, pattern: &str) {
        if pattern.contains('$') {
            let escaped = pattern.replace('$', &hexescape('$'));
            self.disallow(&escaped);
        }
        let quoted = quote_pattern(pattern);
        if quoted.is_empty() {
            return;
        }
        self.rules.push(Rule {
            field: RuleField::Disallow,
            value: UrlPattern::new(quoted),
        });
    }

    /// `sort(key=lambda r: (r.value.priority, r.field == "allow"), reverse=True)`.
    ///
    /// Python's sort is stable and `reverse=True` reverses the comparison, not the input,
    /// so equal keys keep insertion order — `sort_by` here does the same.
    fn finalize_rules(&mut self) {
        self.rules.sort_by(|left, right| {
            let key = |rule: &Rule| (rule.value.priority, rule.field == RuleField::Allow);
            key(right).cmp(&key(left))
        });
    }

    fn can_fetch(&self, url: &str) -> bool {
        let url = quote_path(url);
        for rule in &self.rules {
            if rule.value.matches(&url) {
                return rule.field != RuleField::Disallow;
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// The parser — protego/_protego.py
// ---------------------------------------------------------------------------

/// A parsed `robots.txt`, behaviourally equivalent to a `protego.Protego`.
///
/// Build one with [`RobotsRules::parse`]. An empty or unparseable document produces an
/// empty rule set, which allows everything — the degradation rule the whole subsystem
/// leans on.
///
/// ```
/// use browser_spider::robots::RobotsRules;
///
/// let rules = RobotsRules::parse("User-agent: *\nDisallow: /admin/\nCrawl-delay: 2\n");
/// assert!(!rules.can_fetch("https://example.com/admin/users", "*"));
/// assert!(rules.can_fetch("https://example.com/products", "*"));
/// assert_eq!(rules.crawl_delay("*"), Some(2.0));
/// ```
#[derive(Clone, Debug, Default)]
pub struct RobotsRules {
    /// Insertion-ordered, because Python's `max(...)` over `dict.values()` returns the
    /// first maximum and the tie-break is therefore declaration order.
    user_agents: IndexMap<String, usize>,
    rule_sets: Vec<RuleSet>,
    sitemaps: Vec<String>,
    host: Option<String>,
}

impl RobotsRules {
    /// `Protego.parse(content)`.
    ///
    /// Never fails: `protego` is deliberately forgiving, and the reference implementation falls back to
    /// `Protego.parse("")` if it ever were to raise.
    #[must_use]
    pub fn parse(content: &str) -> Self {
        let mut this = Self::default();
        this.parse_into(content);
        for rule_set in &mut this.rule_sets {
            rule_set.finalize_rules();
        }
        this
    }

    #[allow(clippy::too_many_lines)]
    fn parse_into(&mut self, content: &str) {
        // Indices into `self.rule_sets` for the group currently being read.
        let mut current: Vec<usize> = Vec::new();
        let mut previous_rule_field: Option<String> = None;

        for raw_line in python_splitlines(content) {
            // Everything from the first `#` is a comment. Python strips twice — once after
            // cutting the comment and once for the whole line — which is the same result.
            let without_comment = match raw_line.find('#') {
                Some(hash) => &raw_line[..hash],
                None => raw_line,
            };
            let line = python_strip(without_comment);
            if line.is_empty() {
                continue;
            }

            // `"<field>:<value>"`, or a whitespace-separated fallback for `"Disallow /x"`.
            let (field, value) = match line.find(':') {
                Some(colon) => (line[..colon].to_owned(), line[colon + 1..].to_owned()),
                None => match split_on_spaces(line) {
                    Some(pair) => pair,
                    None => continue,
                },
            };

            let field = python_strip(&field).to_lowercase();
            let value = python_strip(&value).to_owned();

            // "Ignore rules with no value part" — but the field still counts as the
            // previous one, which is what ends a user-agent group.
            if value.is_empty() {
                previous_rule_field = Some(field);
                continue;
            }

            // A rule with no user agent to enforce it on is dropped, and — note — does
            // *not* update `previous_rule_field`.
            if current.is_empty()
                && !USER_AGENT_DIRECTIVE.contains(&field.as_str())
                && !SITEMAP_DIRECTIVE.contains(&field.as_str())
            {
                tracing::trace!(field = %field, "robots.txt rule without any user agent to enforce it on");
                continue;
            }

            if USER_AGENT_DIRECTIVE.contains(&field.as_str()) {
                let starts_new_group = previous_rule_field
                    .as_deref()
                    .is_some_and(|previous| !USER_AGENT_DIRECTIVE.contains(&previous));
                if starts_new_group {
                    current.clear();
                }
                self.register_user_agent(&value, &mut current);
            } else if ALLOW_DIRECTIVE.contains(&field.as_str()) {
                for &index in &current {
                    self.rule_sets[index].allow(&value);
                }
            } else if DISALLOW_DIRECTIVE.contains(&field.as_str()) {
                for &index in &current {
                    self.rule_sets[index].disallow(&value);
                }
            } else if SITEMAP_DIRECTIVE.contains(&field.as_str()) {
                self.sitemaps.push(value.clone());
            } else if CRAWL_DELAY_DIRECTIVE.contains(&field.as_str()) {
                if let Some(delay) = python_float(&value) {
                    for &index in &current {
                        self.rule_sets[index].crawl_delay = Some(delay);
                    }
                } else {
                    tracing::trace!(value = %value, "malformed robots.txt `Crawl-delay`; ignoring the rule");
                }
            } else if REQUEST_RATE_DIRECTIVE.contains(&field.as_str()) {
                if let Some(rate) = parse_request_rate(&value) {
                    for &index in &current {
                        self.rule_sets[index].request_rate = Some(rate);
                    }
                } else {
                    tracing::trace!(value = %value, "malformed robots.txt `Request-rate`; ignoring the rule");
                }
            } else if HOST_DIRECTIVE.contains(&field.as_str()) {
                self.host = Some(value.clone());
            } else if VISIT_TIME_DIRECTIVE.contains(&field.as_str()) {
                if let Some((start_time, end_time)) = parse_time_period(&value, ' ') {
                    for &index in &current {
                        self.rule_sets[index].visit_time = Some(VisitTime {
                            start_time,
                            end_time,
                        });
                    }
                }
            }

            previous_rule_field = Some(field);
        }
    }

    /// `protego`'s user-agent bookkeeping, including the "generously strip the wildcards"
    /// alias: `User-agent: Googlebot-*` registers both `googlebot-*` and `googlebot-`.
    fn register_user_agent(&mut self, value: &str, current: &mut Vec<usize>) {
        let user_agent = python_strip(value).to_lowercase();
        let without_asterisk =
            (user_agent != "*" && user_agent.contains('*')).then(|| user_agent.replace('*', ""));

        for candidate in [Some(user_agent), without_asterisk].into_iter().flatten() {
            if candidate.is_empty() {
                continue;
            }
            match self.user_agents.get(&candidate) {
                Some(&index) => {
                    if !current.contains(&index) {
                        current.push(index);
                    }
                }
                None => {
                    let index = self.rule_sets.len();
                    self.rule_sets.push(RuleSet::new(candidate.clone()));
                    self.user_agents.insert(candidate, index);
                    current.push(index);
                }
            }
        }
    }

    /// `protego._get_matching_rule_set` — highest `applies_to` score, first one on a tie.
    fn matching_rule_set(&self, user_agent: &str) -> Option<&RuleSet> {
        if self.user_agents.is_empty() {
            return None;
        }
        let mut best: Option<(usize, &RuleSet)> = None;
        for &index in self.user_agents.values() {
            let rule_set = &self.rule_sets[index];
            let score = rule_set.applies_to(user_agent);
            if best.is_none_or(|(best_score, _)| score > best_score) {
                best = Some((score, rule_set));
            }
        }
        best.filter(|(score, _)| *score > 0).map(|(_, set)| set)
    }

    /// Whether `user_agent` may fetch `url`.
    ///
    /// An empty ruleset, or a URL no rule matches, allows the fetch.
    #[must_use]
    pub fn can_fetch(&self, url: &str, user_agent: &str) -> bool {
        self.matching_rule_set(user_agent)
            .is_none_or(|rule_set| rule_set.can_fetch(url))
    }

    /// The `Crawl-delay` for `user_agent`, in seconds.
    #[must_use]
    pub fn crawl_delay(&self, user_agent: &str) -> Option<f64> {
        self.matching_rule_set(user_agent)
            .and_then(|rule_set| rule_set.crawl_delay)
    }

    /// The `Request-rate` for `user_agent`.
    #[must_use]
    pub fn request_rate(&self, user_agent: &str) -> Option<RequestRate> {
        self.matching_rule_set(user_agent)
            .and_then(|rule_set| rule_set.request_rate)
    }

    /// The `Visit-time` for `user_agent`.
    #[must_use]
    pub fn visit_time(&self, user_agent: &str) -> Option<VisitTime> {
        self.matching_rule_set(user_agent)
            .and_then(|rule_set| rule_set.visit_time)
    }

    /// Every `Sitemap:` URL, in document order. `SitemapSpider::_robots_body` reads this.
    ///
    /// Sitemap lines are not tied to a user-agent group, so they are collected globally.
    #[must_use]
    pub fn sitemaps(&self) -> &[String] {
        &self.sitemaps
    }

    /// The `Host:` directive, if any. Not used by the reference implementation; parsed for completeness.
    #[must_use]
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    /// Whether the document declared no user-agent group at all — the fully permissive
    /// ruleset every failure path degrades to.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.user_agents.is_empty()
    }
}

/// `protego`'s "second chance" parse for a line with no `:`.
///
/// The first whitespace-separated prefix that names a real directive wins, so
/// `"user agent *"` is `("user agent", "*")` while `"nonsense value"` is dropped.
///
/// The directive test here is **case-sensitive** — `protego` checks the raw prefix against
/// its lower-case vocabulary and only lower-cases the field afterwards, so `"user agent *"`
/// is honoured and `"User agent *"` is silently dropped. Reproduced rather than "fixed".
fn split_on_spaces(line: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = line.split(' ').collect();
    if parts.len() < 2 {
        return None;
    }
    let mut possible_field = parts[0].to_string();
    for index in 1..parts.len() {
        if is_valid_directive_field(&possible_field) {
            return Some((possible_field, parts[index..].join(" ")));
        }
        possible_field.push(' ');
        possible_field.push_str(parts[index]);
    }
    None
}

// ---------------------------------------------------------------------------
// The fetch seam
// ---------------------------------------------------------------------------

/// How [`RobotsTxtManager`] gets a `robots.txt` off the network.
///
/// Python passes a `fetch_fn: Callable[[str, str], Awaitable]`; the engine binds it to
/// `session_manager.fetch(Request(url, sid=sid))`. Work unit 17 supplies the real
/// implementation, so this crate never has to name work unit 12's HTTP client.
///
/// Errors are *expected*: an implementation is free to return `Err` for a DNS failure, a
/// timeout or a closed session, and the manager degrades to a fully permissive ruleset.
#[async_trait]
pub trait RobotsFetcher: Send + Sync + 'static {
    /// Fetch `url` through the session named `sid`.
    async fn fetch_robots(&self, url: &str, sid: &str) -> Result<FetchedResponse>;
}

/// A [`RobotsFetcher`] built from an async closure, for tests and for a caller that already
/// has a function.
///
/// ```
/// use std::sync::Arc;
/// use browser_spider::robots::{fetcher_fn, RobotsTxtManager};
/// use browser_spider::session::FetchedResponse;
///
/// let fetcher = fetcher_fn(|url: String, _sid: String| async move {
///     Ok(FetchedResponse::new(200, url, "User-agent: *\nDisallow: /admin/"))
/// });
/// let manager = RobotsTxtManager::new(Arc::new(fetcher));
/// ```
pub struct FnFetcher<F>(F);

/// Wrap an async closure as a [`RobotsFetcher`]. See [`FnFetcher`].
pub fn fetcher_fn<F, Fut>(function: F) -> FnFetcher<F>
where
    F: Fn(String, String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<FetchedResponse>> + Send,
{
    FnFetcher(function)
}

#[async_trait]
impl<F, Fut> RobotsFetcher for FnFetcher<F>
where
    F: Fn(String, String) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<FetchedResponse>> + Send,
{
    async fn fetch_robots(&self, url: &str, sid: &str) -> Result<FetchedResponse> {
        (self.0)(url.to_owned(), sid.to_owned()).await
    }
}

// ---------------------------------------------------------------------------
// The manager
// ---------------------------------------------------------------------------

/// Fetches, parses and caches one `robots.txt` per netloc.
///
/// Every method takes `&self`, so the engine holds an `Arc<RobotsTxtManager>` and every
/// request task consults the same cache.
///
/// # The degradation rule
///
/// A non-200 status, a transport error, an undecodable body and a parse failure all end in
/// the same place: an empty ruleset that allows everything and declares no delays. A
/// crawl is never stopped by a broken `robots.txt`.
///
/// # Cache key
///
/// `urlparse(url).netloc` — host **and port**, no scheme. `http://a.com/` and
/// `https://a.com/` therefore share one entry (the first one to arrive decides which
/// scheme was fetched), while `a.com` and `a.com:8080` do not. This is Python's behaviour
/// and the tests pin both halves of it.
///
/// # No in-flight de-duplication
///
/// Two concurrent misses for the same uncached netloc both fetch, and the last writer
/// wins — faithful to Python, which has no lock here either. A single-flight would be a
/// strict improvement and no test forbids it; it is left out so the request count stays
/// predictable for whoever compares the two implementations.
pub struct RobotsTxtManager {
    fetcher: Arc<dyn RobotsFetcher>,
    cache: DashMap<String, Arc<RobotsRules>>,
}

impl std::fmt::Debug for RobotsTxtManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RobotsTxtManager")
            .field("cached", &self.cached_domains())
            .finish_non_exhaustive()
    }
}

impl RobotsTxtManager {
    /// Build a manager over a fetcher.
    #[must_use]
    pub fn new(fetcher: Arc<dyn RobotsFetcher>) -> Self {
        Self {
            fetcher,
            cache: DashMap::new(),
        }
    }

    /// `_get_parser(url, sid)` — the cached ruleset for `url`'s netloc, fetching it once.
    pub async fn parser_for(&self, url: &str, sid: &str) -> Arc<RobotsRules> {
        let parsed = urlparse(url);
        let domain = parsed.netloc;

        if let Some(cached) = self.cache.get(&domain) {
            return Arc::clone(&cached);
        }

        let scheme = if parsed.scheme.is_empty() {
            "https"
        } else {
            &parsed.scheme
        };
        let robots_url = format!("{scheme}://{domain}/robots.txt");

        let mut content = String::new();
        match self.fetcher.fetch_robots(&robots_url, sid).await {
            Ok(response) => {
                if response.status == 200 {
                    // Python's `Response.encoding` defaults to `"utf-8"` and is never the
                    // empty string, so `body.decode("")` cannot happen there.
                    // `FetchedResponse` derives `Default`, which leaves `encoding` empty —
                    // and an empty label really is a `LookupError` (see
                    // `decode_robots_body`), which would silently turn every such
                    // `robots.txt` into "allow everything". Reading an empty field as
                    // "unset" rather than as the empty codec name is the one place this
                    // method departs from `_get_parser`; it is declared in
                    // `tests/u16_robots_manager_golden.rs`.
                    let encoding = if response.encoding.is_empty() {
                        "utf-8"
                    } else {
                        &response.encoding
                    };
                    match decode_robots_body(&response.body, encoding) {
                        Some(decoded) => content = decoded,
                        None => {
                            // Python's `body.decode(...)` raises `LookupError` here, and
                            // the `except Exception` that wraps the whole fetch leaves
                            // `content` empty — an allow-everything ruleset.
                            tracing::warn!(
                                %domain,
                                encoding = %response.encoding,
                                "Failed to fetch robots.txt: unknown encoding"
                            );
                        }
                    }
                }
            }
            Err(error) => {
                tracing::warn!(%domain, %error, "Failed to fetch robots.txt");
            }
        }

        let rules = Arc::new(RobotsRules::parse(&content));
        // Last writer wins, exactly like Python's `self._cache[domain] = parser`.
        self.cache.insert(domain, Arc::clone(&rules));
        rules
    }

    /// Whether `url` may be fetched, per its domain's `robots.txt`.
    ///
    /// Always evaluated against the `"*"` user-agent group: the spider never sends its own
    /// token here, so a site that only names `Googlebot` never restricts us.
    pub async fn can_fetch(&self, url: &str, sid: &str) -> bool {
        self.parser_for(url, sid).await.can_fetch(url, "*")
    }

    /// `(crawl_delay, (requests, seconds))` for `url`'s domain, from one parser lookup.
    pub async fn get_delay_directives(
        &self,
        url: &str,
        sid: &str,
    ) -> (Option<f64>, Option<(i64, i64)>) {
        let rules = self.parser_for(url, sid).await;
        let crawl_delay = rules.crawl_delay("*");
        let rate = rules
            .request_rate("*")
            .map(|rate| (rate.requests, rate.seconds));
        (crawl_delay, rate)
    }

    /// Every `Sitemap:` URL declared by `url`'s domain. `SitemapSpider` reads this.
    pub async fn sitemaps(&self, url: &str, sid: &str) -> Vec<String> {
        self.parser_for(url, sid).await.sitemaps().to_vec()
    }

    /// Warm the cache for a list of seed URLs, concurrently.
    ///
    /// One fetch per *uncached* netloc; duplicates inside `urls` are not de-duplicated
    /// here, exactly as in Python — use [`prefetch_urls`] to build the list, which is what
    /// the engine does.
    pub async fn prefetch(&self, urls: &[String], sid: &str) {
        if urls.is_empty() {
            return;
        }
        tracing::debug!(domains = urls.len(), "Pre-fetching robots.txt");
        let pending = urls.iter().map(|url| self.parser_for(url, sid));
        futures::future::join_all(pending).await;
    }

    /// The netlocs currently cached, sorted.
    #[must_use]
    pub fn cached_domains(&self) -> Vec<String> {
        let mut domains: Vec<String> = self.cache.iter().map(|entry| entry.key().clone()).collect();
        domains.sort();
        domains
    }

    /// Whether a netloc has already been fetched.
    #[must_use]
    pub fn is_cached(&self, domain: &str) -> bool {
        self.cache.contains_key(domain)
    }

    /// How many netlocs are cached.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether nothing has been fetched yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }

    /// Forget every cached ruleset. The engine does this between crawls.
    pub fn clear(&self) {
        self.cache.clear();
    }

    /// Seed a ruleset directly, without fetching.
    ///
    /// Not a Python behaviour; it exists so a test — or a spider that already holds a
    /// `robots.txt` body — can prime the cache. `domain` is a netloc.
    pub fn insert(&self, domain: impl Into<String>, rules: RobotsRules) {
        self.cache.insert(domain.into(), Arc::new(rules));
    }
}

// ---------------------------------------------------------------------------
// Engine helpers (spec §6.4 and §6.6), kept here because they are pure
// ---------------------------------------------------------------------------

/// The URL list `CrawlerEngine._prefetch_robots_txt` builds from `spider.start_urls`.
///
/// Keeps the first URL per netloc and rewrites it as `"{scheme}://{netloc}/"`, so one fetch
/// covers every seed on a host.
///
/// Two behaviours here look wrong and are deliberate, because the engine has them:
///
/// * a start URL with **no netloc** (`"/relative"`, `""`, `"relative/path"`) is *not*
///   dropped — it becomes the string `":///"`, and the netloc it de-duplicates on is the
///   empty one, so at most one such seed survives. `RobotsTxtManager::parser_for` then
///   fetches `https:///robots.txt` for it once, fails, and caches an allow-everything
///   ruleset under `""`;
/// * the scheme is **not** defaulted to `https` here, unlike in `parser_for`, so a
///   protocol-relative `"//a.com/x"` becomes `"://a.com/"`.
///
/// Both are pinned by `testdata/u16_engine_helpers.jsonl`, which replays
/// `CrawlerEngine._prefetch_robots_txt`'s own expression.
///
/// ```
/// use browser_spider::robots::prefetch_urls;
///
/// let seeds = [
///     "https://a.com/one".to_owned(),
///     "https://a.com/two".to_owned(),
///     "http://b.com:8080/x".to_owned(),
/// ];
/// assert_eq!(
///     prefetch_urls(&seeds),
///     vec!["https://a.com/".to_owned(), "http://b.com:8080/".to_owned()]
/// );
///
/// // A relative seed still produces one (doomed) fetch, exactly as in Python.
/// assert_eq!(prefetch_urls(&["/relative".to_owned()]), vec![":///".to_owned()]);
/// ```
#[must_use]
pub fn prefetch_urls(start_urls: &[String]) -> Vec<String> {
    let mut seen: IndexMap<String, String> = IndexMap::new();
    for url in start_urls {
        let parts = urlparse(url);
        seen.entry(parts.netloc.clone())
            .or_insert_with(|| format!("{}://{}/", parts.scheme, parts.netloc));
    }
    seen.into_values().collect()
}

/// `CrawlerEngine._get_domain_delay`'s arithmetic: the floor a domain's requests must obey.
///
/// `max(download_delay, seconds / requests, crawl_delay)`, with the rate ignored when it
/// asks for zero or fewer requests.
///
/// ```
/// use browser_spider::robots::effective_domain_delay;
///
/// assert_eq!(effective_domain_delay(0.5, None, None), 0.5);
/// assert_eq!(effective_domain_delay(0.5, Some(2.0), None), 2.0);
/// assert_eq!(effective_domain_delay(0.5, None, Some((1, 10))), 10.0);
/// assert_eq!(effective_domain_delay(0.5, None, Some((0, 10))), 0.5); // guarded
/// ```
#[must_use]
pub fn effective_domain_delay(
    download_delay: f64,
    crawl_delay: Option<f64>,
    request_rate: Option<(i64, i64)>,
) -> f64 {
    let mut delay = download_delay;
    if let Some((requests, seconds)) = request_rate {
        if requests > 0 {
            #[allow(clippy::cast_precision_loss)]
            let per_request = seconds as f64 / requests as f64;
            delay = python_max(delay, per_request);
        }
    }
    if let Some(crawl_delay) = crawl_delay {
        // Python's `max`, not `f64::max`: `max(nan, 1.0)` is `nan` there and `1.0` here,
        // and a `Crawl-delay: nan` really does reach this (`protego` reads it through
        // `float()`, which accepts `"nan"`). A NaN floor then poisons every delay for the
        // domain — faithfully.
        delay = python_max(delay, crawl_delay);
    }
    delay
}

/// Decode a `robots.txt` body the way Python's `body.decode(encoding, errors="replace")`
/// does, or `None` when Python would have raised instead.
///
/// `RobotsTxtManager._get_parser` performs the decode **inside** its `try`, so a
/// `LookupError` from an unknown codec label — or from a byte-to-byte codec like `base64`,
/// which is not a text encoding — leaves `content` as the empty string and the domain ends
/// up with an allow-everything ruleset. Returning `None` here is that path.
///
/// Getting this wrong is not cosmetic: falling back to UTF-8 for an unknown label made the
/// port *apply* rules Python ignores, and falling back to UTF-8 for a `utf-16` label turned
/// a real ruleset into mojibake (or mojibake into a real ruleset). Both showed up in
/// `testdata/u16_robots_manager.jsonl`.
///
/// Note `latin-1` here is *true* ISO-8859-1 (`0x80..=0x9F` are C1 controls), which is what
/// Python does; it is deliberately not the WHATWG windows-1252 alias that `browser-dom`
/// uses for HTML.
///
/// ```
/// use browser_spider::robots::decode_robots_body;
///
/// assert_eq!(decode_robots_body(b"Disallow: /a", "utf-8").as_deref(), Some("Disallow: /a"));
/// assert_eq!(decode_robots_body(b"caf\xe9", "latin-1").as_deref(), Some("café"));
/// assert_eq!(decode_robots_body(b"x", "no-such-codec"), None); // Python raises LookupError
/// ```
#[must_use]
pub fn decode_robots_body(body: &[u8], encoding: &str) -> Option<String> {
    if body.is_empty() {
        // CPython short-circuits an empty `bytes` before it ever looks the codec up, so
        // `b"".decode("no-such-codec")` is `""` rather than a `LookupError`. It matters:
        // an empty `robots.txt` served with a nonsense charset is "allow everything" on
        // both paths, and treating it as a lookup failure would only *look* the same.
        return Some(String::new());
    }
    let module = python_codec_module(encoding)?;
    Some(match module {
        "utf_8" => String::from_utf8_lossy(body).into_owned(),
        "utf_8_sig" => {
            let rest = body.strip_prefix(&[0xEF, 0xBB, 0xBF][..]).unwrap_or(body);
            String::from_utf8_lossy(rest).into_owned()
        }
        "utf_16" | "utf_16_le" | "utf_16_be" => decode_utf16(body, module),
        "utf_32" | "utf_32_le" | "utf_32_be" => decode_utf32(body, module),
        "latin_1" => body.iter().map(|&byte| char::from(byte)).collect(),
        "ascii" => body
            .iter()
            .map(|&byte| {
                if byte.is_ascii() {
                    char::from(byte)
                } else {
                    char::REPLACEMENT_CHARACTER
                }
            })
            .collect(),
        "cp1252" => decode_cp1252(body),
        other => {
            // A codec Python knows and this port does not implement. Every remaining one
            // is an ASCII superset (the UTF-16/32 family, the only labels where an
            // ASCII-shaped `robots.txt` would come out unreadable, is handled above), and
            // every `robots.txt` directive keyword and path is ASCII — so UTF-8 with
            // replacement reproduces the rules even when it does not reproduce the exact
            // code points of a comment. Declared in `tests/u16_robots_manager_golden.rs`.
            tracing::debug!(
                encoding = other,
                "robots.txt served in a codec this port does not implement; \
                 decoding as UTF-8"
            );
            String::from_utf8_lossy(body).into_owned()
        }
    })
}

/// Python's `encodings.normalize_encoding` followed by the alias lookup, returning the
/// codec *module* name (`"utf_8"`, `"iso8859_1"`, …) or `None` for `LookupError`.
///
/// The normalisation is exact: `codecs.lookup` lower-cases first, then every run of
/// characters that are neither alphanumeric nor `.` collapses to a single `_` — which is
/// why `"utf 8"` and `"utf--8"` both resolve and `"...utf..8..."` does not.
fn python_codec_module(label: &str) -> Option<&'static str> {
    let normalized = normalize_codec_label(label);
    let resolve = |key: &str| {
        PYTHON_CODECS
            .binary_search_by(|(name, _)| (*name).cmp(key))
            .ok()
            .map(|index| PYTHON_CODECS[index].1)
    };
    // `search_function` retries with `.` rewritten to `_`, which is how `"iso.8859.1"`
    // reaches `iso8859_1`.
    let module = resolve(&normalized).or_else(|| resolve(&normalized.replace('.', "_")))?;
    if module.is_empty() {
        // A registered codec that `bytes.decode` cannot use: either not a *text*
        // encoding at all (`base64`, `hex`, `zlib`, `rot_13`, `uu`, `quopri`, `bz2`),
        // which raises `LookupError`, or one whose decoder rejects every error handler
        // other than `strict` (`idna`, `undefined`), which raises `UnicodeError`. Both
        // land in `_get_parser`'s `except Exception`.
        return None;
    }
    Some(module)
}

/// `encodings.normalize_encoding`, plus the lower-casing `codecs.lookup` does first.
fn normalize_codec_label(label: &str) -> String {
    let mut out = String::with_capacity(label.len());
    let mut punctuation = false;
    for ch in label.chars() {
        if ch.is_alphanumeric() || ch == '.' {
            if punctuation && !out.is_empty() {
                out.push('_');
            }
            if ch.is_ascii() {
                out.extend(ch.to_lowercase());
            }
            punctuation = false;
        } else {
            punctuation = true;
        }
    }
    out
}

/// `bytes.decode("utf-16*", errors="replace")`.
///
/// Python's plain `utf-16` consumes a BOM when there is one and otherwise assumes the
/// host's byte order, which is little-endian on every platform this crate builds for.
fn decode_utf16(body: &[u8], module: &str) -> String {
    let (little_endian, rest) = match module {
        "utf_16_le" => (true, body),
        "utf_16_be" => (false, body),
        _ => match body {
            [0xFF, 0xFE, rest @ ..] => (true, rest),
            [0xFE, 0xFF, rest @ ..] => (false, rest),
            _ => (true, body),
        },
    };
    let units: Vec<u16> = rest
        .chunks_exact(2)
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes([pair[0], pair[1]])
            } else {
                u16::from_be_bytes([pair[0], pair[1]])
            }
        })
        .collect();
    let mut out: String = char::decode_utf16(units)
        .map(|unit| unit.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect();
    if rest.len() % 2 == 1 {
        out.push(char::REPLACEMENT_CHARACTER); // The dangling byte, replaced.
    }
    out
}

/// `bytes.decode("utf-32*", errors="replace")`. Same BOM rule as [`decode_utf16`].
fn decode_utf32(body: &[u8], module: &str) -> String {
    let (little_endian, rest) = match module {
        "utf_32_le" => (true, body),
        "utf_32_be" => (false, body),
        _ => match body {
            [0xFF, 0xFE, 0x00, 0x00, rest @ ..] => (true, rest),
            [0x00, 0x00, 0xFE, 0xFF, rest @ ..] => (false, rest),
            _ => (true, body),
        },
    };
    let mut out = String::with_capacity(rest.len() / 4);
    for quad in rest.chunks_exact(4) {
        let code = if little_endian {
            u32::from_le_bytes([quad[0], quad[1], quad[2], quad[3]])
        } else {
            u32::from_be_bytes([quad[0], quad[1], quad[2], quad[3]])
        };
        out.push(char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER));
    }
    if rest.len() % 4 != 0 {
        out.push(char::REPLACEMENT_CHARACTER);
    }
    out
}

/// The 32 code points where windows-1252 differs from ISO-8859-1, `0x80..=0x9F`.
const CP1252_HIGH: [char; 32] = [
    '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}', '\u{017D}', '\u{FFFD}',
    '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
];

fn decode_cp1252(body: &[u8]) -> String {
    body.iter()
        .map(|&byte| match byte {
            0x80..=0x9F => CP1252_HIGH[usize::from(byte - 0x80)],
            other => char::from(other),
        })
        .collect()
}

/// Every codec label CPython's `codecs.lookup` resolves, mapped to its module name.
///
/// Sorted, so the lookup is a binary search. An empty module name marks a codec that is
/// registered but that `bytes.decode(label, errors="replace")` cannot actually use — see
/// [`python_codec_module`].
///
/// Generated from the reference interpreter: `encodings.aliases` plus the modules in
/// `encodings/`, each probed with a real `bytes.decode(..., errors="replace")` call rather
/// than trusted from `_is_text_encoding`, because `idna` and `undefined` pass that flag and
/// still raise. Regenerating it on a newer CPython only ever adds labels.
#[rustfmt::skip]
const PYTHON_CODECS: [(&str, &str); 455] = [
    ("037", "cp037"), ("1026", "cp1026"), ("1125", "cp1125"), ("1140", "cp1140"),
    ("1250", "cp1250"), ("1251", "cp1251"), ("1252", "cp1252"), ("1253", "cp1253"),
    ("1254", "cp1254"), ("1255", "cp1255"), ("1256", "cp1256"), ("1257", "cp1257"),
    ("1258", "cp1258"), ("273", "cp273"), ("424", "cp424"), ("437", "cp437"), ("500", "cp500"),
    ("646", "ascii"), ("775", "cp775"), ("850", "cp850"), ("852", "cp852"), ("855", "cp855"),
    ("857", "cp857"), ("858", "cp858"), ("860", "cp860"), ("861", "cp861"), ("862", "cp862"),
    ("863", "cp863"), ("864", "cp864"), ("865", "cp865"), ("866", "cp866"), ("869", "cp869"),
    ("874", "cp874"), ("8859", "latin_1"), ("932", "cp932"), ("936", "gbk"), ("949", "cp949"),
    ("950", "cp950"), ("ansi_x3.4_1968", "ascii"), ("ansi_x3.4_1986", "ascii"),
    ("ansi_x3_4_1968", "ascii"), ("arabic", "iso8859_6"), ("ascii", "ascii"),
    ("asmo_708", "iso8859_6"), ("base64", ""), ("base64_codec", ""), ("base_64", ""),
    ("big5", "big5"), ("big5_hkscs", "big5hkscs"), ("big5_tw", "big5"),
    ("big5hkscs", "big5hkscs"), ("bz2", ""), ("bz2_codec", ""), ("charmap", "charmap"),
    ("chinese", "gb2312"), ("cp00858", "cp858"), ("cp01140", "cp1140"), ("cp037", "cp037"),
    ("cp1006", "cp1006"), ("cp1026", "cp1026"), ("cp1051", "hp_roman8"), ("cp1125", "cp1125"),
    ("cp1140", "cp1140"), ("cp1250", "cp1250"), ("cp1251", "cp1251"), ("cp1252", "cp1252"),
    ("cp1253", "cp1253"), ("cp1254", "cp1254"), ("cp1255", "cp1255"), ("cp1256", "cp1256"),
    ("cp1257", "cp1257"), ("cp1258", "cp1258"), ("cp1361", "johab"), ("cp154", "ptcp154"),
    ("cp273", "cp273"), ("cp367", "ascii"), ("cp424", "cp424"), ("cp437", "cp437"),
    ("cp500", "cp500"), ("cp65001", "utf_8"), ("cp720", "cp720"), ("cp737", "cp737"),
    ("cp775", "cp775"), ("cp819", "latin_1"), ("cp850", "cp850"), ("cp852", "cp852"),
    ("cp855", "cp855"), ("cp856", "cp856"), ("cp857", "cp857"), ("cp858", "cp858"),
    ("cp860", "cp860"), ("cp861", "cp861"), ("cp862", "cp862"), ("cp863", "cp863"),
    ("cp864", "cp864"), ("cp865", "cp865"), ("cp866", "cp866"), ("cp866u", "cp1125"),
    ("cp869", "cp869"), ("cp874", "cp874"), ("cp875", "cp875"), ("cp932", "cp932"),
    ("cp936", "gbk"), ("cp949", "cp949"), ("cp950", "cp950"), ("cp_gr", "cp869"),
    ("cp_is", "cp861"), ("csascii", "ascii"), ("csbig5", "big5"), ("cseuckr", "euc_kr"),
    ("csibm00858", "cp858"), ("csibm01140", "cp1140"), ("csibm037", "cp037"),
    ("csibm1026", "cp1026"), ("csibm273", "cp273"), ("csibm424", "cp424"),
    ("csibm500", "cp500"), ("csibm855", "cp855"), ("csibm857", "cp857"), ("csibm858", "cp858"),
    ("csibm860", "cp860"), ("csibm861", "cp861"), ("csibm863", "cp863"), ("csibm864", "cp864"),
    ("csibm865", "cp865"), ("csibm866", "cp866"), ("csibm869", "cp869"),
    ("csiso2022jp", "iso2022_jp"), ("csiso2022kr", "iso2022_kr"),
    ("csiso58gb231280", "gb2312"), ("csisolatin1", "latin_1"), ("csisolatin2", "iso8859_2"),
    ("csisolatin3", "iso8859_3"), ("csisolatin4", "iso8859_4"), ("csisolatin5", "iso8859_9"),
    ("csisolatin6", "iso8859_10"), ("csisolatinarabic", "iso8859_6"),
    ("csisolatincyrillic", "iso8859_5"), ("csisolatingreek", "iso8859_7"),
    ("csisolatinhebrew", "iso8859_8"), ("cskoi8r", "koi8_r"), ("cspc775baltic", "cp775"),
    ("cspc850multilingual", "cp850"), ("cspc862latinhebrew", "cp862"),
    ("cspc8codepage437", "cp437"), ("cspcp852", "cp852"), ("csptcp154", "ptcp154"),
    ("csshiftjis", "shift_jis"), ("cyrillic", "iso8859_5"), ("cyrillic_asian", "ptcp154"),
    ("ebcdic_cp_be", "cp500"), ("ebcdic_cp_ca", "cp037"), ("ebcdic_cp_ch", "cp500"),
    ("ebcdic_cp_he", "cp424"), ("ebcdic_cp_nl", "cp037"), ("ebcdic_cp_us", "cp037"),
    ("ebcdic_cp_wt", "cp037"), ("ebcdic_us_37_euro", "cp1140"), ("ecma_114", "iso8859_6"),
    ("ecma_118", "iso8859_7"), ("elot_928", "iso8859_7"), ("euc_cn", "gb2312"),
    ("euc_jis2004", "euc_jis_2004"), ("euc_jis_2004", "euc_jis_2004"),
    ("euc_jisx0213", "euc_jisx0213"), ("euc_jp", "euc_jp"), ("euc_kr", "euc_kr"),
    ("euccn", "gb2312"), ("eucgb2312_cn", "gb2312"), ("eucjis2004", "euc_jis_2004"),
    ("eucjisx0213", "euc_jisx0213"), ("eucjp", "euc_jp"), ("euckr", "euc_kr"),
    ("gb18030", "gb18030"), ("gb18030_2000", "gb18030"), ("gb2312", "gb2312"),
    ("gb2312_1980", "gb2312"), ("gb2312_80", "gb2312"), ("gbk", "gbk"), ("greek", "iso8859_7"),
    ("greek8", "iso8859_7"), ("hebrew", "iso8859_8"), ("hex", ""), ("hex_codec", ""),
    ("hkscs", "big5hkscs"), ("hp_roman8", "hp_roman8"), ("hz", "hz"), ("hz_gb", "hz"),
    ("hz_gb_2312", "hz"), ("hzgb", "hz"), ("ibm00858", "cp858"), ("ibm01140", "cp1140"),
    ("ibm037", "cp037"), ("ibm039", "cp037"), ("ibm1026", "cp1026"), ("ibm1051", "hp_roman8"),
    ("ibm1125", "cp1125"), ("ibm1140", "cp1140"), ("ibm273", "cp273"), ("ibm367", "ascii"),
    ("ibm424", "cp424"), ("ibm437", "cp437"), ("ibm500", "cp500"), ("ibm775", "cp775"),
    ("ibm819", "latin_1"), ("ibm850", "cp850"), ("ibm852", "cp852"), ("ibm855", "cp855"),
    ("ibm857", "cp857"), ("ibm858", "cp858"), ("ibm860", "cp860"), ("ibm861", "cp861"),
    ("ibm862", "cp862"), ("ibm863", "cp863"), ("ibm864", "cp864"), ("ibm865", "cp865"),
    ("ibm866", "cp866"), ("ibm869", "cp869"), ("idna", ""), ("iso2022_jp", "iso2022_jp"),
    ("iso2022_jp_1", "iso2022_jp_1"), ("iso2022_jp_2", "iso2022_jp_2"),
    ("iso2022_jp_2004", "iso2022_jp_2004"), ("iso2022_jp_3", "iso2022_jp_3"),
    ("iso2022_jp_ext", "iso2022_jp_ext"), ("iso2022_kr", "iso2022_kr"),
    ("iso2022jp", "iso2022_jp"), ("iso2022jp_1", "iso2022_jp_1"),
    ("iso2022jp_2", "iso2022_jp_2"), ("iso2022jp_2004", "iso2022_jp_2004"),
    ("iso2022jp_3", "iso2022_jp_3"), ("iso2022jp_ext", "iso2022_jp_ext"),
    ("iso2022kr", "iso2022_kr"), ("iso646_us", "ascii"), ("iso8859", "latin_1"),
    ("iso8859_1", "latin_1"), ("iso8859_10", "iso8859_10"), ("iso8859_11", "iso8859_11"),
    ("iso8859_13", "iso8859_13"), ("iso8859_14", "iso8859_14"), ("iso8859_15", "iso8859_15"),
    ("iso8859_16", "iso8859_16"), ("iso8859_2", "iso8859_2"), ("iso8859_3", "iso8859_3"),
    ("iso8859_4", "iso8859_4"), ("iso8859_5", "iso8859_5"), ("iso8859_6", "iso8859_6"),
    ("iso8859_7", "iso8859_7"), ("iso8859_8", "iso8859_8"), ("iso8859_9", "iso8859_9"),
    ("iso_2022_jp", "iso2022_jp"), ("iso_2022_jp_1", "iso2022_jp_1"),
    ("iso_2022_jp_2", "iso2022_jp_2"), ("iso_2022_jp_2004", "iso2022_jp_2004"),
    ("iso_2022_jp_3", "iso2022_jp_3"), ("iso_2022_jp_ext", "iso2022_jp_ext"),
    ("iso_2022_kr", "iso2022_kr"), ("iso_646.irv_1991", "ascii"), ("iso_8859_1", "latin_1"),
    ("iso_8859_10", "iso8859_10"), ("iso_8859_10_1992", "iso8859_10"),
    ("iso_8859_11", "iso8859_11"), ("iso_8859_11_2001", "iso8859_11"),
    ("iso_8859_13", "iso8859_13"), ("iso_8859_14", "iso8859_14"),
    ("iso_8859_14_1998", "iso8859_14"), ("iso_8859_15", "iso8859_15"),
    ("iso_8859_16", "iso8859_16"), ("iso_8859_16_2001", "iso8859_16"),
    ("iso_8859_1_1987", "latin_1"), ("iso_8859_2", "iso8859_2"),
    ("iso_8859_2_1987", "iso8859_2"), ("iso_8859_3", "iso8859_3"),
    ("iso_8859_3_1988", "iso8859_3"), ("iso_8859_4", "iso8859_4"),
    ("iso_8859_4_1988", "iso8859_4"), ("iso_8859_5", "iso8859_5"),
    ("iso_8859_5_1988", "iso8859_5"), ("iso_8859_6", "iso8859_6"),
    ("iso_8859_6_1987", "iso8859_6"), ("iso_8859_7", "iso8859_7"),
    ("iso_8859_7_1987", "iso8859_7"), ("iso_8859_8", "iso8859_8"),
    ("iso_8859_8_1988", "iso8859_8"), ("iso_8859_8_e", "iso8859_8"),
    ("iso_8859_8_i", "iso8859_8"), ("iso_8859_9", "iso8859_9"),
    ("iso_8859_9_1989", "iso8859_9"), ("iso_celtic", "iso8859_14"), ("iso_ir_100", "latin_1"),
    ("iso_ir_101", "iso8859_2"), ("iso_ir_109", "iso8859_3"), ("iso_ir_110", "iso8859_4"),
    ("iso_ir_126", "iso8859_7"), ("iso_ir_127", "iso8859_6"), ("iso_ir_138", "iso8859_8"),
    ("iso_ir_144", "iso8859_5"), ("iso_ir_148", "iso8859_9"), ("iso_ir_157", "iso8859_10"),
    ("iso_ir_166", "tis_620"), ("iso_ir_199", "iso8859_14"), ("iso_ir_226", "iso8859_16"),
    ("iso_ir_58", "gb2312"), ("iso_ir_6", "ascii"), ("jisx0213", "euc_jis_2004"),
    ("johab", "johab"), ("koi8_r", "koi8_r"), ("koi8_t", "koi8_t"), ("koi8_u", "koi8_u"),
    ("korean", "euc_kr"), ("ks_c_5601", "euc_kr"), ("ks_c_5601_1987", "euc_kr"),
    ("ks_x_1001", "euc_kr"), ("ksc5601", "euc_kr"), ("ksx1001", "euc_kr"),
    ("kz1048", "kz1048"), ("kz_1048", "kz1048"), ("l1", "latin_1"), ("l10", "iso8859_16"),
    ("l2", "iso8859_2"), ("l3", "iso8859_3"), ("l4", "iso8859_4"), ("l5", "iso8859_9"),
    ("l6", "iso8859_10"), ("l7", "iso8859_13"), ("l8", "iso8859_14"), ("l9", "iso8859_15"),
    ("latin", "latin_1"), ("latin1", "latin_1"), ("latin10", "iso8859_16"),
    ("latin2", "iso8859_2"), ("latin3", "iso8859_3"), ("latin4", "iso8859_4"),
    ("latin5", "iso8859_9"), ("latin6", "iso8859_10"), ("latin7", "iso8859_13"),
    ("latin8", "iso8859_14"), ("latin9", "iso8859_15"), ("latin_1", "latin_1"),
    ("mac_arabic", "mac_arabic"), ("mac_centeuro", "mac_latin2"),
    ("mac_croatian", "mac_croatian"), ("mac_cyrillic", "mac_cyrillic"),
    ("mac_farsi", "mac_farsi"), ("mac_greek", "mac_greek"), ("mac_iceland", "mac_iceland"),
    ("mac_latin2", "mac_latin2"), ("mac_roman", "mac_roman"), ("mac_romanian", "mac_romanian"),
    ("mac_turkish", "mac_turkish"), ("maccentraleurope", "mac_latin2"),
    ("maccyrillic", "mac_cyrillic"), ("macgreek", "mac_greek"), ("maciceland", "mac_iceland"),
    ("macintosh", "mac_roman"), ("maclatin2", "mac_latin2"), ("macroman", "mac_roman"),
    ("macturkish", "mac_turkish"), ("ms1361", "johab"), ("ms874", "cp874"), ("ms932", "cp932"),
    ("ms936", "gbk"), ("ms949", "cp949"), ("ms950", "cp950"), ("ms_kanji", "cp932"),
    ("mskanji", "cp932"), ("palmos", "palmos"), ("pc_multilingual_850_euro", "cp858"),
    ("pt154", "ptcp154"), ("ptcp154", "ptcp154"), ("punycode", "punycode"), ("quopri", ""),
    ("quopri_codec", ""), ("quoted_printable", ""), ("quotedprintable", ""),
    ("r8", "hp_roman8"), ("raw_unicode_escape", "raw_unicode_escape"), ("rk1048", "kz1048"),
    ("roman8", "hp_roman8"), ("rot13", ""), ("rot_13", ""), ("ruscii", "cp1125"),
    ("s_jis", "shift_jis"), ("s_jis_2004", "shift_jis_2004"), ("s_jisx0213", "shift_jisx0213"),
    ("shift_jis", "shift_jis"), ("shift_jis_2004", "shift_jis_2004"),
    ("shift_jisx0213", "shift_jisx0213"), ("shiftjis", "shift_jis"),
    ("shiftjis2004", "shift_jis_2004"), ("shiftjisx0213", "shift_jisx0213"),
    ("sjis", "shift_jis"), ("sjis_2004", "shift_jis_2004"), ("sjisx0213", "shift_jisx0213"),
    ("strk1048_2002", "kz1048"), ("thai", "iso8859_11"), ("tis620", "tis_620"),
    ("tis_620", "tis_620"), ("tis_620_0", "tis_620"), ("tis_620_2529_0", "tis_620"),
    ("tis_620_2529_1", "tis_620"), ("u16", "utf_16"), ("u32", "utf_32"), ("u7", "utf_7"),
    ("u8", "utf_8"), ("u_jis", "euc_jp"), ("uhc", "cp949"), ("ujis", "euc_jp"),
    ("undefined", ""), ("unicode_1_1_utf_7", "utf_7"), ("unicode_escape", "unicode_escape"),
    ("unicodebigunmarked", "utf_16_be"), ("unicodelittleunmarked", "utf_16_le"),
    ("us", "ascii"), ("us_ascii", "ascii"), ("utf", "utf_8"), ("utf16", "utf_16"),
    ("utf32", "utf_32"), ("utf7", "utf_7"), ("utf8", "utf_8"), ("utf8_ucs2", "utf_8"),
    ("utf8_ucs4", "utf_8"), ("utf_16", "utf_16"), ("utf_16_be", "utf_16_be"),
    ("utf_16_le", "utf_16_le"), ("utf_16be", "utf_16_be"), ("utf_16le", "utf_16_le"),
    ("utf_32", "utf_32"), ("utf_32_be", "utf_32_be"), ("utf_32_le", "utf_32_le"),
    ("utf_32be", "utf_32_be"), ("utf_32le", "utf_32_le"), ("utf_7", "utf_7"),
    ("utf_8", "utf_8"), ("utf_8_sig", "utf_8_sig"), ("uu", ""), ("uu_codec", ""),
    ("windows_1250", "cp1250"), ("windows_1251", "cp1251"), ("windows_1252", "cp1252"),
    ("windows_1253", "cp1253"), ("windows_1254", "cp1254"), ("windows_1255", "cp1255"),
    ("windows_1256", "cp1256"), ("windows_1257", "cp1257"), ("windows_1258", "cp1258"),
    ("windows_31j", "cp932"), ("windows_874", "cp874"), ("x_mac_japanese", "shift_jis"),
    ("x_mac_korean", "euc_kr"), ("x_mac_simp_chinese", "gb2312"),
    ("x_mac_trad_chinese", "big5"), ("zip", ""), ("zlib", ""), ("zlib_codec", ""),
];

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;

    // -- parsing ----------------------------------------------------------

    #[test]
    fn disallow_blocks_the_prefix() {
        let rules = RobotsRules::parse("User-agent: *\nDisallow: /admin/\n");
        assert!(!rules.can_fetch("https://example.com/admin/", "*"));
        assert!(!rules.can_fetch("https://example.com/admin/users", "*"));
        assert!(rules.can_fetch("https://example.com/products", "*"));
        assert!(rules.can_fetch("https://example.com/", "*"));
    }

    #[test]
    fn allow_beats_a_longer_disallow() {
        let rules =
            RobotsRules::parse("User-agent: *\nDisallow: /secret/\nAllow: /secret/public.html\n");
        assert!(rules.can_fetch("https://example.com/secret/public.html", "*"));
        assert!(!rules.can_fetch("https://example.com/secret/private.html", "*"));
    }

    #[test]
    fn allow_wins_an_exact_length_tie() {
        let rules = RobotsRules::parse("User-agent: *\nDisallow: /a/b\nAllow: /a/b\n");
        assert!(rules.can_fetch("https://example.com/a/b", "*"));
    }

    #[test]
    fn disallow_root_blocks_everything() {
        let rules = RobotsRules::parse("User-agent: *\nDisallow: /\n");
        for path in ["/", "/page", "/a/b/c"] {
            assert!(
                !rules.can_fetch(&format!("https://example.com{path}"), "*"),
                "{path}"
            );
        }
    }

    #[test]
    fn empty_document_allows_everything() {
        let rules = RobotsRules::parse("");
        assert!(rules.is_empty());
        assert!(rules.can_fetch("https://example.com/anything", "*"));
        assert_eq!(rules.crawl_delay("*"), None);
        assert_eq!(rules.request_rate("*"), None);
    }

    #[test]
    fn dollar_anchor_only_matches_the_suffix() {
        let rules = RobotsRules::parse("User-agent: *\nDisallow: /*.pdf$\n");
        assert!(!rules.can_fetch("https://example.com/report.pdf", "*"));
        assert!(rules.can_fetch("https://example.com/report.html", "*"));
        assert!(rules.can_fetch("https://example.com/report.pdf.html", "*"));
    }

    #[test]
    fn index_html_allow_also_allows_the_directory() {
        let rules =
            RobotsRules::parse("User-agent: *\nDisallow: /docs/\nAllow: /docs/index.html\n");
        assert!(rules.can_fetch("https://example.com/docs/index.html", "*"));
        assert!(rules.can_fetch("https://example.com/docs/", "*"));
        assert!(!rules.can_fetch("https://example.com/docs/other", "*"));
    }

    #[test]
    fn only_the_star_group_is_consulted() {
        let rules = RobotsRules::parse("User-agent: Googlebot\nDisallow: /\n");
        assert!(rules.can_fetch("https://example.com/anything", "*"));
        assert!(!rules.can_fetch("https://example.com/anything", "googlebot"));
    }

    #[test]
    fn wildcard_agents_register_a_stripped_alias() {
        let rules = RobotsRules::parse("User-agent: Googlebot-*\nDisallow: /x\n");
        assert!(!rules.can_fetch("https://example.com/x", "googlebot-news"));
    }

    #[test]
    fn crawl_delay_is_a_float() {
        assert_eq!(
            RobotsRules::parse("User-agent: *\nCrawl-delay: 2\n").crawl_delay("*"),
            Some(2.0)
        );
        assert_eq!(
            RobotsRules::parse("User-agent: *\nCrawl-delay: 0.5\n").crawl_delay("*"),
            Some(0.5)
        );
        assert_eq!(
            RobotsRules::parse("User-agent: *\nCrawl-delay: soon\n").crawl_delay("*"),
            None
        );
    }

    #[test]
    fn request_rate_forms() {
        let rate = |body: &str| RobotsRules::parse(body).request_rate("*");
        assert_eq!(
            rate("User-agent: *\nRequest-rate: 1/10\n"),
            Some(RequestRate {
                requests: 1,
                seconds: 10,
                start_time: None,
                end_time: None,
            })
        );
        assert_eq!(
            rate("User-agent: *\nRequest-rate: 3/5m\n").map(|r| (r.requests, r.seconds)),
            Some((3, 300))
        );
        assert_eq!(
            rate("User-agent: *\nRequest-rate: 1/2h\n").map(|r| (r.requests, r.seconds)),
            Some((1, 7200))
        );
        assert_eq!(
            rate("User-agent: *\nRequest-rate: 1/1d\n").map(|r| (r.requests, r.seconds)),
            Some((1, 86_400))
        );
        assert_eq!(
            rate("User-agent: *\nRequest-rate: 1/10 0600-0845\n").map(|r| r.start_time),
            Some(Some(TimeOfDay { hour: 6, minute: 0 }))
        );
        assert_eq!(rate("User-agent: *\nRequest-rate: nonsense\n"), None);
        assert_eq!(rate("User-agent: *\nRequest-rate: 1/\n"), None);
    }

    #[test]
    fn sitemaps_are_collected_without_a_user_agent() {
        let rules = RobotsRules::parse(
            "Sitemap: https://example.com/sitemap.xml\nUser-agent: *\nSitemap: https://example.com/b.xml\n",
        );
        assert_eq!(
            rules.sitemaps(),
            [
                "https://example.com/sitemap.xml".to_owned(),
                "https://example.com/b.xml".to_owned()
            ]
        );
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let rules = RobotsRules::parse("# hello\n\n  User-agent: *  # who\nDisallow: /a # why\n");
        assert!(!rules.can_fetch("https://example.com/a", "*"));
    }

    #[test]
    fn empty_disallow_value_is_ignored_and_ends_the_group() {
        // `Disallow:` with no value allows everything, and — a protego wart — the empty
        // rule counts as "previous field", so the next `User-agent` starts a fresh group.
        let rules = RobotsRules::parse("User-agent: *\nDisallow:\nUser-agent: bot\nDisallow: /x\n");
        assert!(rules.can_fetch("https://example.com/x", "*"));
        assert!(!rules.can_fetch("https://example.com/x", "bot"));
    }

    #[test]
    fn rules_before_any_user_agent_are_dropped() {
        let rules = RobotsRules::parse("Disallow: /a\nUser-agent: *\nDisallow: /b\n");
        assert!(rules.can_fetch("https://example.com/a", "*"));
        assert!(!rules.can_fetch("https://example.com/b", "*"));
    }

    #[test]
    fn space_separated_directives_are_accepted() {
        let rules = RobotsRules::parse("user agent *\ndisallow /admin\n");
        assert!(!rules.can_fetch("https://example.com/admin", "*"));
    }

    #[test]
    fn misspelled_disallow_still_counts() {
        let rules = RobotsRules::parse("User-agent: *\nDissallow: /admin\n");
        assert!(!rules.can_fetch("https://example.com/admin", "*"));
    }

    #[test]
    fn query_strings_participate_in_matching() {
        let rules = RobotsRules::parse("User-agent: *\nDisallow: /*?sort=\n");
        assert!(!rules.can_fetch("https://example.com/list?sort=price", "*"));
        assert!(rules.can_fetch("https://example.com/list?page=2", "*"));
    }

    #[test]
    fn lone_carriage_returns_split_lines() {
        let rules = RobotsRules::parse("User-agent: *\rDisallow: /a\r");
        assert!(!rules.can_fetch("https://example.com/a", "*"));
    }

    #[test]
    fn robots_txt_itself_can_be_disallowed() {
        // texting_robots hard-codes `/robots.txt` as allowed; protego does not.
        let rules = RobotsRules::parse("User-agent: *\nDisallow: /\n");
        assert!(!rules.can_fetch("https://example.com/robots.txt", "*"));
    }

    #[test]
    fn host_directive_is_captured() {
        let rules = RobotsRules::parse("User-agent: *\nHost: www.example.com\n");
        assert_eq!(rules.host(), Some("www.example.com"));
    }

    #[test]
    fn visit_time_is_captured() {
        let rules = RobotsRules::parse("User-agent: *\nVisit-time: 0600 0845\n");
        assert_eq!(
            rules.visit_time("*"),
            Some(VisitTime {
                start_time: TimeOfDay { hour: 6, minute: 0 },
                end_time: TimeOfDay {
                    hour: 8,
                    minute: 45
                },
            })
        );
    }

    // -- helpers ----------------------------------------------------------

    #[test]
    fn splitlines_matches_python() {
        assert_eq!(python_splitlines("a\nb"), vec!["a", "b"]);
        assert_eq!(python_splitlines("a\r\nb"), vec!["a", "b"]);
        assert_eq!(python_splitlines("a\rb"), vec!["a", "b"]);
        assert_eq!(python_splitlines("a\u{2028}b"), vec!["a", "b"]);
        assert_eq!(python_splitlines("a\n"), vec!["a"]);
        assert!(python_splitlines("").is_empty());
    }

    #[test]
    fn quote_path_normalises() {
        assert_eq!(quote_path("https://example.com/a b"), "/a%20b");
        assert_eq!(quote_path("https://example.com"), "/");
        assert_eq!(quote_path("https://example.com/%7Euser"), "/~user");
    }

    #[test]
    fn effective_delay_takes_the_maximum() {
        assert_eq!(effective_domain_delay(1.0, Some(0.5), Some((10, 10))), 1.0);
        assert_eq!(effective_domain_delay(1.0, Some(5.0), Some((1, 2))), 5.0);
        assert_eq!(effective_domain_delay(1.0, Some(0.5), Some((1, 20))), 20.0);
    }

    #[test]
    fn prefetch_urls_keeps_one_per_netloc() {
        let seeds = [
            "https://a.com/one".to_owned(),
            "https://a.com/two".to_owned(),
            "http://a.com/three".to_owned(),
            "https://b.com:8443/x".to_owned(),
            "/relative".to_owned(),
            "also/relative".to_owned(),
        ];
        // The two netloc-less seeds collapse into the single `":///"` the engine builds,
        // and it is *not* dropped - see the function docs.
        assert_eq!(
            prefetch_urls(&seeds),
            vec![
                "https://a.com/".to_owned(),
                "https://b.com:8443/".to_owned(),
                ":///".to_owned()
            ]
        );
    }

    #[test]
    fn latin1_bodies_decode() {
        let decoded = decode_robots_body(b"# caf\xe9", "latin-1");
        assert_eq!(decoded.as_deref(), Some("# café"));
    }

    #[test]
    fn unknown_encoding_is_a_lookup_error_like_python() {
        // `b"...".decode("made-up-9000")` raises `LookupError`, which leaves the manager
        // with an empty body and therefore an allow-everything ruleset.
        assert_eq!(
            decode_robots_body("# café".as_bytes(), "made-up-9000"),
            None
        );
        assert_eq!(decode_robots_body(b"x", ""), None);
        // Registered, but not a *text* encoding: `bytes.decode("base64")` raises too.
        assert_eq!(decode_robots_body(b"eA==", "base64"), None);
    }

    #[test]
    fn codec_labels_normalise_like_python() {
        assert_eq!(python_codec_module("UTF-8"), Some("utf_8"));
        assert_eq!(python_codec_module("utf 8"), Some("utf_8"));
        assert_eq!(python_codec_module("utf--8"), Some("utf_8"));
        assert_eq!(python_codec_module("  utf-8  "), Some("utf_8"));
        assert_eq!(python_codec_module("...utf..8..."), None);
        assert_eq!(python_codec_module("L1"), Some("latin_1"));
        assert_eq!(python_codec_module("ISO-8859-1"), Some("latin_1"));
        assert_eq!(python_codec_module("windows-1252"), Some("cp1252"));
        assert_eq!(python_codec_module("utf_16_le"), Some("utf_16_le"));
    }

    #[test]
    fn utf16_bodies_decode() {
        let body: Vec<u8> = "User-agent: *"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(
            decode_robots_body(&body, "utf-16-le").as_deref(),
            Some("User-agent: *")
        );
        let mut with_bom = vec![0xFF, 0xFE];
        with_bom.extend_from_slice(&body);
        assert_eq!(
            decode_robots_body(&with_bom, "utf-16").as_deref(),
            Some("User-agent: *")
        );
    }

    // -- the manager ------------------------------------------------------

    /// Records every URL it is asked for and replies from a table.
    struct RecordingFetcher {
        bodies: Vec<(String, u16, Vec<u8>, String)>,
        calls: Mutex<Vec<String>>,
        failures: AtomicUsize,
    }

    impl RecordingFetcher {
        fn new(bodies: &[(&str, u16, &str)]) -> Self {
            Self {
                bodies: bodies
                    .iter()
                    .map(|(url, status, body)| {
                        (
                            (*url).to_owned(),
                            *status,
                            (*body).as_bytes().to_vec(),
                            "utf-8".to_owned(),
                        )
                    })
                    .collect(),
                calls: Mutex::new(Vec::new()),
                failures: AtomicUsize::new(0),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().expect("not poisoned").clone()
        }
    }

    #[async_trait]
    impl RobotsFetcher for RecordingFetcher {
        async fn fetch_robots(&self, url: &str, _sid: &str) -> Result<FetchedResponse> {
            self.calls
                .lock()
                .expect("not poisoned")
                .push(url.to_owned());
            for (candidate, status, body, encoding) in &self.bodies {
                if candidate == url {
                    let mut response = FetchedResponse::new(*status, url, body.clone());
                    response.encoding.clone_from(encoding);
                    return Ok(response);
                }
            }
            self.failures.fetch_add(1, Ordering::Relaxed);
            Err(crate::error::SpiderError::Fetch {
                sid: String::new(),
                url: url.to_owned(),
                message: "no route".to_owned(),
            })
        }
    }

    fn manager(bodies: &[(&str, u16, &str)]) -> (Arc<RecordingFetcher>, RobotsTxtManager) {
        let fetcher = Arc::new(RecordingFetcher::new(bodies));
        let manager = RobotsTxtManager::new(Arc::clone(&fetcher) as Arc<dyn RobotsFetcher>);
        (fetcher, manager)
    }

    #[tokio::test]
    async fn one_fetch_per_domain_regardless_of_path() {
        let (fetcher, manager) = manager(&[(
            "https://example.com/robots.txt",
            200,
            "User-agent: *\nDisallow: /admin/\n",
        )]);
        assert!(!manager.can_fetch("https://example.com/admin/x", "s").await);
        assert!(manager.can_fetch("https://example.com/products", "s").await);
        assert_eq!(
            manager
                .get_delay_directives("https://example.com/", "s")
                .await,
            (None, None)
        );
        assert_eq!(fetcher.calls(), vec!["https://example.com/robots.txt"]);
    }

    #[tokio::test]
    async fn distinct_domains_and_ports_fetch_separately() {
        let (fetcher, manager) = manager(&[
            (
                "https://a.com/robots.txt",
                200,
                "User-agent: *\nDisallow: /\n",
            ),
            ("https://a.com:8080/robots.txt", 200, ""),
            ("http://a.com/robots.txt", 200, ""),
        ]);
        assert!(!manager.can_fetch("https://a.com/x", "s").await);
        assert!(manager.can_fetch("https://a.com:8080/x", "s").await);
        // Same netloc, different scheme => the cached entry is reused, no second fetch.
        assert!(!manager.can_fetch("http://a.com/x", "s").await);
        assert_eq!(
            fetcher.calls(),
            vec!["https://a.com/robots.txt", "https://a.com:8080/robots.txt"]
        );
    }

    #[tokio::test]
    async fn scheme_is_preserved_and_defaults_to_https() {
        let (explicit_fetcher, explicit) = manager(&[("http://a.com/robots.txt", 200, "")]);
        explicit.parser_for("http://a.com/page", "s").await;
        assert_eq!(explicit_fetcher.calls(), vec!["http://a.com/robots.txt"]);

        let (implicit_fetcher, implicit) = manager(&[]);
        implicit.parser_for("//b.com/page", "s").await;
        assert_eq!(implicit_fetcher.calls(), vec!["https://b.com/robots.txt"]);
    }

    #[tokio::test]
    async fn every_failure_degrades_to_permissive() {
        for status in [403, 404, 500, 503, 301] {
            let (_, manager) = manager(&[(
                "https://example.com/robots.txt",
                status,
                "User-agent: *\nDisallow: /\n",
            )]);
            assert!(
                manager.can_fetch("https://example.com/anything", "s").await,
                "status {status}"
            );
            assert_eq!(
                manager
                    .get_delay_directives("https://example.com/", "s")
                    .await,
                (None, None)
            );
        }
    }

    #[tokio::test]
    async fn a_raising_fetcher_degrades_to_permissive() {
        let (fetcher, manager) = manager(&[]);
        assert!(manager.can_fetch("https://example.com/anything", "s").await);
        assert_eq!(
            manager
                .get_delay_directives("https://example.com/", "s")
                .await,
            (None, None)
        );
        assert_eq!(fetcher.failures.load(Ordering::Relaxed), 1);
        // The failed lookup is still cached, so it is not retried.
        assert_eq!(fetcher.calls().len(), 1);
    }

    #[tokio::test]
    async fn delay_directives_come_from_one_lookup() {
        let (fetcher, manager) = manager(&[(
            "https://example.com/robots.txt",
            200,
            "User-agent: *\nCrawl-delay: 2\nRequest-rate: 1/10\n",
        )]);
        assert_eq!(
            manager
                .get_delay_directives("https://example.com/x", "s")
                .await,
            (Some(2.0), Some((1, 10)))
        );
        assert_eq!(fetcher.calls().len(), 1);
    }

    #[tokio::test]
    async fn body_is_decoded_with_the_declared_encoding() {
        struct Latin1;
        #[async_trait]
        impl RobotsFetcher for Latin1 {
            async fn fetch_robots(&self, url: &str, _sid: &str) -> Result<FetchedResponse> {
                let body = b"# caf\xe9\nUser-agent: *\nDisallow: /caf\xe9\n".to_vec();
                let mut response = FetchedResponse::new(200, url, body);
                response.encoding = "latin-1".to_owned();
                Ok(response)
            }
        }
        let manager = RobotsTxtManager::new(Arc::new(Latin1));
        assert!(!manager.can_fetch("https://example.com/café", "s").await);
    }

    #[tokio::test]
    async fn prefetch_warms_every_domain_concurrently() {
        let (fetcher, manager) = manager(&[
            (
                "https://a.com/robots.txt",
                200,
                "User-agent: *\nDisallow: /\n",
            ),
            ("https://b.com/robots.txt", 200, ""),
            ("https://c.com/robots.txt", 200, ""),
        ]);
        let seeds = vec![
            "https://a.com/".to_owned(),
            "https://b.com/".to_owned(),
            "https://c.com/".to_owned(),
        ];
        manager.prefetch(&seeds, "s").await;
        let mut calls = fetcher.calls();
        calls.sort();
        assert_eq!(
            calls,
            vec![
                "https://a.com/robots.txt",
                "https://b.com/robots.txt",
                "https://c.com/robots.txt"
            ]
        );
        assert_eq!(manager.len(), 3);
        // The cache is warm: no further fetches.
        assert!(!manager.can_fetch("https://a.com/x", "s").await);
        assert_eq!(fetcher.calls().len(), 3);
    }

    #[tokio::test]
    async fn empty_prefetch_is_a_no_op() {
        let (fetcher, manager) = manager(&[]);
        manager.prefetch(&[], "s").await;
        assert!(fetcher.calls().is_empty());
        assert!(manager.is_empty());
    }

    #[tokio::test]
    async fn closure_fetchers_work() {
        let fetcher = fetcher_fn(|url: String, _sid: String| async move {
            Ok(FetchedResponse::new(
                200,
                url,
                "User-agent: *\nDisallow: /nope",
            ))
        });
        let manager = RobotsTxtManager::new(Arc::new(fetcher));
        assert!(!manager.can_fetch("https://example.com/nope", "s").await);
        assert!(manager.can_fetch("https://example.com/yes", "s").await);
    }

    #[tokio::test]
    async fn clear_and_insert() {
        let (_, manager) = manager(&[]);
        manager.insert(
            "example.com",
            RobotsRules::parse("User-agent: *\nDisallow: /\n"),
        );
        assert!(manager.is_cached("example.com"));
        assert!(!manager.can_fetch("https://example.com/x", "s").await);
        manager.clear();
        assert!(manager.is_empty());
        // Now the (empty) fetcher fails and everything is allowed again.
        assert!(manager.can_fetch("https://example.com/x", "s").await);
    }

    #[tokio::test]
    async fn sitemaps_are_exposed_through_the_manager() {
        let (_, manager) = manager(&[(
            "https://example.com/robots.txt",
            200,
            "Sitemap: https://example.com/sitemap.xml\n",
        )]);
        assert_eq!(
            manager.sitemaps("https://example.com/", "s").await,
            vec!["https://example.com/sitemap.xml".to_owned()]
        );
    }
}
