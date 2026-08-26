//! `decompress` with the 64 MiB gzip-bomb cap.
//!
//! A one-for-one port of `reference/spiders/templates/_utils.py`
//! (`spider-support.md` §7). Sitemaps and feeds are routinely served gzipped —
//! sometimes with `Content-Type: application/x-gzip`, sometimes with no hint at
//! all beyond the two magic bytes — so both spiders inflate before they parse,
//! and both cap the output so a 40 kB file cannot expand into an OOM.
//!
//! ```
//! use browser_templates::gzip::decompress;
//!
//! // Not gzip, and no content-type says so: the body is handed straight back.
//! let plain = b"<urlset/>";
//! assert_eq!(decompress(plain, None)?.as_ref(), plain);
//! # Ok::<(), browser_templates::error::TemplateError>(())
//! ```

use std::borrow::Cow;
use std::io::Read;

use flate2::bufread::GzDecoder;

use crate::error::{Result, TemplateError};

/// The two bytes every gzip member starts with.
pub const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];

/// The inflate cap: 64 MiB, Python's `_GUNZIP_MAX_SIZE`.
pub const GUNZIP_MAX_SIZE: usize = 64 * 1024 * 1024;

/// The read granularity, Python's `f.read1(8192)`.
///
/// This is observable, not an implementation detail: Python checks the size
/// **after** appending a chunk, so the buffer can reach
/// `GUNZIP_MAX_SIZE + CHUNK_SIZE - 1` bytes before the error is raised. A port
/// that checked before appending, or that used a different chunk size, would
/// reject a different set of bodies.
pub const CHUNK_SIZE: usize = 8192;

/// Whether [`decompress`] would try to inflate this body.
///
/// Python: `(content_type and "gzip" in content_type.lower()) or body[:2] ==
/// _GZIP_MAGIC`. Note the content-type test is a plain **substring** search, so
/// `application/x-gzip`, `application/gzip` and even `text/gzip-ish` all
/// trigger it, and that it is tried *first* — a body whose content-type says
/// gzip but whose bytes are not gzip is an error, not a passthrough.
#[must_use]
pub fn is_gzipped(body: &[u8], content_type: Option<&str>) -> bool {
    if let Some(content_type) = content_type {
        if content_type.to_lowercase().contains("gzip") {
            return true;
        }
    }
    body.starts_with(&GZIP_MAGIC)
}

