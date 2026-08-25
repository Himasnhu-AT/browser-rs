//! `parse_retry_after` and [`AutoThrottle`], with per-domain entry locking around the
//! read-modify-write.
//!
//! Port of `reference/spiders/throttle.py` (spec: `docs/specs/spider-support.md` §5).
//!
//! # Why the locking matters
//!
//! Python's engine is a single-threaded cooperative loop, so
//! `AutoThrottle.record` — a read of `self.delays[domain]`, an arithmetic step and a write
//! back — can never interleave with another `record` for the same domain. The Rust engine
//! runs its request tasks on a multi-threaded runtime, so the same code with a plain
//! `HashMap` behind an `RwLock` would let two blocked responses read the same `current`
//! and both write `current * 2`, collapsing the pinned doubling sequence
//! `[2, 4, 8, 16, 32]` into `[2, 2, 4, 4, 8]`.
//!
//! The fix is *not* one global mutex — that would serialise every domain against every
//! other domain for the whole of `record`. [`AutoThrottle`] keeps its delays in a
//! [`DashMap`] and does the whole read-modify-write while holding **that key's** entry
//! guard, so two domains never contend and one domain is strictly serialised.
//!
//! Consequence to be aware of when reading the code below: [`AutoThrottle::record`] must
//! **not** call [`AutoThrottle::delay_for`] the way Python does, because taking a second
//! `DashMap` guard for the same key while the first is alive deadlocks. The seeding step
//! is factored into [`AutoThrottle::seed_delay`] instead, and both entry points go through
//! exactly one guard.
//!
//! # Clocks
//!
//! `latency` comes from a monotonic clock (`Instant`) that the engine owns; the HTTP-date
//! branch of [`parse_retry_after`] is wall-clock (`chrono::Utc::now`). The two are
//! deliberately different sources — see `docs/specs/spider-support.md` §14.6.11.

use std::collections::BTreeMap;
use std::str::FromStr;

use dashmap::DashMap;
use indexmap::IndexMap;

/// How much a blocked response multiplies a domain's delay by when the site did not send a
/// `Retry-After` header.
///
/// Python: `throttle.BLOCK_BACKOFF_FACTOR`.
pub const BLOCK_BACKOFF_FACTOR: f64 = 2.0;

/// The header [`parse_retry_after`] looks for, already lower-cased.
const RETRY_AFTER: &str = "retry-after";

// ---------------------------------------------------------------------------
// parse_retry_after
// ---------------------------------------------------------------------------

/// How many seconds a `Retry-After` header asks the spider to wait.
///
/// The lookup is case-insensitive and takes the **first** matching key in iteration
/// order, exactly like Python's
/// `next((headers[key] for key in headers if key.lower() == "retry-after"), "")`.
///
/// The value is then read as, in order:
///
/// 1. a number of seconds, clamped at `0.0` — so `"-5"` is `0.0`, not `-5.0`;
/// 2. an HTTP / RFC-2822 date, as "seconds from now", also clamped at `0.0`;
/// 3. `None`.
///
/// ```
/// use indexmap::IndexMap;
/// use browser_spider::throttle::parse_retry_after;
///
/// let mut headers = IndexMap::new();
/// headers.insert("RETRY-AFTER".to_owned(), "  120 ".to_owned());
/// assert_eq!(parse_retry_after(&headers), Some(120.0));
///
/// headers.clear();
/// headers.insert("Retry-After".to_owned(), "Wed, 21 Oct 2015 07:28:00 GMT".to_owned());
/// assert_eq!(parse_retry_after(&headers), Some(0.0)); // in the past
///
/// headers.clear();
/// headers.insert("Content-Type".to_owned(), "text/html".to_owned());
/// assert_eq!(parse_retry_after(&headers), None);
/// ```
#[must_use]
pub fn parse_retry_after(headers: &IndexMap<String, String>) -> Option<f64> {
    parse_retry_after_iter(headers.iter().map(|(k, v)| (k.as_str(), v.as_str())))
}

/// [`parse_retry_after`] over any iterator of header pairs.
///
/// Lets a caller that holds an `http::HeaderMap`, a `Vec<(String, String)>` or a
/// `BTreeMap` reuse the same lookup without building an [`IndexMap`] first. Iteration
/// order is the caller's, and the first case-insensitive match wins.
pub fn parse_retry_after_iter<'a, I>(headers: I) -> Option<f64>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let raw = headers
        .into_iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(RETRY_AFTER))
        .map_or("", |(_, value)| value);
    parse_retry_after_value(raw)
}

/// The value half of [`parse_retry_after`], for a caller that already has the header.
///
/// ```
/// use browser_spider::throttle::parse_retry_after_value;
///
/// assert_eq!(parse_retry_after_value("120"), Some(120.0));
/// assert_eq!(parse_retry_after_value("-5"), Some(0.0));
/// assert_eq!(parse_retry_after_value("1e3"), Some(1000.0));
/// assert_eq!(parse_retry_after_value("12 seconds"), None);
/// assert_eq!(parse_retry_after_value("   "), None);
/// ```
#[must_use]
pub fn parse_retry_after_value(raw: &str) -> Option<f64> {
    let value = python_strip(raw);
    if value.is_empty() {
        return None;
    }

    // Python: `max(float(value), 0.0)`. `float()` accepts `1e3`, `inf` and `nan`, so those
    // reach the caller — the spec calls this out explicitly (§5.2).
    if let Some(seconds) = python_float(value) {
        return Some(python_max(seconds, 0.0));
    }

    match http_date_seconds_from_now(value) {
        Some(seconds) => Some(python_max(seconds, 0.0)),
        None => {
            tracing::debug!(value, "Ignoring an unreadable `Retry-After` header");
            None
        }
    }
}

