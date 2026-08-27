//! `uncurl`: the `shlex`-based curl-command parser that produces a runnable request.
//!
//! Python's `CurlParser` (`reference/core/shell.py`, `cli-ai.md` §5) lives in the
//! interactive shell, which this port does not have (`ARCHITECTURE.md` §8). Its
//! two useful halves are exposed as one subcommand instead:
//!
//! * `brs uncurl '<curl …>'` is `uncurl()` — parse and print;
//! * `brs uncurl -o out.md '<curl …>'` is `curl2fetcher()` — parse, run
//!   through the fetcher, and write the page.
//!
//! # Why the argument scanner is hand-rolled
//!
//! Python uses `argparse.parse_known_args`, which has three behaviours `clap`
//! deliberately does not have: unknown arguments are *collected* rather than
//! rejected, long options may be **abbreviated** as long as the abbreviation is
//! unambiguous, and positional arguments are matched greedily against a pattern
//! **per contiguous run**. `cli-ai.md` §11.4 item 6 calls this out as having no
//! `clap` analogue, so [`parse_tokens`] reimplements the parts of `argparse`
//! that this grammar exercises. The approximations are listed on that function.
//!
//! # The two faithful-to-a-bug behaviours
//!
//! * `curl_command.strip().lstrip("curl")` is a **character-set** strip, not a
//!   prefix strip — `"crul https://…"` and `"curllll https://…"` are both
//!   mangled the same way. [`strip_curl_prefix`] reproduces it, because a port
//!   that quietly fixed it would accept command lines Python rejects.
//! * `--data-raw`'s value gets `.lstrip("$")`, which is there for DevTools'
//!   `$'…'` ANSI-C quoting and eats every leading `$`.

use std::fmt;

use browser::{FollowRedirects, ProxySpec, RedirectMode};

/// An insertion-ordered string map — Python's `dict`.
///
/// Named through the façade rather than by depending on `indexmap` directly, so
/// this crate keeps exactly the dependency set its `Cargo.toml` declares.
pub type OrderedMap = browser::CookieMap;

/// The four methods `CurlParser._supported_methods` will hand to a fetcher.
pub const SUPPORTED_METHODS: [&str; 4] = ["get", "post", "put", "delete"];

/// `CurlParser`'s `Request` namedtuple (`cli-ai.md` §5.1).
///
/// The field order is Python's, because the spec calls index access part of the
/// contract (`request[0] == method`) and because it is the order the JSON that
/// `uncurl` prints comes out in.
#[derive(Debug, Clone, PartialEq)]
pub struct CurlRequest {
    /// Lowercased. Not validated here: an unsupported method is an error only
    /// when the request is actually run.
    pub method: String,
    /// The positional URL.
    pub url: String,
    /// Query parameters, populated only by the `-G` path.
    pub params: OrderedMap,
    /// The request body, when it did not turn out to be JSON.
    pub data: Option<CurlBody>,
    /// The body, when it parsed as a JSON object or array.
    pub json: Option<serde_json::Value>,
    /// `-H` headers, minus any `Cookie:` line.
    pub headers: OrderedMap,
    /// Cookies from `-H "Cookie: …"` merged with `-b`.
    pub cookies: OrderedMap,
    /// `{"http": …, "https": …}` — the same URL under both schemes.
    pub proxy: Option<ProxySpec>,
    /// Always [`RedirectMode::Safe`]; `parse()` hard-codes it.
    pub follow_redirects: FollowRedirects,
}

/// The three shapes a curl body can take.
#[derive(Debug, Clone, PartialEq)]
pub enum CurlBody {
    /// `-d` / `--data-raw` — a raw string.
    Text(String),
    /// `--data-binary` — UTF-8 bytes.
    Bytes(Vec<u8>),
    /// `--data-urlencode` — parsed into pairs.
    Form(OrderedMap),
}

impl CurlBody {
    /// A JSON rendering for `uncurl`'s output. Bytes are shown as the string
    /// they were encoded from, which is all `--data-binary` can hold here.
    fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Text(text) => serde_json::Value::String(text.clone()),
            Self::Bytes(bytes) => serde_json::Value::String(String::from_utf8_lossy(bytes).into()),
            Self::Form(pairs) => serde_json::Value::Object(
                pairs
                    .iter()
                    .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
                    .collect(),
            ),
        }
    }
}

impl CurlRequest {
    /// The method, if a fetcher can run it.
    #[must_use]
    pub fn supported_method(&self) -> Option<&str> {
        SUPPORTED_METHODS
            .into_iter()
            .find(|candidate| *candidate == self.method)
    }

    /// Whether this method carries a body. `convert2fetcher` pops `data` and
    /// `json` for everything that is not POST or PUT — **including DELETE**.
    #[must_use]
    pub fn method_takes_a_body(&self) -> bool {
        matches!(self.method.as_str(), "post" | "put")
    }

    /// The request as JSON, in the namedtuple's field order.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let map_of = |map: &OrderedMap| {
            serde_json::Value::Object(
                map.iter()
                    .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
                    .collect(),
            )
        };
        let mut out = serde_json::Map::new();
        out.insert("method".into(), self.method.clone().into());
        out.insert("url".into(), self.url.clone().into());
        out.insert("params".into(), map_of(&self.params));
        out.insert(
            "data".into(),
            self.data
                .as_ref()
                .map_or(serde_json::Value::Null, CurlBody::to_json),
        );
        out.insert(
            "json_data".into(),
            self.json.clone().unwrap_or(serde_json::Value::Null),
        );
        out.insert("headers".into(), map_of(&self.headers));
        out.insert("cookies".into(), map_of(&self.cookies));
        out.insert(
            "proxy".into(),
            self.proxy.as_ref().map_or(serde_json::Value::Null, |spec| {
                let mut proxy = serde_json::Map::new();
                if let Some(url) = &spec.http {
                    proxy.insert("http".into(), url.clone().into());
                }
                if let Some(url) = &spec.https {
                    proxy.insert("https".into(), url.clone().into());
                }
                serde_json::Value::Object(proxy)
            }),
        );
        out.insert("follow_redirects".into(), "safe".into());
        serde_json::Value::Object(out)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Everything that can go wrong turning a curl command into a request.
///
/// Python splits these across three outcomes — `AttributeError` (re-raised),
/// `ValueError` (swallowed, `parse` returns `None`) and a logged `None`. The
/// variants keep that distinction visible via [`CurlParseError::is_reraised`],
/// because it is the difference between "the user typed something the reference implementation
/// does not support" and "the shell gave up quietly".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurlParseError {
    /// `AttributeError(f"Unknown/Unsupported curl arguments: {unknown}")`.
    UnknownArguments(Vec<String>),
    /// `shlex.split` failed — an unterminated quote.
    Unsplittable(String),
    /// argparse's `the following arguments are required: url`.
    MissingUrl,
    /// argparse's `expected one argument`.
    MissingValue(&'static str),
    /// argparse's `ignored explicit argument` for a flag given a value.
    UnexpectedValue(&'static str),
    /// argparse's `ambiguous option`.
    AmbiguousOption {
        /// What the user typed, including the leading dashes.
        given: String,
        /// Every long option it could have meant.
        candidates: Vec<&'static str>,
    },
    /// `log.error(f'Request method "{m}" isn\'t supported by the reference implementation yet')`.
    UnsupportedMethod(String),
    /// `_ParseHeaders` raised. `parse()` does **not** guard this call, so the
    /// `ValueError` escapes to the caller exactly as an `AttributeError` would.
    /// Corpus row `header-no-colon`.
    Header(String),
}

impl CurlParseError {
    /// Whether Python lets this escape `parse()` rather than returning `None`.
    ///
    /// Two do: the `AttributeError` for unknown arguments, and the `ValueError`
    /// `_ParseHeaders` raises — step 5 of `cli-ai.md` §5.3 is outside every
    /// `try`. Everything else is logged and swallowed into a `None`.
    #[must_use]
    pub const fn is_reraised(&self) -> bool {
        matches!(self, Self::UnknownArguments(_) | Self::Header(_))
    }

    /// The Python exception class this corresponds to.
    ///
    /// Only meaningful for the two [`CurlParseError::is_reraised`] variants —
    /// the rest are logged and turned into a `None` on the Python side, so their
    /// class is never observable. The corpus compares
    /// `"{python_exception}: {error}"` against the recorded string, which is why
    /// the two have to be told apart at all.
    #[must_use]
    pub const fn python_exception(&self) -> &'static str {
        match self {
            Self::UnknownArguments(_) => "AttributeError",
            _ => "ValueError",
        }
    }
}