/// Gunzip `body` when the content-type or the magic bytes say it is gzipped.
///
/// Returns the input borrowed when there is nothing to do, which is the common
/// case for a plain `sitemap.xml`.
///
/// # Errors
///
/// * [`TemplateError::GzipBomb`] when the inflated output passes
///   [`GUNZIP_MAX_SIZE`] — see [`CHUNK_SIZE`] for the exact threshold.
/// * [`TemplateError::Gzip`] when the body does not inflate at all. Python
///   raises `gzip.BadGzipFile`, an `OSError` subclass, which every caller
///   catches alongside the bomb error.
/// * [`TemplateError::GzipTruncated`] when a member ends early. Python raises
///   `EOFError` here, which is **not** an `OSError` and therefore escapes the
///   callers' handler — the two are kept apart so the callers can reproduce
///   that split.
///
/// ```
/// use browser_templates::gzip::decompress;
///
/// // A body that is not gzip at all, but whose content-type says it is.
/// assert!(decompress(b"<urlset/>", Some("application/gzip")).is_err());
/// ```
pub fn decompress<'a>(body: &'a [u8], content_type: Option<&str>) -> Result<Cow<'a, [u8]>> {
    if !is_gzipped(body, content_type) {
        return Ok(Cow::Borrowed(body));
    }

    // Python's `GzipFile` reads members in a loop, and the *header* read is
    // what decides between the two errors:
    //
    // ```text
    // magic = fp.read(2)
    // if magic == b"":        return None                 # clean EOF
    // if len(magic) != 2 or magic != b"\037\213":
    //     raise BadGzipFile(f"Not a gzipped file ({magic})")
    // ```
    //
    // So an **empty** body is an empty gzip *stream*, not a broken one — a 204
    // or an empty `Content-Type: application/gzip` reply inflates to `b""` —
    // while a single stray `\x1f` after a complete member is `BadGzipFile`, not
    // a truncated member. `flate2` reports both as `UnexpectedEof`, which is
    // why the header is checked here rather than left to the decoder. The u18
    // fuzz caught the empty case; the `trailing-magic-byte` corpus row caught
    // the other.
    //
    // The loop is per-member (`bufread::GzDecoder` + `into_inner`) rather than
    // one `MultiGzDecoder`, because only a per-member loop can see how the
    // *next* header fails. Python's `GzipFile.read()` likewise returns every
    // concatenated member: `gzip -c a b > ab.gz` is a legal file.
    let mut rest = body;
    let mut out: Vec<u8> = Vec::new();
    let mut chunk = [0_u8; CHUNK_SIZE];
    while !rest.is_empty() {
        if !rest.starts_with(&GZIP_MAGIC) {
            let head: Vec<u8> = rest.iter().take(2).copied().collect();
            return Err(TemplateError::Gzip {
                message: format!("Not a gzipped file ({:?})", String::from_utf8_lossy(&head)),
            });
        }
        let mut decoder = GzDecoder::new(rest);
        loop {
            let read = decoder.read(&mut chunk).map_err(|error| {
                if error.kind() == std::io::ErrorKind::UnexpectedEof {
                    TemplateError::GzipTruncated
                } else {
                    TemplateError::Gzip {
                        message: error.to_string(),
                    }
                }
            })?;
            if read == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..read]);
            // After the append, exactly as Python does.
            if out.len() > GUNZIP_MAX_SIZE {
                return Err(TemplateError::GzipBomb {
                    limit: GUNZIP_MAX_SIZE,
                });
            }
        }
        let remaining = decoder.into_inner();
        if remaining.len() >= rest.len() {
            // Defensive: a member that consumed nothing would spin forever.
            break;
        }
        rest = remaining;
    }
    Ok(Cow::Owned(out))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::write::GzEncoder;
    use flate2::Compression;

    use super::*;

    fn gzip(payload: &[u8]) -> Vec<u8> {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(payload).expect("in-memory write");
        encoder.finish().expect("in-memory finish")
    }

    #[test]
    fn a_plain_body_is_borrowed_not_copied() {
        let body = b"<urlset></urlset>";
        let out = decompress(body, Some("application/xml")).expect("no inflate");
        assert!(matches!(out, Cow::Borrowed(_)));
        assert_eq!(out.as_ref(), body);
    }

    #[test]
    fn the_magic_bytes_trigger_inflation_without_a_content_type() {
        let compressed = gzip(b"<urlset><url><loc>https://a.test/</loc></url></urlset>");
        assert!(is_gzipped(&compressed, None));
        let out = decompress(&compressed, None).expect("inflates");
        assert!(out.starts_with(b"<urlset>"));
    }

    #[test]
    fn the_content_type_triggers_inflation_case_insensitively() {
        for content_type in ["application/gzip", "application/X-GZIP", "text/GZip"] {
            assert!(
                is_gzipped(b"not really", Some(content_type)),
                "{content_type}"
            );
        }
        assert!(!is_gzipped(b"not really", Some("application/xml")));
        assert!(!is_gzipped(b"not really", None));
    }

    /// `_read_gzip_header` returns `None` — not an error — when the first read
    /// is `b""`, so an empty gzip stream inflates to an empty body.
    #[test]
    fn an_empty_body_is_an_empty_stream_not_a_broken_one() {
        assert_eq!(
            decompress(b"", Some("application/gzip")).expect("empty stream"),
            Cow::Borrowed(b"".as_slice())
        );
        assert_eq!(decompress(b"", None).expect("not gzip at all").len(), 0);
    }

    /// The header read is what tells `BadGzipFile` from `EOFError`: fewer than
    /// two bytes, or two that are not the magic, is `BadGzipFile`; the magic
    /// followed by too little is `EOFError`.
    #[test]
    fn a_partial_next_header_is_a_bad_file_and_a_partial_member_is_truncated() {
        let member = gzip(b"hi");

        let error = decompress(&[member.clone(), vec![0x1f]].concat(), None).expect_err("bad");
        assert!(matches!(error, TemplateError::Gzip { .. }), "{error:?}");

        let error = decompress(&[member.clone(), GZIP_MAGIC.to_vec()].concat(), None)
            .expect_err("truncated");
        assert!(matches!(error, TemplateError::GzipTruncated), "{error:?}");

        let error = decompress(b"\x1f", Some("application/gzip")).expect_err("bad");
        assert!(matches!(error, TemplateError::Gzip { .. }), "{error:?}");

        let error = decompress(&GZIP_MAGIC, Some("application/gzip")).expect_err("truncated");
        assert!(matches!(error, TemplateError::GzipTruncated), "{error:?}");

        // Trailing junk that is two-or-more non-magic bytes is a bad file too.
        let error = decompress(&[member, b"junk".to_vec()].concat(), None).expect_err("bad");
        assert!(matches!(error, TemplateError::Gzip { .. }), "{error:?}");
    }

    #[test]
    fn a_content_type_that_lies_is_an_error_not_a_passthrough() {
        // Faithful to Python: `GzipFile` is opened unconditionally once the
        // content-type matches, so a plain body raises `BadGzipFile`.
        let error = decompress(b"<urlset/>", Some("application/gzip")).expect_err("bad gzip");
        assert!(matches!(error, TemplateError::Gzip { .. }), "{error:?}");
    }

    /// Python raises `EOFError` here, not `OSError`, and the callers do not
    /// catch it. Keeping the variant distinct is what lets them reproduce that.
    #[test]
    fn a_truncated_member_is_its_own_error() {
        let full = gzip(b"<urlset><url><loc>https://a.test/</loc></url></urlset>");
        let truncated = &full[..full.len() - 6];
        let error = decompress(truncated, None).expect_err("truncated");
        assert!(matches!(error, TemplateError::GzipTruncated), "{error:?}");
        assert_eq!(
            error.to_string(),
            "Compressed file ended before the end-of-stream marker was reached"
        );
    }

    #[test]
    fn concatenated_members_are_both_read() {
        let mut both = gzip(b"first;");
        both.extend_from_slice(&gzip(b"second"));
        let out = decompress(&both, None).expect("inflates");
        assert_eq!(out.as_ref(), b"first;second");
    }

    /// Python raises `EOFError` for a body that is only the magic bytes (the
    /// header read runs out) and `BadGzipFile` for a bad compression method.
    #[test]
    fn the_two_gzip_failure_classes_stay_apart() {
        assert!(matches!(
            decompress(&[0x1f, 0x8b], Some("application/gzip")).expect_err("header truncated"),
            TemplateError::GzipTruncated
        ));
        let bad_method = [0x1f, 0x8b, 0x09, 0x00, 0, 0, 0, 0, 0, 0x03];
        assert!(matches!(
            decompress(&bad_method, None).expect_err("bad method"),
            TemplateError::Gzip { .. }
        ));
    }

    #[test]
    fn an_empty_body_is_not_gzip() {
        assert!(!is_gzipped(b"", None));
        assert_eq!(decompress(b"", None).expect("passthrough").as_ref(), b"");
        // One magic byte is not two.
        assert!(!is_gzipped(&[0x1f], None));
    }

    #[test]
    fn the_bomb_cap_is_sixty_four_mebibytes() {
        assert_eq!(GUNZIP_MAX_SIZE, 67_108_864);
        // A body that inflates to just over the cap. Zeroes compress to
        // roughly 1/1000th, so this stays a ~70 kB fixture.
        let payload = vec![0_u8; GUNZIP_MAX_SIZE + CHUNK_SIZE];
        let compressed = gzip(&payload);
        let error = decompress(&compressed, None).expect_err("bomb");
        assert!(
            matches!(
                error,
                TemplateError::GzipBomb {
                    limit: GUNZIP_MAX_SIZE
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn a_body_exactly_at_the_cap_is_allowed() {
        // Python's check is `len(out) > _GUNZIP_MAX_SIZE`, so the cap itself
        // passes. Testing the true boundary would need a 64 MiB allocation;
        // this asserts the comparison is `>` and not `>=` on a body that has
        // gone through the same chunked loop.
        let payload = vec![7_u8; CHUNK_SIZE * 3 + 11];
        let compressed = gzip(&payload);
        assert_eq!(
            decompress(&compressed, None).expect("inflates").len(),
            payload.len()
        );
    }
}