/// Seconds between now and an RFC-2822 / HTTP date, negative when the date is past.
///
/// Python calls `email.utils.parsedate_to_datetime`, which is a thin wrapper over
/// `email._parseaddr._parsedate_tz`. That function is **much** more permissive than either
/// `chrono`'s RFC-2822 reader or `httpdate`: it ignores the day-of-week entirely (so a
/// `Retry-After: Mon, 21 Oct 2015 …` on a Wednesday still parses), drops trailing junk,
/// accepts a two-digit year, a missing seconds field, the RFC-850 `dd-mmm-yy` form with a
/// two *or* four digit year, `asctime`'s `Mmm dd hh:mm:ss yyyy` word order, and the whole
/// obsolete zone-name table. Delegating to a strict parser silently answered `None` for
/// half of those, so [`parsedate_tz`] below is a direct port instead.
///
/// The naive/aware rule is the other half of the contract: `parsedate_to_datetime` returns
/// a **naive** `datetime` when the zone is missing or is RFC 2822's "unknown local offset"
/// spelling `-0000`, and subtracting an aware `datetime.now(timezone.utc)` from a naive one
/// raises `TypeError` — which `parse_retry_after` turns into `None`.
fn http_date_seconds_from_now(value: &str) -> Option<f64> {
    let parsed = parsedate_tz(value)?;
    // `tz is None` -> naive -> `TypeError` in Python -> unreadable.
    let offset_seconds = parsed.tz_offset_seconds?;
    // `datetime.timezone(timedelta(seconds=tz))` rejects anything not strictly inside
    // ±24h, so `+9999` (which is 100h39m once the HHMM is expanded) is a `ValueError`.
    if !(-86_399..=86_399).contains(&offset_seconds) {
        return None;
    }
    // `datetime.datetime(*dtuple[:6])` validates the calendar fields and raises for
    // year 0, month 13, day 0, hour 24 and so on. `datetime` also only spans years
    // 1..=9999, which is narrower than `NaiveDate`.
    if !(1..=9999).contains(&parsed.year) {
        return None;
    }
    let naive = chrono::NaiveDate::from_ymd_opt(parsed.year, parsed.month, parsed.day)?
        .and_hms_opt(parsed.hour, parsed.minute, parsed.second)?;
    let when = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(naive, chrono::Utc)
        .checked_sub_signed(chrono::TimeDelta::try_seconds(i64::from(offset_seconds))?)?;

    let delta = when.signed_duration_since(chrono::Utc::now());
    // Microseconds, to match `timedelta.total_seconds()`'s resolution exactly.
    // `num_nanoseconds` would overflow past ~year 2262; microseconds do not.
    Some(delta.num_microseconds()? as f64 / 1_000_000.0)
}

/// What CPython's `email._parseaddr._parsedate_tz` extracts from a date header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ParsedDate {
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    /// `None` is Python's "no usable zone", which makes the resulting `datetime` naive.
    tz_offset_seconds: Option<i32>,
}