impl fmt::Display for CurlParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownArguments(args) => {
                write!(
                    f,
                    "Unknown/Unsupported curl arguments: {}",
                    python_list(args)
                )
            }
            Self::Unsplittable(command) => {
                write!(f, "Could not split command line: {command}")
            }
            Self::MissingUrl => f.write_str(
                "Curl arguments parsing error: the following arguments are required: url",
            ),
            Self::MissingValue(option) => write!(
                f,
                "Curl arguments parsing error: argument {option}: expected one argument"
            ),
            Self::UnexpectedValue(option) => write!(
                f,
                "Curl arguments parsing error: argument {option}: ignored explicit argument"
            ),
            Self::AmbiguousOption { given, candidates } => write!(
                f,
                "Curl arguments parsing error: ambiguous option: {given} could match {}",
                candidates.join(", ")
            ),
            Self::UnsupportedMethod(method) => {
                write!(f, "Request method \"{method}\" isn't supported yet")
            }
            Self::Header(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for CurlParseError {}

/// Render a `Vec<String>` the way Python renders a list of `str`, so the
/// `Unknown/Unsupported curl arguments:` message reads identically.
fn python_list(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|item| python_repr(item)).collect();
    format!("[{}]", inner.join(", "))
}

/// CPython's `repr()` for a `str`.
///
/// The rule that matters and that a naive `format!("'{}'", …)` gets wrong:
/// **the quote character is chosen**. A string containing `'` but no `"` is
/// rendered in double quotes with the apostrophe left bare, so a rejected
/// `{'a':1}` argument reads `"{'a':1}"` and not `'{\'a\':1}'`. The seed-1 fuzz
/// run found this three times in 2000 command lines.
///
/// Escapes follow `unicode_repr`: the quote itself and `\` are backslashed,
/// `\n` / `\r` / `\t` get their short forms, and anything Python calls
/// non-printable becomes `\xNN` / `\uXXXX`. [`is_python_printable`] is a
/// pragmatic subset of Python's `str.isprintable()` — see its note.
fn python_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(value.len() + 2);
    out.push(quote);
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ if ch == quote => {
                out.push('\\');
                out.push(ch);
            }
            _ if is_python_printable(ch) => out.push(ch),
            _ => {
                let code = ch as u32;
                if code <= 0xff {
                    out.push_str(&format!("\\x{code:02x}"));
                } else if code <= 0xffff {
                    out.push_str(&format!("\\u{code:04x}"));
                } else {
                    out.push_str(&format!("\\U{code:08x}"));
                }
            }
        }
    }
    out.push(quote);
    out
}

/// Python's `str.isprintable()`, for the categories that can realistically
/// reach a curl command line.
///
/// Python's real answer is "not in Cc, Cf, Cs, Co, Cn, Zl, Zp, Zs — except
/// U+0020". Reproducing that exactly needs the Unicode category tables, which
/// this crate does not depend on; what is implemented is the C0/C1 controls,
/// the separators and the format characters, which is every non-printable a
/// pasted command line has ever contained. An unassigned code point (Cn) is
/// treated as printable here and is not — the one known residue, and it can
/// only ever change the *text of an error message*.
fn is_python_printable(ch: char) -> bool {
    if ch == ' ' {
        return true;
    }
    !matches!(ch,
        '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}'          // Cc
        | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200a}' // Zs
        | '\u{202f}' | '\u{205f}' | '\u{3000}'
        | '\u{2028}' | '\u{2029}'                          // Zl, Zp
        | '\u{ad}' | '\u{600}'..='\u{605}' | '\u{61c}'     // Cf
        | '\u{6dd}' | '\u{70f}' | '\u{180e}'
        | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}'
        | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}'
        | '\u{feff}' | '\u{fff9}'..='\u{fffb}'
    )
}

// ---------------------------------------------------------------------------
// The argument grammar
// ---------------------------------------------------------------------------

/// Which value an option writes into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dest {
    Method,
    Header,
    UserAgent,
    Data,
    DataRaw,
    DataBinary,
    DataUrlencode,
    Get,
    Cookie,
    Proxy,
    ProxyUser,
    Insecure,
    Compressed,
    Include,
    Silent,
    Verbose,
}

/// How many values an option takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arity {
    /// `default=None`, overwritten each time.
    Store,
    /// `action="append"`.
    Append,
    /// `action="store_true"`.
    Flag,
}

/// One registered argument, in `CurlParser.__init__`'s registration order —
/// which is the order argparse reports ambiguity candidates in.
struct OptionSpec {
    long: &'static str,
    short: Option<char>,
    dest: Dest,
    arity: Arity,
    /// The `-x/--long` label argparse puts in its error messages.
    label: &'static str,
}

const OPTIONS: &[OptionSpec] = &[
    spec(
        "--request",
        Some('X'),
        Dest::Method,
        Arity::Store,
        "-X/--request",
    ),
    spec(
        "--header",
        Some('H'),
        Dest::Header,
        Arity::Append,
        "-H/--header",
    ),
    spec(
        "--user-agent",
        Some('A'),
        Dest::UserAgent,
        Arity::Store,
        "-A/--user-agent",
    ),
    spec("--data", Some('d'), Dest::Data, Arity::Store, "-d/--data"),
    spec(
        "--data-raw",
        None,
        Dest::DataRaw,
        Arity::Store,
        "--data-raw",
    ),
    spec(
        "--data-binary",
        None,
        Dest::DataBinary,
        Arity::Store,
        "--data-binary",
    ),
    spec(
        "--data-urlencode",
        None,
        Dest::DataUrlencode,
        Arity::Append,
        "--data-urlencode",
    ),
    spec("--get", Some('G'), Dest::Get, Arity::Flag, "-G/--get"),
    spec(
        "--cookie",
        Some('b'),
        Dest::Cookie,
        Arity::Store,
        "-b/--cookie",
    ),
    spec(
        "--proxy",
        Some('x'),
        Dest::Proxy,
        Arity::Store,
        "-x/--proxy",
    ),
    spec(
        "--proxy-user",
        Some('U'),
        Dest::ProxyUser,
        Arity::Store,
        "-U/--proxy-user",
    ),
    spec(
        "--insecure",
        Some('k'),
        Dest::Insecure,
        Arity::Flag,
        "-k/--insecure",
    ),
    spec(
        "--compressed",
        None,
        Dest::Compressed,
        Arity::Flag,
        "--compressed",
    ),
    spec(
        "--include",
        Some('i'),
        Dest::Include,
        Arity::Flag,
        "-i/--include",
    ),
    spec(
        "--silent",
        Some('s'),
        Dest::Silent,
        Arity::Flag,
        "-s/--silent",
    ),
    spec(
        "--verbose",
        Some('v'),
        Dest::Verbose,
        Arity::Flag,
        "-v/--verbose",
    ),
];

const fn spec(
    long: &'static str,
    short: Option<char>,
    dest: Dest,
    arity: Arity,
    label: &'static str,
) -> OptionSpec {
    OptionSpec {
        long,
        short,
        dest,
        arity,
        label,
    }
}

/// The namespace argparse fills in.
#[derive(Debug, Default)]
struct Namespace {
    placeholder: Option<String>,
    url: Option<String>,
    method: Option<String>,
    header: Vec<String>,
    #[allow(
        dead_code,
        reason = "parsed and stored exactly as Python does; never read"
    )]
    user_agent: Option<String>,
    data: Option<String>,
    data_raw: Option<String>,
    data_binary: Option<String>,
    data_urlencode: Vec<String>,
    get: bool,
    cookie: Option<String>,
    proxy: Option<String>,
    proxy_user: Option<String>,
    #[allow(
        dead_code,
        reason = "parsed and stored exactly as Python does; never read"
    )]
    insecure: bool,
    #[allow(
        dead_code,
        reason = "parsed and stored exactly as Python does; never read"
    )]
    compressed: bool,
    #[allow(
        dead_code,
        reason = "parsed and stored exactly as Python does; never read"
    )]
    include: bool,
    #[allow(
        dead_code,
        reason = "parsed and stored exactly as Python does; never read"
    )]
    silent: bool,
    #[allow(
        dead_code,
        reason = "parsed and stored exactly as Python does; never read"
    )]
    verbose: bool,
}

impl Namespace {
    fn store(&mut self, dest: Dest, value: String) {
        match dest {
            Dest::Method => self.method = Some(value),
            Dest::Header => self.header.push(value),
            Dest::UserAgent => self.user_agent = Some(value),
            Dest::Data => self.data = Some(value),
            Dest::DataRaw => self.data_raw = Some(value),
            Dest::DataBinary => self.data_binary = Some(value),
            Dest::DataUrlencode => self.data_urlencode.push(value),
            Dest::Cookie => self.cookie = Some(value),
            Dest::Proxy => self.proxy = Some(value),
            Dest::ProxyUser => self.proxy_user = Some(value),
            // Flags never reach here; `set_flag` handles them.
            Dest::Get
            | Dest::Insecure
            | Dest::Compressed
            | Dest::Include
            | Dest::Silent
            | Dest::Verbose => {}
        }
    }

