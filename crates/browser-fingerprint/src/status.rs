//! [`StatusText`]: HTTP status code → reason phrase, as a compile-time
//! `phf::Map` with the `"Unknown Status Code"` fallback.
//!
//! Deliberately **not** [`http::StatusCode::canonical_reason`]: the phrase set
//! differs. This table says `"Payload Too Large"` (not "Content Too Large"),
//! `"Unprocessable Entity"` (not "Unprocessable Content"), `"I'm a teapot"`,
//! and it omits 306/309/509/510-adjacent codes by design.
//!
//! [`http::StatusCode::canonical_reason`]: https://docs.rs/http/latest/http/status/struct.StatusCode.html#method.canonical_reason

/// The reason phrase returned for any code absent from [`PHRASES`].
pub const UNKNOWN_STATUS_TEXT: &str = "Unknown Status Code";

/// The status-code → reason-phrase table, transcribed from
/// `reference/engines/toolbelt/custom.py::StatusText._phrases`.
pub static PHRASES: phf::Map<u16, &'static str> = phf::phf_map! {
    100u16 => "Continue",
    101u16 => "Switching Protocols",
    102u16 => "Processing",
    103u16 => "Early Hints",
    200u16 => "OK",
    201u16 => "Created",
    202u16 => "Accepted",
    203u16 => "Non-Authoritative Information",
    204u16 => "No Content",
    205u16 => "Reset Content",
    206u16 => "Partial Content",
    207u16 => "Multi-Status",
    208u16 => "Already Reported",
    226u16 => "IM Used",
    300u16 => "Multiple Choices",
    301u16 => "Moved Permanently",
    302u16 => "Found",
    303u16 => "See Other",
    304u16 => "Not Modified",
    305u16 => "Use Proxy",
    307u16 => "Temporary Redirect",
    308u16 => "Permanent Redirect",
    400u16 => "Bad Request",
    401u16 => "Unauthorized",
    402u16 => "Payment Required",
    403u16 => "Forbidden",
    404u16 => "Not Found",
    405u16 => "Method Not Allowed",
    406u16 => "Not Acceptable",
    407u16 => "Proxy Authentication Required",
    408u16 => "Request Timeout",
    409u16 => "Conflict",
    410u16 => "Gone",
    411u16 => "Length Required",
    412u16 => "Precondition Failed",
    413u16 => "Payload Too Large",
    414u16 => "URI Too Long",
    415u16 => "Unsupported Media Type",
    416u16 => "Range Not Satisfiable",
    417u16 => "Expectation Failed",
    418u16 => "I'm a teapot",
    421u16 => "Misdirected Request",
    422u16 => "Unprocessable Entity",
    423u16 => "Locked",
    424u16 => "Failed Dependency",
    425u16 => "Too Early",
    426u16 => "Upgrade Required",
    428u16 => "Precondition Required",
    429u16 => "Too Many Requests",
    431u16 => "Request Header Fields Too Large",
    451u16 => "Unavailable For Legal Reasons",
    500u16 => "Internal Server Error",
    501u16 => "Not Implemented",
    502u16 => "Bad Gateway",
    503u16 => "Service Unavailable",
    504u16 => "Gateway Timeout",
    505u16 => "HTTP Version Not Supported",
    506u16 => "Variant Also Negotiates",
    507u16 => "Insufficient Storage",
    508u16 => "Loop Detected",
    510u16 => "Not Extended",
    511u16 => "Network Authentication Required",
};

/// Namespace for the status-phrase lookup, mirroring Python's `StatusText`.
///
/// This is a zero-sized carrier type; there is nothing to construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct StatusText;

impl StatusText {
    /// The reason phrase for `status_code`, or `"Unknown Status Code"`.
    ///
    /// Python memoises this with `lru_cache(maxsize=128)`; a perfect-hash
    /// lookup is cheaper than a cache probe, so the cache is dropped.
    ///
    /// # Examples
    ///
    /// ```
    /// use browser_fingerprint::status::StatusText;
    ///
    /// assert_eq!(StatusText::get(404), "Not Found");
    /// assert_eq!(StatusText::get(418), "I'm a teapot");
    /// assert_eq!(StatusText::get(306), "Unknown Status Code");
    /// ```
    #[must_use]
    pub fn get(status_code: u16) -> &'static str {
        PHRASES
            .get(&status_code)
            .copied()
            .unwrap_or(UNKNOWN_STATUS_TEXT)
    }

    /// The reason phrase for `status_code`, or `None` when it is unknown.
    ///
    /// Useful when a caller wants to distinguish "no phrase" from the literal
    /// fallback string.
    #[must_use]
    pub fn try_get(status_code: u16) -> Option<&'static str> {
        PHRASES.get(&status_code).copied()
    }

    /// The number of codes in the table.
    #[must_use]
    pub fn len() -> usize {
        PHRASES.len()
    }

    /// Always `false`; the table is a non-empty compile-time constant.
    #[must_use]
    pub fn is_empty() -> bool {
        PHRASES.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_size_matches_the_python_source() {
        // reference/engines/toolbelt/custom.py::StatusText._phrases has 62
        // entries; toolbelt.md §2.3's "58" miscounts the same list.
        assert_eq!(StatusText::len(), 62);
        assert!(!StatusText::is_empty());
    }

    #[test]
    fn known_phrases() {
        assert_eq!(StatusText::get(100), "Continue");
        assert_eq!(StatusText::get(200), "OK");
        assert_eq!(StatusText::get(203), "Non-Authoritative Information");
        assert_eq!(StatusText::get(226), "IM Used");
        assert_eq!(StatusText::get(308), "Permanent Redirect");
        assert_eq!(StatusText::get(413), "Payload Too Large");
        assert_eq!(StatusText::get(418), "I'm a teapot");
        assert_eq!(StatusText::get(422), "Unprocessable Entity");
        assert_eq!(StatusText::get(451), "Unavailable For Legal Reasons");
        assert_eq!(StatusText::get(511), "Network Authentication Required");
    }

    #[test]
    fn codes_absent_by_design_fall_back() {
        for code in [0, 1, 99, 306, 309, 399, 419, 430, 509, 512, 599, 999] {
            assert_eq!(StatusText::get(code), UNKNOWN_STATUS_TEXT, "code {code}");
            assert_eq!(StatusText::try_get(code), None, "code {code}");
        }
    }

    #[test]
    fn phrases_differ_from_the_http_crate_defaults() {
        // The two places `http`'s canonical_reason would disagree.
        assert_ne!(StatusText::get(413), "Content Too Large");
        assert_ne!(StatusText::get(422), "Unprocessable Content");
    }
}