/// The three-letter day names `_parsedate_tz` skips over, lower-cased.
const DAY_NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// `_monthnames`: the three-letter abbreviations followed by the full names, so
/// `index(name) + 1` is `1..=24` and anything above 12 has 12 subtracted from it.
const MONTH_NAMES: [&str; 24] = [
    "jan",
    "feb",
    "mar",
    "apr",
    "may",
    "jun",
    "jul",
    "aug",
    "sep",
    "oct",
    "nov",
    "dec",
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

/// `_timezones`: the obsolete alphabetic zone names, in hundreds-of-hours (`-500` is EST).
const TIMEZONE_NAMES: [(&str, i32); 14] = [
    ("UT", 0),
    ("UTC", 0),
    ("GMT", 0),
    ("Z", 0),
    ("AST", -400),
    ("ADT", -300),
    ("EST", -500),
    ("EDT", -400),
    ("CST", -600),
    ("CDT", -500),
    ("MST", -700),
    ("MDT", -600),
    ("PST", -800),
    ("PDT", -700),
];

/// A direct port of CPython's `email._parseaddr._parsedate_tz`.
///
/// Every branch below mirrors one in the Python, including the ones that look like bugs:
/// the day-of-week is thrown away without being checked, tokens past the fifth are
/// discarded, a `yy`/`dd` that does not look like a month is swapped with `mm`, and a
/// numeric zone of exactly `-0000` becomes `None` ("UTC, source zone unknown") while
/// `+0000` becomes `Some(0)`.
fn parsedate_tz(data: &str) -> Option<ParsedDate> {
    if data.is_empty() {
        return None;
    }
    // `str.split()` with no argument: split on runs of Python whitespace, no empty pieces.
    let mut parts: Vec<String> = data
        .split(python_is_space)
        .filter(|piece| !piece.is_empty())
        .map(str::to_owned)
        .collect();
    if parts.is_empty() {
        return None;
    }

    // The FWS after the day-of-week's comma is optional, so this handles both spellings.
    if parts[0].ends_with(',') || DAY_NAMES.contains(&parts[0].to_lowercase().as_str()) {
        parts.remove(0);
    } else if let Some(comma) = parts[0].rfind(',') {
        parts[0] = parts[0][comma + 1..].to_owned();
    }

    if parts.len() == 3 {
        // RFC 850's `Wednesday, 21-Oct-15 07:28:00 GMT`, deprecated but still seen.
        let stuff: Vec<String> = parts[0].split('-').map(str::to_owned).collect();
        if stuff.len() == 3 {
            let mut rebuilt = stuff;
            rebuilt.extend_from_slice(&parts[1..]);
            parts = rebuilt;
        }
    }
    if parts.len() == 4 {
        // A zone glued to the time, as in `07:28:00+0200`.
        let s = parts[3].clone();
        let index = s.find('+').or_else(|| s.find('-'));
        match index {
            Some(at) if at > 0 => {
                parts.truncate(3);
                parts.push(s[..at].to_owned());
                parts.push(s[at..].to_owned());
            }
            _ => parts.push(String::new()), // Dummy tz.
        }
    }
    if parts.len() < 5 {
        return None;
    }
    parts.truncate(5);

    let mut dd = parts[0].clone();
    let mut mm = parts[1].clone();
    let mut yy = parts[2].clone();
    let mut tm = parts[3].clone();
    let mut tz = parts[4].clone();

    if dd.is_empty() || mm.is_empty() || yy.is_empty() {
        return None;
    }
    mm = mm.to_lowercase();
    let mut month_index = MONTH_NAMES.iter().position(|name| *name == mm);
    if month_index.is_none() {
        // `asctime`'s `Oct 21 …` word order: the month is where the day should be.
        std::mem::swap(&mut dd, &mut mm);
        mm = mm.to_lowercase();
        month_index = MONTH_NAMES.iter().position(|name| *name == mm);
        month_index?;
    }
    let mut month = month_index? + 1;
    if month > 12 {
        month -= 12;
    }

    if dd.ends_with(',') {
        dd.pop();
    }
    if yy.find(':').is_some_and(|at| at > 0) {
        std::mem::swap(&mut yy, &mut tm);
    }
    if yy.ends_with(',') {
        yy.pop();
        if yy.is_empty() {
            return None;
        }
    }
    if !yy.chars().next().is_some_and(python_is_digit) {
        std::mem::swap(&mut yy, &mut tz);
    }
    if tm.ends_with(',') {
        tm.pop();
    }

    let pieces: Vec<&str> = tm.split(':').collect();
    let (hh_raw, mm_raw, ss_raw): (String, String, String) = match pieces.len() {
        2 => (pieces[0].to_owned(), pieces[1].to_owned(), "0".to_owned()),
        3 => (
            pieces[0].to_owned(),
            pieces[1].to_owned(),
            pieces[2].to_owned(),
        ),
        // Some non-compliant senders separate the time with `.` instead of `:`.
        1 if pieces[0].contains('.') => {
            let dotted: Vec<&str> = pieces[0].split('.').collect();
            match dotted.len() {
                2 => (dotted[0].to_owned(), dotted[1].to_owned(), "0".to_owned()),
                3 => (
                    dotted[0].to_owned(),
                    dotted[1].to_owned(),
                    dotted[2].to_owned(),
                ),
                _ => return None,
            }
        }
        _ => return None,
    };

    // Python's `int()`, so Unicode decimal digits and `_` separators are accepted here too.
    let year = python_int(&yy)?;
    let day = python_int(&dd)?;
    let hour = python_int(&hh_raw)?;
    let minute = python_int(&mm_raw)?;
    let second = python_int(&ss_raw)?;

    let year = if year < 100 {
        // POSIX's two-digit-year window: 69..=99 is 1969..=1999, 0..=68 is 2000..=2068.
        if year > 68 {
            year + 1900
        } else {
            year + 2000
        }
    } else {
        year
    };

    let upper = tz.to_uppercase();
    tz = upper;
    let mut tz_offset: Option<i64> = TIMEZONE_NAMES
        .iter()
        .find(|(name, _)| *name == tz)
        .map(|(_, offset)| i64::from(*offset));
    if tz_offset.is_none() {
        tz_offset = python_int(&tz);
        // `-0000` is "UTC, but the source zone is unknown", which Python models as naive.
        if tz_offset == Some(0) && tz.starts_with('-') {
            tz_offset = None;
        }
    }
    // Python only expands a *truthy* offset, so a zero stays zero rather than being
    // re-derived from `0 // 100`.
    let tz_offset_seconds = tz_offset.map(|offset| {
        if offset == 0 {
            return 0i64;
        }
        let sign = if offset < 0 { -1 } else { 1 };
        let magnitude = offset.abs();
        sign * ((magnitude / 100) * 3600 + (magnitude % 100) * 60)
    });

    Some(ParsedDate {
        year: i32::try_from(year).ok()?,
        month: u32::try_from(month).ok()?,
        day: u32::try_from(day).ok()?,
        hour: u32::try_from(hour).ok()?,
        minute: u32::try_from(minute).ok()?,
        second: u32::try_from(second).ok()?,
        tz_offset_seconds: match tz_offset_seconds {
            None => None,
            Some(seconds) => Some(i32::try_from(seconds).ok()?),
        },
    })
}

// ---------------------------------------------------------------------------
// Python numeric-literal helpers
// ---------------------------------------------------------------------------

/// CPython's two-argument `max`.
///
/// `f64::max` treats NaN as "missing" and returns the other operand; Python's `max(a, b)`
/// is a plain `b if b > a else a`, so `max(nan, 0.0)` is `nan` there and `0.0` here. A
/// `Retry-After: nan` really does reach [`AutoThrottle::record`] as a NaN penalty in
/// Python, so the port keeps the semantics rather than silently sanitising them.
pub(crate) fn python_max(left: f64, right: f64) -> f64 {
    if right > left {
        right
    } else {
        left
    }
}

/// CPython's two-argument `min`. See [`python_max`].
fn python_min(left: f64, right: f64) -> f64 {
    if right < left {
        right
    } else {
        left
    }
}

/// CPython's `str.isspace()` for a single character.
///
/// **Not** the same set as Rust's `char::is_whitespace`, which is the Unicode `White_Space`
/// property: CPython additionally treats the four C0 separators `\x1c` (file), `\x1d`
/// (group), `\x1e` (record) and `\x1f` (unit) as whitespace, because they carry the
/// bidirectional `B`/`S` class. A `Retry-After: "\x1c5"` therefore strips to `"5"` and
/// parses as five seconds in Python, and used to answer `None` here.
pub(crate) fn python_is_space(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '\u{1c}'..='\u{1f}')
}

/// Python's `str.strip()` — whitespace by [`python_is_space`], both ends.
pub(crate) fn python_strip(text: &str) -> &str {
    text.trim_matches(python_is_space)
}

/// The first code point of every contiguous run of ten Unicode `Nd` decimal digits.
///
/// `Nd` is laid out as runs of exactly ten code points whose numeric values ascend `0..=9`,
/// so a digit's value is `ch - start`. ASCII `0x30` is handled before this table is
/// consulted. Generated from the reference interpreter's `unicodedata` (Unicode 16.0);
/// a newer Unicode adds runs at the end, which only ever *adds* accepted spellings.
#[rustfmt::skip]
const ND_DIGIT_RUN_STARTS: [u32; 75] = [
    0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66,
    0x0BE6, 0x0C66, 0x0CE6, 0x0D66, 0x0DE6, 0x0E50, 0x0ED0, 0x0F20,
    0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80, 0x1A90,
    0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0,
    0xA9F0, 0xAA50, 0xABF0, 0xFF10, 0x104A0, 0x10D30, 0x10D40, 0x11066,
    0x110F0, 0x11136, 0x111D0, 0x112F0, 0x11450, 0x114D0, 0x11650, 0x116C0,
    0x116D0, 0x116DA, 0x11730, 0x118E0, 0x11950, 0x11BF0, 0x11C50, 0x11D50,
    0x11DA0, 0x11F50, 0x16130, 0x16A60, 0x16AC0, 0x16B50, 0x16D70, 0x1CCF0,
    0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0,
    0x1E5F1, 0x1E950, 0x1FBF0,
];