    fn set_flag(&mut self, dest: Dest) {
        match dest {
            Dest::Get => self.get = true,
            Dest::Insecure => self.insecure = true,
            Dest::Compressed => self.compressed = true,
            Dest::Include => self.include = true,
            Dest::Silent => self.silent = true,
            Dest::Verbose => self.verbose = true,
            _ => {}
        }
    }
}

/// `str.lstrip("curl")` — a **character-set** strip, faithfully.
///
/// `cli-ai.md` §11.4 item 5: this is a Python bug the port keeps, because
/// fixing it would make the Rust CLI accept command lines the Python one
/// rejects, and the whole point of the port is that they agree.
///
/// ```
/// use browser_cli::commands::uncurl::strip_curl_prefix;
///
/// assert_eq!(strip_curl_prefix("curl https://x.test/"), " https://x.test/");
/// // …and the bug:
/// assert_eq!(strip_curl_prefix("crul https://x.test/"), " https://x.test/");
/// assert_eq!(strip_curl_prefix("curllll https://x.test/"), " https://x.test/");
/// ```
#[must_use]
pub fn strip_curl_prefix(command: &str) -> &str {
    command.trim_start_matches(['c', 'u', 'r', 'l'])
}

/// `parse(curl_command)` — the whole pipeline from a pasted command line.
///
/// # Errors
///
/// See [`CurlParseError`].
pub fn parse_curl(command: &str) -> Result<CurlRequest, CurlParseError> {
    // `curl_command.strip().lstrip("curl").strip().replace("\\\n", " ")`.
    let cleaned = strip_curl_prefix(command.trim())
        .trim()
        .replace("\\\n", " ");
    let tokens =
        shlex_split(&cleaned).ok_or_else(|| CurlParseError::Unsplittable(cleaned.clone()))?;
    parse_tokens(&tokens)
}

/// `shlex.split(s)` — CPython's, not the `shlex` crate's.
///
/// The two are close but not the same, and every difference is observable
/// through `uncurl`. The `shlex` crate 2.0.1:
///
/// * treats `#` as starting a comment and drops the rest of the line, where
///   `shlex.split` sets `commenters = ''` and `#` is an ordinary character;
/// * does not count `\r` as whitespace, where CPython's `whitespace` is
///   `" \t\r\n"`;
/// * unescapes `\$` and ``\` `` inside double quotes, where CPython leaves the
///   backslash in place (only `\\` and `\"` are special inside `"`);
/// * swallows a `\` immediately before a newline, where CPython keeps the
///   newline in the token.
///
/// The `fuzz_shlex_json.py` run found the first of those 274 times in 12000
/// command lines, so this is a transcription of `shlex.read_token` for
/// `posix=True, whitespace_split=True, commenters='', punctuation_chars=''` —
/// the exact configuration `shlex.split` builds.
///
/// Returns `None` for CPython's two `ValueError`s (`No closing quotation` and
/// `No escaped character`), which `CurlParser.parse` logs and turns into a
/// bare `None`.
#[must_use]
pub fn shlex_split(input: &str) -> Option<Vec<String>> {
    /// CPython's `self.state`, with the escape state carrying the state it
    /// will return to (`escapedstate`).
    enum State {
        Space,
        Quote(char),
        /// `escapedstate`: `None` for `'a'`, `Some(q)` for a quote.
        Escape(Option<char>),
        Word,
    }

    const WHITESPACE: [char; 4] = [' ', '\t', '\r', '\n'];

    let mut tokens = Vec::new();
    let mut chars = input.chars();
    let mut state = State::Space;
    let mut token = String::new();
    let mut quoted = false;

    loop {
        let next = chars.next();
        match state {
            State::Space => match next {
                // `self.state = None; break` — the token so far is emitted and
                // the lexer stops.
                None => break,
                Some(ch) if WHITESPACE.contains(&ch) => {
                    if !token.is_empty() || quoted {
                        tokens.push(std::mem::take(&mut token));
                        quoted = false;
                    }
                }
                Some('\\') => state = State::Escape(None),
                Some(ch @ ('\'' | '"')) => state = State::Quote(ch),
                // `elif self.whitespace_split:` — with `whitespace_split` on,
                // every remaining character starts a word, `#` included.
                Some(ch) => {
                    token.push(ch);
                    state = State::Word;
                }
            },
            State::Quote(quote) => {
                quoted = true;
                match next {
                    // `raise ValueError("No closing quotation")`.
                    None => return None,
                    Some(ch) if ch == quote => state = State::Word,
                    // Only `"` is in `escapedquotes`, so a backslash inside
                    // `'…'` is a literal backslash.
                    Some('\\') if quote == '"' => state = State::Escape(Some(quote)),
                    Some(ch) => token.push(ch),
                }
            }
            State::Escape(escaped_state) => {
                // `raise ValueError("No escaped character")` at end of input.
                let ch = next?;
                // "In posix shells, only the quote itself or the escape
                // character may be escaped within quotes" — anything else keeps
                // its backslash.
                if let Some(quote) = escaped_state {
                    if ch != '\\' && ch != quote {
                        token.push('\\');
                    }
                }
                token.push(ch);
                state = escaped_state.map_or(State::Word, State::Quote);
            }
            State::Word => match next {
                None => break,
                Some(ch) if WHITESPACE.contains(&ch) => {
                    state = State::Space;
                    if !token.is_empty() || quoted {
                        tokens.push(std::mem::take(&mut token));
                        quoted = false;
                    }
                }
                Some(ch @ ('\'' | '"')) => state = State::Quote(ch),
                Some('\\') => state = State::Escape(None),
                Some(ch) => token.push(ch),
            },
        }
    }

    // `if self.posix and not quoted and result == '': result = None` — an empty
    // unquoted tail is the end-of-file marker, not a token.
    if !token.is_empty() || quoted {
        tokens.push(token);
    }
    Some(tokens)
}

/// The same, from tokens a shell has already split.
///
/// A leading `curl` token is dropped, which is the token form of
/// [`strip_curl_prefix`]; anything else is left for the positional matcher, so
/// `brs uncurl not a curl command` still produces Python's
/// `Unknown/Unsupported curl arguments: ['curl', 'command']`.
///
/// # Errors
///
/// See [`CurlParseError`].
pub fn parse_curl_tokens(tokens: &[String]) -> Result<CurlRequest, CurlParseError> {
    match tokens.split_first() {
        Some((first, rest)) if first == "curl" => parse_tokens(rest),
        _ => parse_tokens(tokens),
    }
}

/// `parser.parse_known_args(tokens)`, then steps 4–10 of `cli-ai.md` §5.3.
///
/// # Where this is an approximation of `argparse`
///
/// * Long options accept unambiguous **abbreviations** (`--head` → `--header`),
///   like argparse; an ambiguous one is an error naming every candidate.
/// * An unknown option becomes an *extra*, like `parse_known_args`; an unknown
///   short option takes the rest of its cluster with it, where argparse's
///   recovery is finer-grained.
/// * A value that itself looks like an option (`-d -X`) is refused rather than
///   consumed, matching argparse's `expected one argument`.
/// * Positional arguments are matched per contiguous run against
///   `[placeholder?, url]` by `match_positionals`, which is
///   `_match_arguments_partial` over `(-*A?-*)(-*A-*)` written out longhand —
///   including the `-` that a literal `--` contributes to the pattern.
///
/// # Errors
///
/// See [`CurlParseError`].
pub fn parse_tokens(tokens: &[String]) -> Result<CurlRequest, CurlParseError> {
    let (namespace, unknown) = parse_known(tokens)?;
    // `the following arguments are required: url` is raised *inside*
    // `parse_known_args`, so it beats the `if unknown:` check `parse()` makes
    // afterwards — and it is a `ValueError`, so `parse()` swallows it and
    // returns `None` where the unknown-argument `AttributeError` escapes. A
    // command with both a missing URL and an unsupported flag is therefore a
    // silent `None`, not a raise. Found by the seed-1 fuzz run (`curl --retry`).
    let url = namespace.url.clone().ok_or(CurlParseError::MissingUrl)?;
    if !unknown.is_empty() {
        return Err(CurlParseError::UnknownArguments(unknown));
    }
    build_request(namespace, url)
}

/// The scanner. Returns the filled namespace and the arguments argparse would
/// have handed back as "unknown".
fn parse_known(tokens: &[String]) -> Result<(Namespace, Vec<String>), CurlParseError> {
    let mut namespace = Namespace::default();
    let mut unknown: Vec<String> = Vec::new();
    // One contiguous run of positionals, each tagged with the pattern character
    // argparse would have written for it: `'-'` for the **first** literal `--`
    // in the whole command line, `'A'` for everything else.
    let mut chunk: Vec<(String, char)> = Vec::new();
    // The positional actions still waiting, in declaration order.
    let mut placeholder_pending = true;
    let mut url_pending = true;
    let mut only_positionals_left = false;

    let mut index = 0usize;
    while index < tokens.len() {
        let token = &tokens[index];

        if only_positionals_left {
            // Past the first `--`, argparse writes `'A'` for every remaining
            // token — a *second* `--` is an ordinary positional string.
            chunk.push((token.clone(), 'A'));
            index += 1;
            continue;
        }
        if token == "--" {
            // `_parse_known_args` writes a `'-'` for it and keeps it in
            // `arg_strings`; it is dropped later, per action, and only when
            // that action's slice actually covers the `'-'`. So it is a
            // positional-run member here, not a token that disappears.
            only_positionals_left = true;
            chunk.push((token.clone(), '-'));
            index += 1;
            continue;
        }
        if !looks_like_option(token) {
            chunk.push((token.clone(), 'A'));
            index += 1;
            continue;
        }

        // An option closes the current run of positionals.
        consume_positionals(
            &mut chunk,
            &mut placeholder_pending,
            &mut url_pending,
            &mut namespace,
            &mut unknown,
        );

        index = if token.starts_with("--") {
            handle_long(tokens, index, &mut namespace, &mut unknown)?
        } else {
            handle_short(tokens, index, &mut namespace, &mut unknown)?
        };
    }

    consume_positionals(
        &mut chunk,
        &mut placeholder_pending,
        &mut url_pending,
        &mut namespace,
        &mut unknown,
    );
    Ok((namespace, unknown))
}

/// argparse's `_parse_optional`, in order — this decides whether a token is an
/// `'O'` or an `'A'` in the pattern, and therefore whether the option before it
/// can eat it as a value.
///
/// The steps, straight off CPython 3.14's source:
///
/// 1. an empty string is a positional;
/// 2. a token not starting with a prefix character is a positional;
/// 3. an exactly registered option string is an option;
/// 4. a one-character token (`-`) is a positional;
/// 5. `--long=value` whose `--long` is registered is an option;
/// 6. `_get_option_tuples` — long-option abbreviation, or a registered short
///    option with its value attached (`-dfoo`) — is an option;
/// 7. **`_negative_number_matcher.match`**, which in 3.11+ is the *prefix*
///    pattern `-\.?\d`, so `-0NaNfalse` and `-1` are positionals but
///    `-Infinity` is not;
/// 8. a token containing a space is a positional;
/// 9. anything left is an unrecognised option and goes to the extras.
///
/// The first draft had steps 7 and 8 as `token[1..].parse::<f64>()`, which is
/// neither: it accepted `-inf` and `-1e3` and rejected `-0abc`. The
/// `fuzz_shlex_json.py` run turned that into 274 disagreeing rows.
fn looks_like_option(token: &str) -> bool {
    if token.is_empty() || !token.starts_with('-') {
        return false;
    }
    if is_registered(token) {
        return true;
    }
    if token.chars().count() == 1 {
        return false;
    }
    if let Some((head, _)) = token.split_once('=') {
        if is_registered(head) {
            return true;
        }
    }
    if option_tuple_count(token) > 0 {
        return true;
    }
    if is_negative_number(token) {
        return false;
    }
    !token.contains(' ')
}

/// Whether an option string is registered verbatim.
fn is_registered(token: &str) -> bool {
    OPTIONS.iter().any(|option| {
        option.long == token
            || option.short.is_some_and(|letter| {
                let mut chars = token.chars();
                chars.next() == Some('-') && chars.next() == Some(letter) && chars.next().is_none()
            })
    })
}

/// `_get_option_tuples`, counted rather than collected.
///
/// Two dashes: only the abbreviation search, against the part before `=`.
/// One dash: the registered two-character prefix (`-dfoo` → `-d`) *or* the same
/// abbreviation search.
fn option_tuple_count(token: &str) -> usize {
    let (prefix, _) = token.split_once('=').unwrap_or((token, ""));
    if token.starts_with("--") {
        return OPTIONS
            .iter()
            .filter(|option| option.long.starts_with(prefix))
            .count();
    }
    let short_prefix: String = token.chars().take(2).collect();
    OPTIONS
        .iter()
        .filter(|option| {
            option
                .short
                .is_some_and(|letter| format!("-{letter}") == short_prefix)
                || option.long.starts_with(prefix)
        })
        .count()
}

/// `_negative_number_matcher`, which is `re.compile(r'-\.?\d')` used with
/// `.match()` — i.e. an *unanchored-at-the-end* prefix test, not a full parse.
///
/// The parser registers no option that looks like a negative number, so
/// `_has_negative_number_optionals` is empty and a match means "positional".
fn is_negative_number(token: &str) -> bool {
    let rest = match token.strip_prefix('-') {
        Some(rest) => rest,
        None => return false,
    };
    let rest = rest.strip_prefix('.').unwrap_or(rest);
    rest.starts_with(|ch: char| ch.is_ascii_digit())
}

/// Match one contiguous run of positionals against `[placeholder?, url]`.
///
/// This is argparse's `consume_positionals` for this grammar, including the
/// `--` rule that the first draft got wrong: a literal `--` stays in
/// `arg_strings` and contributes a `'-'` to the pattern, and is dropped again
/// only from the slice of the action whose range actually covers it
/// (`_get_values`' `arg_strings.remove('--')`, guarded by
/// `arg_strings_pattern.find('-', start, start + count)`). A `--` that no
/// action's slice reaches comes back out as an *extra*, which is why
/// `curl https://x -- ` raises `Unknown/Unsupported curl arguments: ['--']`.
///
/// argparse also re-enters `consume_positionals` while it keeps making
/// progress, so a run that only satisfies the optional action on the first pass
/// gets a second one for `url`; the loop here is that.
fn consume_positionals(
    chunk: &mut Vec<(String, char)>,
    placeholder_pending: &mut bool,
    url_pending: &mut bool,
    namespace: &mut Namespace,
    unknown: &mut Vec<String>,
) {
    if chunk.is_empty() {
        return;
    }
    let run = std::mem::take(chunk);
    let pattern: Vec<u8> = run.iter().map(|(_, mark)| *mark as u8).collect();

    let mut at = 0usize;
    loop {
        let counts = match_positionals(&pattern[at..], *placeholder_pending, *url_pending);
        if counts.is_empty() || counts.iter().all(|count| *count == 0) {
            break;
        }
        let mut counts = counts.into_iter();
        if *placeholder_pending {
            if let Some(count) = counts.next() {
                namespace.placeholder = slice_values(&run, &pattern, at, count).into_iter().next();
                at += count;
                *placeholder_pending = false;
            }
        }
        if *url_pending {
            if let Some(count) = counts.next() {
                let values = slice_values(&run, &pattern, at, count);
                // `url` has `nargs=None`, whose pattern is `(-*A-*)`: exactly one
                // `'A'` and, at most, the one `'-'` the whole command line can
                // contain. So after `_get_values` has dropped the `--`, exactly
                // one string is left. Asserting it keeps the drop *load-bearing*
                // — without this, a mutant that never removes the `--` still
                // passes every corpus row, because `.next()` happens to take the
                // right element for every shape this grammar can produce.
                debug_assert_eq!(
                    values.len(),
                    1,
                    "url's slice should hold exactly one value after the `--` is dropped, got {values:?}"
                );
                namespace.url = values.into_iter().next();
                at += count;
                *url_pending = false;
            }
        }
        if !*placeholder_pending && !*url_pending {
            break;
        }
    }
    unknown.extend(run[at..].iter().map(|(value, _)| value.clone()));
}

/// One action's slice of a positional run, with the first `--` removed when the
/// slice's pattern range covers the `'-'`.
fn slice_values(run: &[(String, char)], pattern: &[u8], at: usize, count: usize) -> Vec<String> {
    let mut values: Vec<String> = run[at..at + count]
        .iter()
        .map(|(value, _)| value.clone())
        .collect();
    if pattern[at..at + count].contains(&b'-') {
        if let Some(position) = values.iter().position(|value| value == "--") {
            values.remove(position);
        }
    }
    values
}

/// `_match_arguments_partial` over the still-pending positional actions.
///
/// Returns one count per action it managed to match, longest action list first,
/// exactly as argparse's `for i in range(len(actions), 0, -1)` loop does. An
/// empty result means nothing was consumed and the actions stay pending.
fn match_positionals(pattern: &[u8], placeholder_pending: bool, url_pending: bool) -> Vec<usize> {
    match (placeholder_pending, url_pending) {
        (true, true) => match_optional_then_one(pattern).map_or_else(
            || vec![match_optional(pattern)],
            |(first, second)| vec![first, second],
        ),
        (true, false) => vec![match_optional(pattern)],
        (false, true) => match_one(pattern, 0).map_or_else(Vec::new, |count| vec![count]),
        (false, false) => Vec::new(),
    }
}