/// The `0..=9` value of a Unicode decimal digit, ASCII included.
///
/// Equivalent to `unicodedata.decimal(ch)` restricted to category `Nd`.
pub(crate) fn nd_digit_value(ch: char) -> Option<u32> {
    let code = u32::from(ch);
    if ch.is_ascii_digit() {
        return Some(code - u32::from(b'0'));
    }
    ND_DIGIT_RUN_STARTS
        .iter()
        .find(|start| (**start..**start + 10).contains(&code))
        .map(|start| code - start)
}

/// CPython's `str.isdigit()`, near enough for the parsers here.
///
/// CPython's predicate is `Numeric_Type` in `{Decimal, Digit}`, which additionally covers a
/// handful of `No` superscripts such as `²`. This is the `Nd` half — the half that can
/// actually be converted by `int()`/`float()`, and the only half these parsers go on to use.
pub(crate) fn python_is_digit(ch: char) -> bool {
    nd_digit_value(ch).is_some()
}

/// CPython's `_PyUnicode_TransformDecimalAndSpaceToASCII`.
///
/// Every numeric conversion from a `str` — `int()`, `float()`, `complex()` — runs the
/// string through this first, which is why `float("٣.٥")` is `3.5` and `int("١٢")` is `12`.
/// It maps each Unicode decimal digit to its ASCII twin and each whitespace character to a
/// plain space (so an embedded `\u{a0}` becomes a space and then fails to parse, exactly as
/// a plain space would).
///
/// Returns the input untouched when it is already ASCII-clean, so the common path does not
/// allocate.
pub(crate) fn transform_decimal_and_space(text: &str) -> std::borrow::Cow<'_, str> {
    let needs_work = text
        .chars()
        .any(|ch| !ch.is_ascii() && (python_is_space(ch) || nd_digit_value(ch).is_some()));
    if !needs_work {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else if let Some(value) = nd_digit_value(ch) {
            // `nd_digit_value` is `0..=9` by construction, so `from_digit` always
            // succeeds; the fallback keeps the original character rather than inventing a
            // digit if that ever stops being true.
            debug_assert!(value < 10, "nd_digit_value returned {value}");
            out.push(char::from_digit(value, 10).unwrap_or(ch));
        } else if python_is_space(ch) {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    std::borrow::Cow::Owned(out)
}

/// CPython's `float(str)`.
///
/// The differences from `f64::from_str` that matter here:
///
/// * Python strips surrounding whitespace — and by [`python_is_space`]'s definition of it,
///   which is wider than Rust's; Rust's `from_str` rejects whitespace outright.
/// * Python accepts `_` between digits (`float("1_0") == 10.0`); Rust rejects it.
/// * Python accepts any Unicode decimal digit (`float("٣.٥") == 3.5`), because every
///   string-to-number conversion runs [`transform_decimal_and_space`] first.
/// * Python accepts `"infinity"`, `"inf"` and `"nan"` case-insensitively, as does Rust.
pub(crate) fn python_float(text: &str) -> Option<f64> {
    let transformed = transform_decimal_and_space(text);
    let stripped = python_strip(&transformed);
    if stripped.is_empty() {
        return None;
    }
    let cleaned = strip_python_underscores(stripped)?;
    // Rust's grammar is otherwise a superset match: `[+-]?(inf|infinity|nan|<significand>[e<exp>])`.
    f64::from_str(&cleaned).ok()
}

/// CPython's `int(str)`, base 10.
///
/// Same underscore, whitespace and Unicode-digit rules as [`python_float`], but no
/// exponent, no decimal point and no `inf`/`nan`.
///
/// Values outside `i64` are rejected; Python's ints are unbounded,
/// so a `Request-rate: 99999999999999999999/1` is ignored here and honoured there. No
/// corpus row exercises it — a `Request-rate` that large is meaningless — but it is a real
/// difference and is called out rather than hidden.
pub(crate) fn python_int(text: &str) -> Option<i64> {
    let transformed = transform_decimal_and_space(text);
    let stripped = python_strip(&transformed);
    if stripped.is_empty() {
        return None;
    }
    let cleaned = strip_python_underscores(stripped)?;
    let (sign, digits) = match cleaned.strip_prefix('-') {
        Some(rest) => (-1i64, rest),
        None => (1i64, cleaned.strip_prefix('+').unwrap_or(&cleaned)),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits
        .parse::<i64>()
        .ok()
        .and_then(|magnitude| magnitude.checked_mul(sign))
}

/// Remove `_` separators, rejecting the string outright when one is misplaced.
///
/// Python only allows an underscore *between* two digits, so `"_1"`, `"1_"` and `"1__2"`
/// are all `ValueError`s rather than `1`.
fn strip_python_underscores(text: &str) -> Option<String> {
    if !text.contains('_') {
        return Some(text.to_owned());
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (index, &ch) in chars.iter().enumerate() {
        if ch != '_' {
            out.push(ch);
            continue;
        }
        let before = index.checked_sub(1).and_then(|i| chars.get(i))?;
        let after = chars.get(index + 1)?;
        if !before.is_ascii_digit() || !after.is_ascii_digit() {
            return None;
        }
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// AutoThrottle
// ---------------------------------------------------------------------------

/// The two constructor guards Python raises `ValueError` for.
///
/// Kept module-local rather than added to [`crate::error::SpiderError`] because work unit 17
/// is editing that enum in the same wave; unit 20 may fold it in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum AutoThrottleConfigError {
    /// `target_concurrency` was zero, negative or NaN.
    ///
    /// Python: ``ValueError("`target_concurrency` must be higher than 0")``.
    #[error("`target_concurrency` must be higher than 0")]
    TargetConcurrency,

    /// `max_delay` was below `start_delay`, which would make the seed delay unreachable.
    ///
    /// Python: ``ValueError("`autothrottle_max_delay` can't be lower than
    /// `autothrottle_start_delay`")``.
    #[error("`autothrottle_max_delay` can't be lower than `autothrottle_start_delay`")]
    MaxDelayBelowStartDelay,
}

/// The four knobs `AutoThrottle.__init__` takes, with Python's defaults.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AutoThrottleConfig {
    /// The delay used for the first request to a domain. Python default `5.0`.
    pub start_delay: f64,
    /// The highest delay the throttle may reach, `Retry-After` included. Python default `60.0`.
    pub max_delay: f64,
    /// How many requests the spider aims to have in flight per domain. Python default `1.0`.
    pub target_concurrency: f64,
    /// Double the delay (or honour `Retry-After`) when a domain blocks us. Python default `true`.
    pub block_backoff: bool,
}

impl Default for AutoThrottleConfig {
    fn default() -> Self {
        Self {
            start_delay: 5.0,
            max_delay: 60.0,
            target_concurrency: 1.0,
            block_backoff: true,
        }
    }
}

impl AutoThrottleConfig {
    /// The engine's resolution order for `target_concurrency` (spec §5.5):
    /// `autothrottle_target_concurrency or concurrent_requests_per_domain or 1.0`.
    ///
    /// Python's `or` chain treats both `None` **and** `0` as falsy, which is what the three
    /// `test_target_concurrency_*` tests pin — so a `Some(0.0)` falls through to the next
    /// candidate instead of raising.
    ///
    /// ```
    /// use browser_spider::throttle::AutoThrottleConfig;
    ///
    /// assert_eq!(AutoThrottleConfig::resolve_target_concurrency(Some(4.0), Some(8.0)), 4.0);
    /// assert_eq!(AutoThrottleConfig::resolve_target_concurrency(None, Some(8.0)), 8.0);
    /// assert_eq!(AutoThrottleConfig::resolve_target_concurrency(Some(0.0), None), 1.0);
    /// ```
    #[must_use]
    pub fn resolve_target_concurrency(autothrottle: Option<f64>, per_domain: Option<f64>) -> f64 {
        let truthy = |candidate: Option<f64>| candidate.filter(|value| *value != 0.0);
        truthy(autothrottle)
            .or_else(|| truthy(per_domain))
            .unwrap_or(1.0)
    }

    /// Validate the two guards, in Python's order.
    ///
    /// # Errors
    ///
    /// [`AutoThrottleConfigError::TargetConcurrency`] first, then
    /// [`AutoThrottleConfigError::MaxDelayBelowStartDelay`] — the order is observable
    /// because a config can violate both.
    pub fn validate(&self) -> Result<(), AutoThrottleConfigError> {
        // Python's guard is `target_concurrency <= 0`, which NaN slips through (`nan <= 0`
        // is False) — and a NaN divisor then poisons every delay for the rest of the
        // crawl. The port rejects NaN as well, which is the one deliberate difference in
        // this method; `tests/u16_throttle_golden.rs` pins it, and it belongs in
        // docs/DIVERGENCES.md when unit 20 consolidates the list.
        if !matches!(
            self.target_concurrency.partial_cmp(&0.0),
            Some(std::cmp::Ordering::Greater)
        ) {
            return Err(AutoThrottleConfigError::TargetConcurrency);
        }
        if self.max_delay < self.start_delay {
            return Err(AutoThrottleConfigError::MaxDelayBelowStartDelay);
        }
        Ok(())
    }
}

/// Per-domain adaptive delay, learned from observed latency.
///
/// A faithful port of `reference.spiders.throttle.AutoThrottle`, with the one change the
/// module docs describe: the delay map is a [`DashMap`] and every read-modify-write happens
/// under that domain's entry guard.
///
/// Every method takes `&self`, so the engine holds one `Arc<AutoThrottle>` and hands it to
/// each request task.
///
/// ```
/// use browser_spider::throttle::{AutoThrottle, AutoThrottleConfig};
///
/// let throttle = AutoThrottle::new(AutoThrottleConfig {
///     start_delay: 1.0,
///     max_delay: 60.0,
///     ..AutoThrottleConfig::default()
/// })
/// .expect("valid config");
///
/// // Five blocked responses double the delay each time.
/// let sequence: Vec<f64> = (0..5)
///     .map(|_| throttle.record("example.com", 0.05, false, 0.0, None))
///     .collect();
/// assert_eq!(sequence, vec![2.0, 4.0, 8.0, 16.0, 32.0]);
/// ```
#[derive(Debug)]
pub struct AutoThrottle {
    config: AutoThrottleConfig,
    /// Keyed by `Request::domain` — the netloc, port included.
    delays: DashMap<String, f64>,
}

impl AutoThrottle {
    /// Build a throttle, validating the config the way `__init__` does.
    ///
    /// # Errors
    ///
    /// See [`AutoThrottleConfig::validate`].
    pub fn new(config: AutoThrottleConfig) -> Result<Self, AutoThrottleConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            delays: DashMap::new(),
        })
    }

    /// The config this throttle was built with.
    #[must_use]
    pub fn config(&self) -> AutoThrottleConfig {
        self.config
    }

    /// The delay a never-seen domain starts at: `min(max(floor, start_delay), max_delay)`.
    ///
    /// A `floor` above `max_delay` is capped to `max_delay`, which is why
    /// `start_delay=5, max_delay=20, floor=100` seeds at `20.0`.
    #[must_use]
    pub fn seed_delay(&self, floor: f64) -> f64 {
        python_min(
            python_max(floor, self.config.start_delay),
            self.config.max_delay,
        )
    }

    /// The current delay for a domain, seeding it at [`AutoThrottle::seed_delay`] the first
    /// time it is asked for.
    ///
    /// The seeding is a write, so this is not a read-only method — it is the reason a
    /// domain's very first request already waits `start_delay`.
    pub fn delay_for(&self, domain: &str, floor: f64) -> f64 {
        if let Some(existing) = self.delays.get(domain) {
            return *existing;
        }
        let seed = self.seed_delay(floor);
        // Between the `get` and the `entry` another task may have seeded the domain; the
        // entry API resolves that by keeping whatever landed first, which is also what the
        // single-threaded Python does when two coroutines interleave at an await point.
        *self.delays.entry(domain.to_owned()).or_insert(seed)
    }

    /// Feed a finished request back in and return the domain's new delay.
    ///
    /// * `latency` — wall time the whole fetch took, retries and browser render included.
    /// * `ok` — "healthy": a 2xx that `is_blocked()` did not flag.
    /// * `floor` — the lowest delay allowed for this domain (`download_delay`, raised by
    ///   `robots.txt` `Crawl-delay` / `Request-rate`).
    /// * `retry_after` — what the site asked for, when it asked. Ignored entirely when
    ///   `block_backoff` is off.
    ///
    /// The whole read-modify-write runs under this domain's [`DashMap`] entry guard, so
    /// concurrent blocks on one domain compose (`2, 4, 8, …`) instead of racing.
    pub fn record(
        &self,
        domain: &str,
        latency: f64,
        ok: bool,
        floor: f64,
        retry_after: Option<f64>,
    ) -> f64 {
        let seed = self.seed_delay(floor);
        // One guard for the whole operation. `delay_for` is deliberately NOT reused here:
        // taking a second guard for the same key while this one is alive deadlocks DashMap.
        let mut entry = self.delays.entry(domain.to_owned()).or_insert(seed);

        let current = *entry;
        let target = latency / self.config.target_concurrency;
        // Exponential smoothing, except a latency spike applies immediately: the delay is
        // never averaged *down* below what the last request actually cost.
        let mut new = python_max((current + target) / 2.0, target);

        if !ok {
            let penalty = if self.config.block_backoff {
                retry_after.unwrap_or(current * BLOCK_BACKOFF_FACTOR)
            } else {
                current
            };
            // A block can never speed the spider up. Python's three-argument
            // `max(new, penalty, current)` folds left, which is what this chain does.
            new = python_max(python_max(new, penalty), current);
        }

        new = python_min(python_max(new, floor), self.config.max_delay);
        *entry = new;
        drop(entry);

        tracing::debug!(
            domain,
            latency,
            ok,
            previous_delay = current,
            new_delay = new,
            "AutoThrottle adjusted the domain delay"
        );
        new
    }

    /// Drop every learned delay. The engine calls this at the start of each crawl.
    pub fn reset(&self) {
        self.delays.clear();
    }

    /// Whether a domain has a learned delay yet.
    #[must_use]
    pub fn contains(&self, domain: &str) -> bool {
        self.delays.contains_key(domain)
    }

    /// How many domains have a learned delay.
    #[must_use]
    pub fn len(&self) -> usize {
        self.delays.len()
    }

    /// Whether no domain has a learned delay.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.delays.is_empty()
    }

    /// A snapshot of every learned delay, for `stats.autothrottle_delays`.
    ///
    /// Python hands the engine `dict(autothrottle.delays)`, which is insertion-ordered. A
    /// [`DashMap`] has no insertion order, so the snapshot is sorted by domain instead —
    /// a declared divergence that makes the stats dict deterministic rather than
    /// arrival-order dependent.
    #[must_use]
    pub fn delays(&self) -> BTreeMap<String, f64> {
        self.delays
            .iter()
            .map(|entry| (entry.key().clone(), *entry.value()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn throttle(start: f64, max: f64) -> AutoThrottle {
        AutoThrottle::new(AutoThrottleConfig {
            start_delay: start,
            max_delay: max,
            ..AutoThrottleConfig::default()
        })
        .expect("valid config")
    }

    fn headers(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    // -- parse_retry_after ------------------------------------------------

    #[test]
    fn missing_header_is_none() {
        assert_eq!(parse_retry_after(&headers(&[])), None);
        assert_eq!(
            parse_retry_after(&headers(&[("Content-Type", "text/html")])),
            None
        );
    }

    #[test]
    fn numeric_seconds() {
        assert_eq!(
            parse_retry_after(&headers(&[("Retry-After", "120")])),
            Some(120.0)
        );
    }

    #[test]
    fn header_lookup_is_case_insensitive_and_value_is_stripped() {
        for spelling in ["retry-after", "RETRY-AFTER", "Retry-After", "ReTrY-AfTeR"] {
            assert_eq!(
                parse_retry_after(&headers(&[(spelling, " 120 ")])),
                Some(120.0),
                "{spelling}"
            );
        }
    }

    #[test]
    fn first_matching_key_wins() {
        let map = headers(&[("Retry-After", "10"), ("retry-after", "20")]);
        assert_eq!(parse_retry_after(&map), Some(10.0));
    }

    #[test]
    fn negative_seconds_clamp_to_zero() {
        assert_eq!(parse_retry_after_value("-5"), Some(0.0));
        assert_eq!(parse_retry_after_value("-0.5"), Some(0.0));
    }

    #[test]
    fn python_float_spellings_are_accepted() {
        assert_eq!(parse_retry_after_value("1e3"), Some(1000.0));
        assert_eq!(parse_retry_after_value("inf"), Some(f64::INFINITY));
        assert!(parse_retry_after_value("nan").is_some_and(f64::is_nan));
        assert_eq!(parse_retry_after_value("1_0"), Some(10.0));
        assert_eq!(parse_retry_after_value(".5"), Some(0.5));
        assert_eq!(parse_retry_after_value("+3."), Some(3.0));
    }

    #[test]
    fn malformed_underscores_are_rejected() {
        assert_eq!(python_float("_1"), None);
        assert_eq!(python_float("1_"), None);
        assert_eq!(python_float("1__2"), None);
        assert_eq!(python_float("1_.2"), None);
    }

    #[test]
    fn unreadable_values_are_none() {
        for value in ["soon", "", "   ", "12 seconds", "0x10", "1,000"] {
            assert_eq!(parse_retry_after_value(value), None, "{value:?}");
        }
    }

    #[test]
    fn past_http_date_clamps_to_zero() {
        assert_eq!(
            parse_retry_after_value("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(0.0)
        );
    }

    #[test]
    fn future_http_date_is_seconds_from_now() {
        let when = chrono::Utc::now() + chrono::Duration::seconds(90);
        let formatted = when.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
        let seconds = parse_retry_after_value(&formatted).expect("an HTTP date");
        assert!(
            (seconds - 90.0).abs() < 2.0,
            "{formatted} -> {seconds}, wanted ~90"
        );
    }

    #[test]
    fn rfc2822_numeric_offset_is_accepted() {
        let when = chrono::Utc::now() + chrono::Duration::seconds(90);
        let formatted = when.format("%a, %d %b %Y %H:%M:%S +0000").to_string();
        let seconds = parse_retry_after_value(&formatted).expect("an RFC-2822 date");
        assert!((seconds - 90.0).abs() < 2.0, "{formatted} -> {seconds}");
    }

    #[test]
    fn unknown_local_offset_is_naive_and_rejected() {
        // Python's `parsedate_to_datetime` returns a naive datetime for `-0000`, and the
        // subtraction then raises `TypeError`.
        assert_eq!(
            parse_retry_after_value("Wed, 21 Oct 2035 07:28:00 -0000"),
            None
        );
    }

    #[test]
    fn rfc850_dates_parse_and_asctime_does_not() {
        assert_eq!(
            parse_retry_after_value("Wednesday, 21-Oct-15 07:28:00 GMT"),
            Some(0.0)
        );
        // No timezone => naive in Python => `TypeError` => `None`. `httpdate` would happily
        // parse this one, which is exactly why the fallback is gated on a trailing `GMT`.
        assert_eq!(parse_retry_after_value("Wed Oct 21 07:28:00 2015"), None);
        assert_eq!(parse_retry_after_value("Wed, 21 Oct 2015 07:28:00"), None);
    }

    // -- python_int -------------------------------------------------------

    #[test]
    fn python_int_matches_cpython() {
        assert_eq!(python_int(" 10 "), Some(10));
        assert_eq!(python_int("+10"), Some(10));
        assert_eq!(python_int("-10"), Some(-10));
        assert_eq!(python_int("1_0"), Some(10));
        assert_eq!(python_int("10.0"), None);
        assert_eq!(python_int("1e3"), None);
        assert_eq!(python_int(""), None);
        assert_eq!(python_int("99999999999999999999"), None);
    }

    // -- AutoThrottle config ---------------------------------------------

    #[test]
    fn constructor_guards_fire_in_order() {
        let bad_both = AutoThrottleConfig {
            start_delay: 10.0,
            max_delay: 1.0,
            target_concurrency: 0.0,
            block_backoff: true,
        };
        assert_eq!(
            bad_both.validate(),
            Err(AutoThrottleConfigError::TargetConcurrency)
        );
        assert_eq!(
            AutoThrottleConfig {
                start_delay: 10.0,
                max_delay: 1.0,
                ..AutoThrottleConfig::default()
            }
            .validate(),
            Err(AutoThrottleConfigError::MaxDelayBelowStartDelay)
        );
    }

    #[test]
    fn guard_messages_match_python() {
        assert_eq!(
            AutoThrottleConfigError::TargetConcurrency.to_string(),
            "`target_concurrency` must be higher than 0"
        );
        assert_eq!(
            AutoThrottleConfigError::MaxDelayBelowStartDelay.to_string(),
            "`autothrottle_max_delay` can't be lower than `autothrottle_start_delay`"
        );
    }

    #[test]
    fn python_isspace_covers_the_c0_separators() {
        // The four characters where CPython's `str.isspace()` and Rust's Unicode
        // `White_Space` disagree. A `Retry-After: "\x1c5"` is five seconds in Python.
        for ch in ['\u{1c}', '\u{1d}', '\u{1e}', '\u{1f}'] {
            assert!(python_is_space(ch), "{ch:?}");
            assert!(!ch.is_whitespace(), "{ch:?} is not Unicode White_Space");
        }
        assert_eq!(python_strip("\u{1c}5\u{1f}"), "5");
        assert_eq!(python_strip("\u{a0}\u{3000}5\t"), "5");
        assert!(!python_is_space('\u{200b}')); // ZERO WIDTH SPACE is not whitespace
    }

    #[test]
    fn unicode_decimal_digits_parse_like_python() {
        assert_eq!(nd_digit_value('7'), Some(7));
        assert_eq!(nd_digit_value('٥'), Some(5)); // ARABIC-INDIC DIGIT FIVE
        assert_eq!(nd_digit_value('１'), Some(1)); // FULLWIDTH DIGIT ONE
        assert_eq!(nd_digit_value('\u{1d7dd}'), Some(5)); // MATHEMATICAL DOUBLE-STRUCK
        assert_eq!(nd_digit_value('x'), None);
        assert_eq!(nd_digit_value('²'), None); // No, not Nd - and `float("²")` fails too

        assert_eq!(python_float("٣.٥"), Some(3.5));
        assert_eq!(python_float("１２３"), Some(123.0));
        assert_eq!(python_float("١e٢"), Some(100.0));
        assert_eq!(python_int("١٢"), Some(12));
        assert_eq!(python_float("\u{200b}5"), None);
    }

    #[test]
    fn nan_target_concurrency_is_rejected() {
        assert_eq!(
            AutoThrottleConfig {
                target_concurrency: f64::NAN,
                ..AutoThrottleConfig::default()
            }
            .validate(),
            Err(AutoThrottleConfigError::TargetConcurrency)
        );
    }

    #[test]
    fn equal_start_and_max_delay_is_allowed() {
        assert!(AutoThrottleConfig {
            start_delay: 5.0,
            max_delay: 5.0,
            ..AutoThrottleConfig::default()
        }
        .validate()
        .is_ok());
    }

    // -- delay_for --------------------------------------------------------

    #[test]
    fn seed_delay_rules() {
        assert_eq!(throttle(5.0, 60.0).delay_for("example.com", 0.0), 5.0);
        assert_eq!(throttle(1.0, 60.0).delay_for("example.com", 3.0), 3.0);
        assert_eq!(throttle(5.0, 20.0).delay_for("example.com", 100.0), 20.0);
    }

    #[test]
    fn delay_for_is_sticky() {
        let t = throttle(5.0, 60.0);
        assert_eq!(t.delay_for("example.com", 0.0), 5.0);
        // A later, higher floor does not re-seed an already known domain.
        assert_eq!(t.delay_for("example.com", 30.0), 5.0);
    }

    // -- record -----------------------------------------------------------

    #[test]
    fn healthy_requests_converge_on_latency() {
        let t = throttle(5.0, 60.0);
        let mut delay = 0.0;
        for _ in 0..20 {
            delay = t.record("example.com", 2.0, true, 0.0, None);
        }
        assert!((delay - 2.0).abs() < 0.01, "{delay}");
    }

    #[test]
    fn target_concurrency_divides_the_target() {
        let t = AutoThrottle::new(AutoThrottleConfig {
            start_delay: 5.0,
            target_concurrency: 4.0,
            ..AutoThrottleConfig::default()
        })
        .expect("valid config");
        let mut delay = 0.0;
        for _ in 0..20 {
            delay = t.record("example.com", 2.0, true, 0.0, None);
        }
        assert!((delay - 0.5).abs() < 0.01, "{delay}");
    }

    #[test]
    fn latency_spike_applies_immediately() {
        let t = throttle(1.0, 60.0);
        assert_eq!(t.record("example.com", 10.0, true, 0.0, None), 10.0);
    }

    #[test]
    fn block_never_speeds_the_spider_up() {
        let t = AutoThrottle::new(AutoThrottleConfig {
            start_delay: 5.0,
            block_backoff: false,
            ..AutoThrottleConfig::default()
        })
        .expect("valid config");
        assert_eq!(t.record("example.com", 0.1, false, 0.0, None), 5.0);

        let t = AutoThrottle::new(AutoThrottleConfig {
            start_delay: 1.0,
            block_backoff: false,
            ..AutoThrottleConfig::default()
        })
        .expect("valid config");
        assert_eq!(t.record("example.com", 8.0, false, 0.0, None), 8.0);
    }

    #[test]
    fn clamping() {
        let t = throttle(1.0, 3.0);
        assert_eq!(t.record("example.com", 100.0, true, 0.0, None), 3.0);
        let t = throttle(1.0, 3.0);
        assert_eq!(t.record("fresh.com", 0.001, true, 2.0, None), 2.0);
    }

    #[test]
    fn domains_are_independent() {
        let t = throttle(1.0, 60.0);
        t.record("a.com", 0.05, false, 0.0, None);
        assert_eq!(t.delay_for("a.com", 0.0), 2.0);
        assert_eq!(t.delay_for("b.com", 0.0), 1.0);
    }

    #[test]
    fn reset_reseeds() {
        let t = throttle(1.0, 60.0);
        t.record("a.com", 0.05, false, 0.0, None);
        assert_eq!(t.len(), 1);
        t.reset();
        assert!(t.is_empty());
        assert_eq!(t.delay_for("a.com", 0.0), 1.0);
    }

    #[test]
    fn backoff_doubles() {
        let t = throttle(1.0, 60.0);
        let seq: Vec<f64> = (0..5)
            .map(|_| t.record("example.com", 0.05, false, 0.0, None))
            .collect();
        assert_eq!(seq, vec![2.0, 4.0, 8.0, 16.0, 32.0]);
    }

    #[test]
    fn backoff_saturates_at_max_delay() {
        let t = throttle(1.0, 5.0);
        let mut delay = 0.0;
        for _ in 0..10 {
            delay = t.record("example.com", 0.05, false, 0.0, None);
        }
        assert_eq!(delay, 5.0);
    }

    #[test]
    fn recovery_walks_the_delay_back_down() {
        let t = throttle(1.0, 60.0);
        for _ in 0..4 {
            t.record("example.com", 0.05, false, 0.0, None);
        }
        assert_eq!(t.delay_for("example.com", 0.0), 16.0);
        let mut delay = 0.0;
        for _ in 0..20 {
            delay = t.record("example.com", 0.5, true, 0.0, None);
        }
        assert!((delay - 0.5).abs() < 0.01, "{delay}");
    }

    #[test]
    fn block_backoff_off_freezes_the_delay() {
        let t = AutoThrottle::new(AutoThrottleConfig {
            start_delay: 1.0,
            block_backoff: false,
            ..AutoThrottleConfig::default()
        })
        .expect("valid config");
        let seq: Vec<f64> = (0..3)
            .map(|_| t.record("example.com", 0.05, false, 0.0, None))
            .collect();
        assert_eq!(seq, vec![1.0, 1.0, 1.0]);
    }

    #[test]
    fn retry_after_is_honoured() {
        assert_eq!(
            throttle(1.0, 60.0).record("example.com", 0.05, false, 0.0, Some(30.0)),
            30.0
        );
        assert_eq!(
            throttle(1.0, 60.0).record("example.com", 0.05, false, 0.0, Some(3600.0)),
            60.0
        );
        // Never speeds the spider up.
        assert_eq!(
            throttle(10.0, 60.0).record("example.com", 0.05, false, 0.0, Some(0.0)),
            10.0
        );
    }

    #[test]
    fn retry_after_is_ignored_without_block_backoff() {
        let t = AutoThrottle::new(AutoThrottleConfig {
            start_delay: 1.0,
            block_backoff: false,
            ..AutoThrottleConfig::default()
        })
        .expect("valid config");
        assert_eq!(t.record("example.com", 0.05, false, 0.0, Some(30.0)), 1.0);
    }

    #[test]
    fn delays_snapshot_is_sorted() {
        let t = throttle(1.0, 60.0);
        t.delay_for("z.com", 0.0);
        t.delay_for("a.com", 0.0);
        let snapshot = t.delays();
        assert_eq!(snapshot.keys().collect::<Vec<_>>(), vec!["a.com", "z.com"]);
    }

    #[test]
    fn concurrent_blocks_on_one_domain_still_double() {
        // The regression this whole module is shaped around: 16 OS threads hammering one
        // domain must still produce a strictly doubling sequence, because each
        // read-modify-write is serialised by that key's entry guard. A plain
        // `RwLock<HashMap>` with a separate read and write would produce duplicates here.
        let throttle = throttle(1.0, 1_048_576.0);
        let observed = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(|| {
                    let delay = throttle.record("example.com", 0.0, false, 0.0, None);
                    observed.lock().expect("not poisoned").push(delay);
                });
            }
        });
        let mut observed = observed.into_inner().expect("not poisoned");
        observed.sort_by(f64::total_cmp);
        let expected: Vec<f64> = (1..=16).map(|n| 2f64.powi(n)).collect();
        assert_eq!(observed, expected);
    }

    #[test]
    fn concurrent_domains_do_not_interfere() {
        let throttle = throttle(1.0, 1024.0);
        std::thread::scope(|scope| {
            for domain in 0..16u32 {
                for _ in 0..4 {
                    let throttle = &throttle;
                    scope.spawn(move || {
                        throttle.record(&format!("d{domain}.example.com"), 0.0, false, 0.0, None);
                    });
                }
            }
        });
        let delays = throttle.delays();
        assert_eq!(delays.len(), 16);
        assert!(delays
            .values()
            .all(|delay| (*delay - 16.0).abs() < f64::EPSILON));
    }
}