/// `re.match(r"(-*A?-*)(-*A-*)", pattern)`, greedy with backtracking in
/// CPython's preference order (leading `-*` longest first, then `A?` present,
/// then the middle `-*` longest first).
fn match_optional_then_one(pattern: &[u8]) -> Option<(usize, usize)> {
    let lead_max = dashes_from(pattern, 0);
    for lead in (0..=lead_max).rev() {
        for optional in [1usize, 0] {
            let mut cursor = lead;
            if optional == 1 {
                if pattern.get(cursor) == Some(&b'A') {
                    cursor += 1;
                } else {
                    continue;
                }
            }
            let trail_max = dashes_from(pattern, cursor);
            for trail in (0..=trail_max).rev() {
                if let Some(second) = match_one(pattern, cursor + trail) {
                    return Some((lead + optional + trail, second));
                }
            }
        }
    }
    None
}

/// `(-*A?-*)` on its own — the `i == 1` fallback. Always matches, possibly
/// with a length of zero.
fn match_optional(pattern: &[u8]) -> usize {
    let lead = dashes_from(pattern, 0);
    let mut cursor = lead;
    let optional = usize::from(pattern.get(cursor) == Some(&b'A'));
    cursor += optional;
    lead + optional + dashes_from(pattern, cursor)
}

/// `(-*A-*)` anchored at `start`. It is always the last group, so both `-*`
/// runs are taken whole.
///
/// The `lead` loop is a faithful transcription of the regex engine's
/// backtracking, but for this alphabet it can never fire past its first
/// iteration: the `'-'`s are contiguous, so any shorter `lead` lands the cursor
/// on another `'-'`, which is not `'A'`. Mutation testing flagged the loop as
/// dead for exactly that reason. It is kept because it is what `re` does, and
/// because a future grammar with a second `--` would need it.
fn match_one(pattern: &[u8], start: usize) -> Option<usize> {
    let lead_max = dashes_from(pattern, start);
    for lead in (0..=lead_max).rev() {
        let cursor = start + lead;
        if pattern.get(cursor) == Some(&b'A') {
            return Some(lead + 1 + dashes_from(pattern, cursor + 1));
        }
    }
    None
}

/// How many `'-'` pattern characters run from `start`.
fn dashes_from(pattern: &[u8], start: usize) -> usize {
    pattern[start.min(pattern.len())..]
        .iter()
        .take_while(|mark| **mark == b'-')
        .count()
}

/// Handle one `--long[=value]` token; returns the next index to read.
fn handle_long(
    tokens: &[String],
    index: usize,
    namespace: &mut Namespace,
    unknown: &mut Vec<String>,
) -> Result<usize, CurlParseError> {
    let token = &tokens[index];
    let body = &token[2..];
    let (name, inline) = match body.split_once('=') {
        Some((name, value)) => (name, Some(value.to_owned())),
        None => (body, None),
    };

    let Some(option) = resolve_long(name, token)? else {
        unknown.push(token.clone());
        return Ok(index + 1);
    };
    apply(option, inline, tokens, index, namespace)
}

/// Handle one `-abc` cluster; returns the next index to read.
fn handle_short(
    tokens: &[String],
    index: usize,
    namespace: &mut Namespace,
    unknown: &mut Vec<String>,
) -> Result<usize, CurlParseError> {
    let token = &tokens[index];
    // argparse splits at the first `=` for short options too, so `-X=POST`
    // carries the explicit value `POST`.
    let (cluster, inline) = match token.split_once('=') {
        Some((head, value)) if head.len() == 2 => (head, Some(value.to_owned())),
        _ => (token.as_str(), None),
    };

    let chars: Vec<char> = cluster.chars().skip(1).collect();
    let mut position = 0usize;
    while position < chars.len() {
        let letter = chars[position];
        let Some(option) = OPTIONS.iter().find(|option| option.short == Some(letter)) else {
            // Unknown short: argparse gives back the unmatched tail.
            let tail: String = chars[position..].iter().collect();
            unknown.push(format!("-{tail}"));
            return Ok(index + 1);
        };
        if option.arity == Arity::Flag {
            if position + 1 == chars.len() {
                if inline.is_some() {
                    return Err(CurlParseError::UnexpectedValue(option.label));
                }
                namespace.set_flag(option.dest);
                return Ok(index + 1);
            }
            namespace.set_flag(option.dest);
            position += 1;
            continue;
        }
        // A value-taking option swallows the rest of the cluster, or the inline
        // `=value`, or the next token.
        let attached: String = chars[position + 1..].iter().collect();
        let value = inline.or(if attached.is_empty() {
            None
        } else {
            Some(attached)
        });
        return apply(option, value, tokens, index, namespace);
    }
    Ok(index + 1)
}

/// Store one option's value, taking the following token when it has no inline
/// one; returns the next index to read.
fn apply(
    option: &OptionSpec,
    inline: Option<String>,
    tokens: &[String],
    index: usize,
    namespace: &mut Namespace,
) -> Result<usize, CurlParseError> {
    if option.arity == Arity::Flag {
        if inline.is_some() {
            return Err(CurlParseError::UnexpectedValue(option.label));
        }
        namespace.set_flag(option.dest);
        return Ok(index + 1);
    }
    match inline {
        Some(value) => {
            namespace.store(option.dest, value);
            Ok(index + 1)
        }
        None => {
            let next = tokens
                .get(index + 1)
                .filter(|candidate| !looks_like_option(candidate))
                .ok_or(CurlParseError::MissingValue(option.label))?;
            namespace.store(option.dest, next.clone());
            Ok(index + 2)
        }
    }
}

/// argparse's long-option lookup: exact match first, then unambiguous prefix.
fn resolve_long(name: &str, token: &str) -> Result<Option<&'static OptionSpec>, CurlParseError> {
    let with_dashes = format!("--{name}");
    if let Some(exact) = OPTIONS.iter().find(|option| option.long == with_dashes) {
        return Ok(Some(exact));
    }
    let candidates: Vec<&'static OptionSpec> = OPTIONS
        .iter()
        .filter(|option| option.long.starts_with(&with_dashes))
        .collect();
    match candidates.len() {
        0 => Ok(None),
        1 => Ok(Some(candidates[0])),
        _ => Err(CurlParseError::AmbiguousOption {
            given: token.to_owned(),
            candidates: candidates.into_iter().map(|option| option.long).collect(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Steps 4–10: namespace -> request
// ---------------------------------------------------------------------------

fn build_request(namespace: Namespace, url: String) -> Result<CurlRequest, CurlParseError> {
    // -- 4: method --------------------------------------------------------
    // `any([data, data_raw, data_binary, data_urlencode])` is a **truthiness**
    // test, not a "was it given" test — so `--data ''` leaves the method at
    // `get` even though step 6 below still sets the body to `''` (that one is
    // `is not None`). The two tests really are different in Python; keeping
    // them different here is what makes `curl x --data ''` a GET with an empty
    // body on both sides.
    let has_data = namespace.data.as_ref().is_some_and(|v| !v.is_empty())
        || namespace.data_raw.as_ref().is_some_and(|v| !v.is_empty())
        || namespace
            .data_binary
            .as_ref()
            .is_some_and(|v| !v.is_empty())
        || !namespace.data_urlencode.is_empty();
    let method = if namespace.get {
        "get".to_owned()
    // `elif parsed_args.method:` — an empty `-X ''` is falsy and falls through
    // to the data-inference branch. Only a non-empty value is honoured, and it
    // is `.strip().lower()`-ed, so `-X '   '` really does produce `""`.
    } else if let Some(explicit) = namespace.method.as_ref().filter(|v| !v.is_empty()) {
        explicit.trim().to_lowercase()
    } else if has_data {
        "post".to_owned()
    } else {
        "get".to_owned()
    };

    // -- 5: headers and cookies -------------------------------------------
    // `_ParseHeaders` raises on a header without a colon, and `parse()` does not
    // guard the call, so the `ValueError` escapes. Corpus row `header-no-colon`
    // is the one that caught this being swallowed. The colon-less form ending
    // in `;` is accepted by the parser itself and never reaches here.
    let (headers, mut cookies) = browser::parse_header_lines(&namespace.header, true)
        .map_err(|error| CurlParseError::Header(error.to_string()))?;
    // `if parsed_args.cookie:` — `-b ''` is falsy and is skipped outright.
    if let Some(raw) = namespace.cookie.as_ref().filter(|value| !value.is_empty()) {
        match browser::parse_cookie_string(raw) {
            // `-b` **merges over** `-H "Cookie: …"`, it does not replace.
            Ok(pairs) => {
                for (key, value) in pairs {
                    cookies.insert(key, value);
                }
                tracing::debug!(
                    "Parsed cookies from -b argument: {:?}",
                    cookies.keys().collect::<Vec<_>>()
                );
            }
            Err(error) => {
                tracing::error!("Could not parse cookie string from -b '{raw}': {error}");
            }
        }
    }
    // NB: no `headers.remove("")`. `_ParseHeaders` really does keep an empty
    // key — `-H ';'` becomes `{"": ""}` and `-H ':novalue'` becomes
    // `{"": "novalue"}` — and dropping it here was a divergence the seed-1 fuzz
    // run caught 16 times in 2000 command lines.

    // -- 6: body, in curl's precedence order -------------------------------
    let mut data: Option<CurlBody> = if let Some(binary) = namespace.data_binary {
        Some(CurlBody::Bytes(binary.into_bytes()))
    } else if let Some(raw) = namespace.data_raw {
        // `.lstrip("$")` — a character-set strip, again.
        Some(CurlBody::Text(raw.trim_start_matches('$').to_owned()))
    } else if let Some(plain) = namespace.data {
        Some(CurlBody::Text(plain))
    } else if namespace.data_urlencode.is_empty() {
        None
    } else {
        Some(CurlBody::Form(parse_qsl(
            &namespace.data_urlencode.join("&"),
        )))
    };

    // -- 7: JSON promotion --------------------------------------------------
    let mut json: Option<serde_json::Value> = None;
    if let Some(CurlBody::Text(text)) = &data {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(text) {
            if value.is_object() || value.is_array() {
                json = Some(value);
                data = None;
            }
        }
    }

    // -- 8: `-G` moves the body into the query string ----------------------
    let mut params = OrderedMap::new();
    if method == "get" {
        match &data {
            Some(CurlBody::Form(pairs)) => {
                for (key, value) in pairs {
                    params.insert(key.clone(), value.clone());
                }
            }
            Some(CurlBody::Text(text)) => {
                for (key, value) in parse_qsl(text) {
                    params.insert(key, value);
                }
            }
            Some(CurlBody::Bytes(_)) | None => {}
        }
        if !params.is_empty() {
            data = None;
            json = None;
        }
    }

    // -- 9: proxy -----------------------------------------------------------
    // `if parsed_args.proxy:` and `if parsed_args.proxy_user:` are both
    // truthiness tests: `-x ''` produces **no** proxy at all (not
    // `http://`), and `-U ''` leaves the URL's userinfo alone.
    let proxy = namespace
        .proxy
        .as_ref()
        .filter(|raw| !raw.is_empty())
        .map(|raw| {
            let url = if raw.contains("://") {
                raw.clone()
            } else {
                format!("http://{raw}")
            };
            let url = namespace
                .proxy_user
                .as_ref()
                .filter(|credentials| !credentials.is_empty())
                .map_or(url.clone(), |credentials| {
                    splice_userinfo(&url, credentials)
                });
            ProxySpec {
                http: Some(url.clone()),
                https: Some(url),
                ..ProxySpec::default()
            }
        });

    Ok(CurlRequest {
        method,
        url,
        params,
        data,
        json,
        headers,
        cookies,
        proxy,
        follow_redirects: FollowRedirects::Mode(RedirectMode::Safe),
    })
}

/// `urlparse` / `urlunparse` with the userinfo replaced, done on the string.
///
/// The `url` crate would percent-encode the credentials; Python splices them in
/// verbatim (`f"{user_pass}@{netloc_parts[-1]}"`), and a proxy password with a
/// `@` or a `:` in it therefore round-trips differently. Faithfulness wins.
fn splice_userinfo(url: &str, credentials: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let (netloc, tail) = match rest.find(['/', '?', '#']) {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, ""),
    };
    // `netloc.split("@")[-1]` — everything after the last `@`.
    let host = netloc.rsplit('@').next().unwrap_or(netloc);
    format!("{scheme}://{credentials}@{host}{tail}")
}

/// `urllib.parse.parse_qsl(value, keep_blank_values=True)`, collected into a
/// `dict` — so a repeated key keeps its first position and its last value.
fn parse_qsl(query: &str) -> OrderedMap {
    let mut out = OrderedMap::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key.is_empty() && value.is_empty() {
            continue;
        }
        out.insert(key.into_owned(), value.into_owned());
    }
    out
}

// ---------------------------------------------------------------------------
// Running it
// ---------------------------------------------------------------------------

/// `brs uncurl`.
///
/// Without `--output` this is `uncurl()`: parse and print the request as JSON.
/// With one it is `curl2fetcher()`: run the request and write the page.
///
/// # Errors
///
/// A parse failure, an unsupported method, a transport failure or a write
/// failure.
pub async fn run(options: crate::args::UncurlOptions) -> anyhow::Result<()> {
    let request = if options.command.len() == 1 {
        parse_curl(&options.command[0])?
    } else {
        parse_curl_tokens(&options.command)?
    };

    let Some(output_file) = options.output_file.as_deref() else {
        println!("{}", serde_json::to_string_pretty(&request.to_json())?);
        return Ok(());
    };

    let output_path = crate::convert::resolve_output_path(output_file)?;
    // Refuse a bad suffix before spending a request on it. Python discovers this
    // only after the fetch; erroring early is strictly friendlier and cannot
    // change what a successful run produces.
    let _ = crate::convert::extraction_for(&output_path.to_string_lossy())?;

    let response = send(&request).await?;
    tracing::info!("<{} {}>", response.status, response.url);
    crate::convert::write_content_to_file(
        &response,
        &output_path,
        options.css_selector.as_deref(),
        options.ai_targeted,
    )?;
    tracing::info!("Content successfully saved to '{}'", output_path.display());
    Ok(())
}

/// `convert2fetcher`: hand the parsed request to `AsyncFetcher`.
///
/// # Errors
///
/// [`CurlParseError::UnsupportedMethod`] for anything but GET/POST/PUT/DELETE,
/// and whatever the fetcher reports.
pub async fn send(request: &CurlRequest) -> anyhow::Result<browser::Response> {
    let method = request
        .supported_method()
        .ok_or_else(|| CurlParseError::UnsupportedMethod(request.method.clone()))?;

    let url = request.url.clone();
    let mut builder = match method {
        "get" => browser::AsyncFetcher::get(url),
        "post" => browser::AsyncFetcher::post(url),
        "put" => browser::AsyncFetcher::put(url),
        "delete" => browser::AsyncFetcher::delete(url),
        other => unreachable!("`supported_method` returned {other}"),
    };

    for (name, value) in &request.headers {
        builder = builder.header(name.clone(), value.clone());
    }
    if !request.cookies.is_empty() {
        builder = builder.cookies(request.cookies.clone());
    }
    if !request.params.is_empty() {
        builder = builder.params(
            request
                .params
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        );
    }
    if let Some(proxy) = &request.proxy {
        builder = builder.proxies(proxy.clone());
    }
    builder = builder.follow_redirects(request.follow_redirects);

    // `convert2fetcher` pops `data` and `json` for every method that is not
    // POST or PUT — DELETE included.
    if request.method_takes_a_body() {
        if let Some(json) = &request.json {
            builder = builder.json(json.clone());
        } else if let Some(body) = &request.data {
            builder = builder.body(match body {
                CurlBody::Text(text) => browser::Body::Text(text.clone()),
                CurlBody::Bytes(bytes) => browser::Body::Raw(bytes.clone().into()),
                CurlBody::Form(pairs) => browser::Body::Form(
                    pairs
                        .iter()
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect(),
                ),
            });
        }
    }

    Ok(builder.send().await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(command: &str) -> CurlRequest {
        parse_curl(command).unwrap_or_else(|error| panic!("{command:?}: {error}"))
    }

    // -- the `lstrip("curl")` quirk ---------------------------------------

    #[test]
    fn the_leading_curl_is_stripped_as_a_character_set_not_as_a_prefix() {
        assert_eq!(
            strip_curl_prefix("curl https://x.test/"),
            " https://x.test/"
        );
        assert_eq!(
            strip_curl_prefix("crul https://x.test/"),
            " https://x.test/"
        );
        assert_eq!(
            strip_curl_prefix("curllll https://x.test/"),
            " https://x.test/"
        );
        // A command that does not start with one of those four characters is
        // untouched — which is why `not a curl command` keeps its `not`.
        assert_eq!(
            strip_curl_prefix("not a curl command"),
            "not a curl command"
        );
    }

    #[test]
    fn a_backslash_newline_continuation_becomes_a_space() {
        let request = parse("curl 'https://x.test/' \\\n  -H 'A: 1'");
        assert_eq!(request.url, "https://x.test/");
        assert_eq!(request.headers["A"], "1");
    }

    // -- §5.3's test-pinned examples --------------------------------------

    #[test]
    fn a_bare_curl_command_is_a_get_with_no_body() {
        let request = parse("curl https://example.com");
        assert_eq!(request.method, "get");
        assert_eq!(request.url, "https://example.com");
        assert_eq!(request.data, None);
        assert_eq!(request.json, None);
        assert!(request.params.is_empty());
        assert!(request.headers.is_empty());
        assert!(request.cookies.is_empty());
        assert_eq!(request.proxy, None);
        assert_eq!(
            request.follow_redirects,
            FollowRedirects::Mode(RedirectMode::Safe)
        );
    }

    #[test]
    fn form_data_stays_data_because_it_is_not_json() {
        let request = parse("curl https://x.test/ -X POST -d 'key=value&foo=bar'");
        assert_eq!(request.method, "post");
        assert_eq!(
            request.data,
            Some(CurlBody::Text("key=value&foo=bar".to_owned()))
        );
        assert_eq!(request.json, None);
        assert!(
            request.params.is_empty(),
            "`-G` was not given, so nothing moves into the query"
        );
    }

    #[test]
    fn a_json_body_is_promoted_out_of_data() {
        let request = parse(r#"curl https://x.test/ -X POST --data-raw '{"key": "value"}'"#);
        assert_eq!(request.method, "post");
        assert_eq!(request.data, None);
        assert_eq!(
            request.json,
            Some(serde_json::json!({"key": "value"})),
            "a dict is promoted"
        );
    }

    #[test]
    fn a_json_array_body_is_promoted_too_but_a_scalar_is_not() {
        assert_eq!(
            parse("curl https://x.test/ --data-raw '[1, 2]'").json,
            Some(serde_json::json!([1, 2]))
        );
        // `orjson.loads("123")` succeeds but is neither dict nor list.
        let scalar = parse("curl https://x.test/ -X POST --data-raw '123'");
        assert_eq!(scalar.json, None);
        assert_eq!(scalar.data, Some(CurlBody::Text("123".to_owned())));
        // `null` likewise.
        assert_eq!(
            parse("curl https://x.test/ -X POST --data-raw 'null'").data,
            Some(CurlBody::Text("null".to_owned()))
        );
    }

    #[test]
    fn cookies_from_dash_b_merge_over_the_cookie_header() {
        let request =
            parse("curl https://x.test/ -H 'Cookie: session=abc123; user=john' -b 'extra=cookie'");
        assert_eq!(request.cookies.len(), 3);
        assert_eq!(request.cookies["session"], "abc123");
        assert_eq!(request.cookies["user"], "john");
        assert_eq!(request.cookies["extra"], "cookie");
        assert!(
            !request.headers.contains_key("Cookie"),
            "the Cookie header is diverted, not duplicated"
        );
    }

    #[test]
    fn a_dash_b_cookie_overwrites_a_header_cookie_of_the_same_name() {
        let request =
            parse("curl https://x.test/ -H 'Cookie: session=from-header' -b 'session=from-b'");
        assert_eq!(request.cookies["session"], "from-b");
    }

    #[test]
    fn a_proxy_user_is_spliced_into_the_proxy_url() {
        let request = parse("curl https://x.test/ -x http://proxy:8080 -U user:pass");
        let proxy = request.proxy.expect("a proxy");
        assert_eq!(proxy.http.as_deref(), Some("http://user:pass@proxy:8080"));
        assert_eq!(proxy.https, proxy.http, "both schemes get the same URL");
    }

    #[test]
    fn a_scheme_less_proxy_gets_http_prepended() {
        let request = parse("curl https://x.test/ -x proxy:8080");
        assert_eq!(
            request.proxy.expect("a proxy").http.as_deref(),
            Some("http://proxy:8080")
        );
    }

    #[test]
    fn an_existing_userinfo_in_the_proxy_url_is_replaced_not_appended() {
        let request = parse("curl https://x.test/ -x http://old:creds@proxy:8080 -U new:pair");
        assert_eq!(
            request.proxy.expect("a proxy").http.as_deref(),
            Some("http://new:pair@proxy:8080")
        );
    }

    #[test]
    fn a_proxy_path_survives_the_userinfo_splice() {
        assert_eq!(
            splice_userinfo("http://p:1/path?q=1", "u:p"),
            "http://u:p@p:1/path?q=1"
        );
        assert_eq!(splice_userinfo("socks5://p:1", "u:p"), "socks5://u:p@p:1");
    }

    /// `cli-ai.md` §5.3: `parse('not a curl command')` raises `AttributeError`.
    #[test]
    fn a_non_curl_command_reports_the_leftover_tokens() {
        let error = parse_curl("not a curl command").unwrap_err();
        assert_eq!(
            error,
            CurlParseError::UnknownArguments(vec!["curl".to_owned(), "command".to_owned()]),
            "`not` becomes the placeholder and `a` becomes the url"
        );
        assert!(error.is_reraised(), "Python re-raises this one");
        assert_eq!(
            error.to_string(),
            "Unknown/Unsupported curl arguments: ['curl', 'command']"
        );
    }

    // -- method inference ---------------------------------------------------

    #[test]
    fn data_without_an_explicit_method_infers_post() {
        for flag in ["-d 'a=1'", "--data-raw 'a=1'", "--data-binary 'a=1'"] {
            let request = parse(&format!("curl https://x.test/ {flag}"));
            assert_eq!(request.method, "post", "{flag}");
        }
        assert_eq!(
            parse("curl https://x.test/ --data-urlencode 'a=1'").method,
            "post"
        );
    }

    #[test]
    fn an_explicit_method_is_trimmed_and_lowercased() {
        assert_eq!(parse("curl https://x.test/ -X ' PATCH '").method, "patch");
        assert_eq!(parse("curl https://x.test/ -X DELETE").method, "delete");
    }

    #[test]
    fn dash_g_forces_get_even_over_an_explicit_method() {
        let request = parse("curl https://x.test/ -G -X POST -d 'a=1&b=2'");
        assert_eq!(request.method, "get");
        assert_eq!(request.params["a"], "1");
        assert_eq!(request.params["b"], "2");
        assert_eq!(request.data, None, "the body moved into the query");
        assert_eq!(request.json, None);
    }

    #[test]
    fn dash_g_with_a_json_looking_body_still_clears_it() {
        // `-G` runs after the JSON promotion, and its `if params:` branch clears
        // both. A JSON object has no `=`, so `parse_qsl` finds nothing and the
        // promotion survives — pinned so the ordering cannot be "simplified".
        let request = parse(r#"curl https://x.test/ -G --data-raw '{"a": 1}'"#);
        assert_eq!(request.method, "get");
        assert!(request.params.is_empty(), "{:?}", request.params);
        assert_eq!(request.json, Some(serde_json::json!({"a": 1})));
    }

    #[test]
    fn an_unsupported_method_is_only_rejected_at_send_time() {
        let request = parse("curl https://x.test/ -X PATCH");
        assert_eq!(request.supported_method(), None);
        assert!(!request.method_takes_a_body());
    }

    #[test]
    fn only_post_and_put_carry_a_body_delete_does_not() {
        assert!(parse("curl https://x.test/ -X POST -d a=1").method_takes_a_body());
        assert!(parse("curl https://x.test/ -X PUT -d a=1").method_takes_a_body());
        assert!(!parse("curl https://x.test/ -X DELETE -d a=1").method_takes_a_body());
        assert!(!parse("curl https://x.test/ -G").method_takes_a_body());
    }

    // -- body precedence ----------------------------------------------------

    #[test]
    fn data_binary_beats_data_raw_beats_data_beats_urlencode() {
        let request = parse(
            "curl https://x.test/ --data-binary 'BIN' --data-raw 'RAW' -d 'PLAIN' \
             --data-urlencode 'a=1'",
        );
        assert_eq!(request.data, Some(CurlBody::Bytes(b"BIN".to_vec())));

        let request =
            parse("curl https://x.test/ --data-raw 'RAW' -d 'PLAIN' --data-urlencode 'a=1'");
        assert_eq!(request.data, Some(CurlBody::Text("RAW".to_owned())));

        let request = parse("curl https://x.test/ -d 'PLAIN' --data-urlencode 'a=1'");
        assert_eq!(request.data, Some(CurlBody::Text("PLAIN".to_owned())));

        let request =
            parse("curl https://x.test/ -X POST --data-urlencode 'a=1' --data-urlencode 'b=2'");
        let Some(CurlBody::Form(pairs)) = request.data else {
            panic!("expected a form body, got {:?}", request.data)
        };
        assert_eq!(pairs["a"], "1");
        assert_eq!(pairs["b"], "2");
    }

    #[test]
    fn data_raw_loses_its_leading_dollars() {
        assert_eq!(
            parse("curl https://x.test/ --data-raw '$$$a=1'").data,
            Some(CurlBody::Text("a=1".to_owned())),
            "`.lstrip('$')` eats every leading `$`, not just one"
        );
        assert_eq!(
            parse("curl https://x.test/ --data-raw 'a=$1'").data,
            Some(CurlBody::Text("a=$1".to_owned())),
            "only leading ones"
        );
    }

    #[test]
    fn a_binary_body_is_never_promoted_to_json() {
        let request = parse(r#"curl https://x.test/ --data-binary '{"a": 1}'"#);
        assert_eq!(request.json, None, "step 7 only looks at `str` bodies");
        assert_eq!(request.data, Some(CurlBody::Bytes(br#"{"a": 1}"#.to_vec())));
    }

    // -- the argparse emulation --------------------------------------------

    #[test]
    fn a_long_option_accepts_an_inline_value() {
        let request = parse("curl https://x.test/ --request=PUT --header=A:1");
        assert_eq!(request.method, "put");
        assert_eq!(request.headers["A"], "1");
    }

    #[test]
    fn a_short_option_accepts_attached_inline_and_separate_values() {
        for form in ["-XPOST", "-X=POST", "-X POST"] {
            assert_eq!(
                parse(&format!("curl https://x.test/ {form}")).method,
                "post",
                "{form}"
            );
        }
    }

    #[test]
    fn short_flags_cluster() {
        // `-ksv` is three flags; the command still parses as a plain GET.
        let request = parse("curl https://x.test/ -ksv --compressed -i");
        assert_eq!(request.method, "get");
        assert_eq!(request.url, "https://x.test/");
    }

    #[test]
    fn an_unambiguous_long_abbreviation_is_accepted() {
        assert_eq!(
            parse("curl https://x.test/ --head 'A: 1'").headers["A"],
            "1"
        );
        assert_eq!(
            parse("curl https://x.test/ --data-r 'RAW'").data,
            Some(CurlBody::Text("RAW".to_owned()))
        );
    }

    #[test]
    fn an_ambiguous_long_abbreviation_names_every_candidate() {
        let error = parse_curl("curl https://x.test/ --data-").unwrap_err();
        let CurlParseError::AmbiguousOption { given, candidates } = error else {
            panic!("expected an ambiguity, got {error:?}")
        };
        assert_eq!(given, "--data-");
        assert_eq!(
            candidates,
            ["--data-raw", "--data-binary", "--data-urlencode"]
        );
    }

    /// `--data` is an exact match even though three longer options start with
    /// it — argparse checks for an exact hit before it considers prefixes.
    #[test]
    fn an_exact_match_wins_over_longer_options_with_the_same_prefix() {
        assert_eq!(
            parse("curl https://x.test/ --data 'PLAIN'").data,
            Some(CurlBody::Text("PLAIN".to_owned()))
        );
    }

    #[test]
    fn an_unknown_option_is_collected_rather_than_rejected_outright() {
        let error = parse_curl("curl https://x.test/ --nonsense").unwrap_err();
        assert_eq!(
            error,
            CurlParseError::UnknownArguments(vec!["--nonsense".to_owned()])
        );
    }

    #[test]
    fn an_option_missing_its_value_is_a_parse_error() {
        assert_eq!(
            parse_curl("curl https://x.test/ -X").unwrap_err(),
            CurlParseError::MissingValue("-X/--request")
        );
        assert_eq!(
            parse_curl("curl https://x.test/ -X -H 'A: 1'").unwrap_err(),
            CurlParseError::MissingValue("-X/--request"),
            "a token that looks like an option is not swallowed as a value"
        );
    }

    #[test]
    fn a_flag_given_an_explicit_value_is_rejected() {
        assert_eq!(
            parse_curl("curl https://x.test/ --get=yes").unwrap_err(),
            CurlParseError::UnexpectedValue("-G/--get")
        );
    }

    #[test]
    fn a_url_is_required() {
        assert_eq!(parse_curl("curl").unwrap_err(), CurlParseError::MissingUrl);
        assert_eq!(
            parse_curl("curl -k").unwrap_err(),
            CurlParseError::MissingUrl
        );
    }

    /// The positional pattern, per contiguous run: one argument fills `url`,
    /// two fill the suppressed placeholder and `url`, and the rest are extras.
    #[test]
    fn positionals_are_matched_run_by_run() {
        // One run of one: `url` takes it and the placeholder stays empty.
        assert_eq!(parse("curl https://x.test/").url, "https://x.test/");
        // A later run has no actions left, so it is an extra.
        assert_eq!(
            parse_curl("curl https://x.test/ -k stray").unwrap_err(),
            CurlParseError::UnknownArguments(vec!["stray".to_owned()])
        );
        // One run of two fills both actions.
        let request = parse("curl one https://x.test/");
        assert_eq!(request.url, "https://x.test/");
    }

    #[test]
    fn an_unterminated_quote_is_reported_not_panicked_on() {
        let error = parse_curl("curl 'https://x.test/").unwrap_err();
        assert!(
            matches!(error, CurlParseError::Unsplittable(_)),
            "{error:?}"
        );
        assert!(!error.is_reraised(), "Python returns None for this one");
    }

    #[test]
    fn a_double_dash_ends_the_options() {
        let request = parse("curl -- https://x.test/");
        assert_eq!(request.url, "https://x.test/");
    }

    // -- the token form ----------------------------------------------------

    #[test]
    fn already_split_tokens_parse_the_same_way() {
        let tokens: Vec<String> = ["curl", "https://x.test/", "-H", "A: 1", "-X", "POST"]
            .iter()
            .map(|token| (*token).to_owned())
            .collect();
        let request = parse_curl_tokens(&tokens).expect("parses");
        assert_eq!(request.method, "post");
        assert_eq!(request.url, "https://x.test/");
        assert_eq!(request.headers["A"], "1");
    }

    #[test]
    fn the_token_form_drops_only_a_literal_leading_curl() {
        let tokens: Vec<String> = ["https://x.test/"]
            .iter()
            .map(|t| (*t).to_owned())
            .collect();
        assert_eq!(
            parse_curl_tokens(&tokens).expect("parses").url,
            "https://x.test/"
        );
    }

    // -- headers ------------------------------------------------------------

    #[test]
    fn headers_split_on_the_first_colon_only_and_keep_their_case() {
        let request = parse("curl https://x.test/ -H 'Authorization: Bearer: token123'");
        assert_eq!(request.headers["Authorization"], "Bearer: token123");
    }

    #[test]
    fn a_repeated_header_overwrites() {
        let request = parse("curl https://x.test/ -H 'A: 1' -H 'A: 2'");
        assert_eq!(request.headers["A"], "2");
        assert_eq!(request.headers.len(), 1);
    }

    // -- JSON output --------------------------------------------------------

    #[test]
    fn the_json_rendering_keeps_the_namedtuple_field_order() {
        let request = parse("curl https://x.test/ -H 'A: 1'");
        let rendered = request.to_json();
        let keys: Vec<&String> = rendered.as_object().expect("an object").keys().collect();
        assert_eq!(
            keys,
            [
                "method",
                "url",
                "params",
                "data",
                "json_data",
                "headers",
                "cookies",
                "proxy",
                "follow_redirects"
            ]
        );
        assert_eq!(rendered["follow_redirects"], "safe");
        assert_eq!(rendered["data"], serde_json::Value::Null);
    }

    #[test]
    fn a_form_body_renders_as_an_object_and_bytes_render_as_their_text() {
        let form = CurlBody::Form(
            [("a".to_owned(), "1".to_owned())]
                .into_iter()
                .collect::<OrderedMap>(),
        );
        assert_eq!(form.to_json(), serde_json::json!({"a": "1"}));
        assert_eq!(
            CurlBody::Bytes(b"hi".to_vec()).to_json(),
            serde_json::Value::String("hi".to_owned())
        );
    }

    // -- `parse_qsl` --------------------------------------------------------

    #[test]
    fn parse_qsl_keeps_blank_values_and_lets_the_last_duplicate_win() {
        let parsed = parse_qsl("a=1&b=&a=2");
        assert_eq!(parsed["a"], "2");
        assert_eq!(parsed["b"], "");
        assert_eq!(parsed.len(), 2);
        assert_eq!(
            parsed.keys().collect::<Vec<_>>(),
            ["a", "b"],
            "a repeated key keeps its first position, like `dict`"
        );
    }

    #[test]
    fn parse_qsl_decodes_percent_escapes_and_plus_signs() {
        let parsed = parse_qsl("q=a%20b&r=c+d");
        assert_eq!(parsed["q"], "a b");
        assert_eq!(parsed["r"], "c d");
    }

    // -- error rendering ----------------------------------------------------

    #[test]
    fn python_list_rendering_quotes_the_way_python_does() {
        assert_eq!(python_list(&[]), "[]");
        assert_eq!(python_list(&["a".to_owned()]), "['a']");
        assert_eq!(python_list(&["a".to_owned(), "b".to_owned()]), "['a', 'b']");
    }
}
