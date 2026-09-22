//! Article parsing and semantic validation.
//!
//! Provides zero-copy views over complete NNTP article-family responses
//! (`ARTICLE`, `HEAD`, `BODY`, and `STAT`). Parsing validates the response shape
//! and retains borrowed ranges; it does not allocate an owned article or decode
//! yEnc payload bytes.

mod error;
mod headers;
pub(crate) mod state;
pub mod yenc;

pub use error::ParseError;
pub use headers::{HeaderIter, Headers};

use crate::types::protocol::MessageId;
use std::ops::Range;
use yenc::validate_yenc_structure;

/// Parsed NNTP article response as borrowed views into a framed response.
///
/// The fields present depend on the request-scoped response shape. A complete
/// `ARTICLE` response has both headers and a body, `HEAD` has headers only,
/// `BODY` has a body only, and `STAT` has neither. The view borrows the framed
/// bytes; it does not own or mutate them.
///
/// Different response codes populate different fields:
/// - 220 ARTICLE: headers + body
/// - 221 HEAD: headers only
/// - 222 BODY: body only
/// - 223 STAT: neither (just metadata)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Article<'a> {
    /// Message ID returned by the server.
    pub message_id: MessageId<'a>,
    /// Article number returned by the server, when the response supplied one.
    pub article_number: Option<u64>,
    /// Parsed headers for `ARTICLE` and `HEAD` responses.
    pub headers: Option<Headers<'a>>,
    /// Article body bytes for `ARTICLE` and `BODY` responses.
    pub body: Option<&'a [u8]>,
}

/// Consumer-facing view of an article whose framing and semantic validation
/// have already been handled.
///
/// This is an alias for [`Article`]. The alias names the stronger construction
/// boundary used by [`crate::client::ValidatedArticle`]; it does not allocate,
/// copy, or revalidate the response bytes.
pub type ArticleView<'a> = Article<'a>;

/// Optional yEnc policy applied after NNTP article structure is validated.
///
/// Enabling this policy checks yEnc structure when the framed response is
/// consumed. It does not decode the body; call [`Article::decode`] or
/// [`Article::decode_into`] when decoded bytes are required.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YencValidation {
    /// Validate NNTP structure without inspecting yEnc payload syntax.
    Disabled,
    /// Validate a body that begins with a yEnc header.
    Enabled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArticleLayout {
    message_id: Range<usize>,
    article_number: Option<u64>,
    content: ArticleContent,
}

/// Article-family response shape. Keeping the alternatives explicit prevents
/// impossible header/body combinations while preserving the proxy's raw-wire
/// representation and lenient article-number policy.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ArticleContent {
    Article {
        headers: Range<usize>,
        body: Range<usize>,
    },
    Head {
        headers: Range<usize>,
    },
    Body {
        body: Range<usize>,
    },
    Stat,
}

impl ArticleLayout {
    pub(crate) fn parse(buf: &[u8]) -> Result<Self, ParseError> {
        let status =
            crate::protocol::StatusCode::parse(buf).ok_or(ParseError::InvalidStatusCode(0))?;
        let status_code = status.as_u16();
        if !matches!(status_code, 220..=223) {
            return Err(ParseError::InvalidStatusCode(status_code));
        }
        let first_line_end = find_line_end(buf, 0)?;
        let (message_id, article_number) = parse_first_line_layout(&buf[..first_line_end])?;
        Self::from_first_line(
            buf,
            status,
            first_line_end,
            message_id,
            article_number,
            buf.len(),
        )
    }

    /// Parse article semantics using the status-line boundary already proven
    /// by the response framer. The boundary and bytes remain in one framed
    /// state, so this transition does not rediscover the first CRLF.
    pub(crate) fn parse_framed(
        buf: &[u8],
        status: crate::protocol::StatusCode,
        status_line_end: state::StatusLineEnd,
        content_end: state::ContentEnd,
    ) -> Result<Self, ParseError> {
        let status_code = status.as_u16();
        if !matches!(status_code, 220..=223) {
            return Err(ParseError::InvalidStatusCode(status_code));
        }
        let line_end = status_line_end.get();
        let content_end = content_end.get();
        let first_line_end = line_end
            .checked_sub(crate::protocol::CRLF.len())
            .ok_or(ParseError::BufferTooShort)?;
        if line_end > content_end
            || content_end > buf.len()
            || buf.get(first_line_end..line_end) != Some(crate::protocol::CRLF)
        {
            return Err(ParseError::BufferTooShort);
        }
        let (message_id, article_number) = parse_first_line_layout(&buf[..first_line_end])?;
        Self::from_first_line(
            buf,
            status,
            first_line_end,
            message_id,
            article_number,
            content_end,
        )
    }

    fn from_first_line(
        buf: &[u8],
        status: crate::protocol::StatusCode,
        first_line_end: usize,
        message_id: Range<usize>,
        article_number: Option<u64>,
        content_end: usize,
    ) -> Result<Self, ParseError> {
        let framed = buf.get(..content_end).ok_or(ParseError::BufferTooShort)?;
        let content_start = first_line_end
            .checked_add(crate::protocol::CRLF.len())
            .ok_or(ParseError::BufferTooShort)?;

        let content = match status.as_u16() {
            220 => {
                let separator_pos = find_blank_line(framed, content_start)?;
                let headers_range = content_start..separator_pos;
                Headers::parse(&framed[headers_range.clone()])?;
                let body_range = separator_pos + 4..content_end;
                ArticleContent::Article {
                    headers: headers_range,
                    body: body_range,
                }
            }
            221 => {
                if find_blank_line(framed, content_start).is_ok() {
                    return Err(ParseError::UnexpectedBody);
                }
                let headers_range = content_start..content_end;
                Headers::parse(&framed[headers_range.clone()])?;
                ArticleContent::Head {
                    headers: headers_range,
                }
            }
            222 => {
                let body_range = content_start..content_end;
                ArticleContent::Body { body: body_range }
            }
            223 => {
                if content_start < content_end {
                    return Err(ParseError::UnexpectedBody);
                }
                ArticleContent::Stat
            }
            _ => unreachable!("article status was checked above"),
        };

        Ok(Self {
            message_id,
            article_number,
            content,
        })
    }

    /// Apply the optional encoding policy after NNTP structure is validated.
    ///
    /// The validated article state therefore proves protocol semantics only;
    /// callers choose whether yEnc is relevant to their operation.
    pub(crate) fn validate_yenc(&self, buf: &[u8]) -> Result<(), ParseError> {
        let body = match &self.content {
            ArticleContent::Article { body, .. } | ArticleContent::Body { body } => {
                &buf[body.clone()]
            }
            ArticleContent::Head { .. } | ArticleContent::Stat => return Ok(()),
        };
        if body.get(..b"=ybegin".len()) == Some(b"=ybegin") {
            validate_yenc_structure(body)?;
        }
        Ok(())
    }

    pub(crate) fn view<'a>(&self, buf: &'a [u8]) -> Article<'a> {
        let message_id = std::str::from_utf8(&buf[self.message_id.clone()])
            .expect("validated message ID remains UTF-8");
        let (headers, body) = match &self.content {
            ArticleContent::Article { headers, body } => (
                Some(Headers::from_validated(&buf[headers.clone()])),
                Some(&buf[body.clone()]),
            ),
            ArticleContent::Head { headers } => {
                (Some(Headers::from_validated(&buf[headers.clone()])), None)
            }
            ArticleContent::Body { body } => (None, Some(&buf[body.clone()])),
            ArticleContent::Stat => (None, None),
        };
        Article {
            message_id: MessageId::from_validated(message_id),
            article_number: self.article_number,
            headers,
            body,
        }
    }
}

impl<'a> TryFrom<&'a [u8]> for Article<'a> {
    type Error = ParseError;

    /// Parse article with yEnc validation enabled by default
    /// Use `Article::parse(buf, false)` to disable validation
    fn try_from(buf: &'a [u8]) -> Result<Self, Self::Error> {
        Self::parse(buf, true)
    }
}

impl<'a> Article<'a> {
    /// Parse NNTP article response with optional yEnc validation
    ///
    /// # Arguments
    /// * `buf` - Complete framed response bytes from the session response
    ///   reader. The framer has already consumed the multiline terminator.
    /// * `validate_yenc` - Whether to validate yEnc structure/checksums
    ///
    /// # Errors
    /// Returns `ParseError` when the NNTP response status line, message metadata,
    /// headers, body structure, or optional yEnc validation fails.
    pub fn parse(buf: &'a [u8], validate_yenc: bool) -> Result<Self, ParseError> {
        let policy = match validate_yenc {
            true => YencValidation::Enabled,
            false => YencValidation::Disabled,
        };
        Self::parse_with_yenc(buf, policy)
    }

    /// Parse NNTP structure and apply the selected optional yEnc policy.
    pub fn parse_with_yenc(buf: &'a [u8], policy: YencValidation) -> Result<Self, ParseError> {
        let layout = ArticleLayout::parse(buf)?;
        match policy {
            YencValidation::Disabled => {}
            YencValidation::Enabled => layout.validate_yenc(buf)?,
        }
        Ok(layout.view(buf))
    }

    /// Decode yEnc-encoded body to raw bytes
    ///
    /// This method allocates a new `Vec<u8>` for each call. For hot paths where
    /// articles are frequently decoded, prefer [`Article::decode_into`] to
    /// reuse a caller-provided buffer and avoid per-call allocations.
    ///
    /// # Returns
    /// Decoded bytes, or `None` if body is not yEnc-encoded
    #[must_use]
    pub fn decode(&self) -> Option<Vec<u8>> {
        // Allocate once and delegate decoding to the zero-allocation helper
        let mut decoded = Vec::with_capacity(self.body?.len());
        if !self.decode_into(&mut decoded) {
            return None;
        }
        Some(decoded)
    }

    /// Decode yEnc-encoded body into the provided buffer
    ///
    /// This method reuses the capacity of `output` and performs no allocations
    /// if the buffer is already large enough, making it suitable for hot paths.
    ///
    /// # Behavior
    /// - Returns `false` if:
    ///   - The article has no body, or
    ///   - The body is not yEnc-encoded (does not start with `=ybegin`)
    /// - Returns `true` and writes the decoded bytes into `output` otherwise.
    ///
    /// On success, `output` is cleared before writing the decoded bytes.
    #[must_use]
    pub fn decode_into(&self, output: &mut Vec<u8>) -> bool {
        // Ensure we have a yEnc-encoded body
        let body = match self.body {
            Some(b) if b.starts_with(b"=ybegin") => b,
            _ => return false,
        };

        output.clear();

        // Skip the =ybegin line, strip trailing CRs, stop at =yend/=ypart,
        // and decode each line into the output buffer.
        for byte in body
            .split(|&b| b == b'\n')
            .skip(1) // Skip =ybegin line
            .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
            .take_while(|line| !line.starts_with(b"=yend") && !line.starts_with(b"=ypart"))
            .flat_map(yenc::decode_yenc_line)
        {
            output.push(byte);
        }

        true
    }
}

/// Parse status code from buffer
#[cfg(test)]
fn parse_status_code(buf: &[u8]) -> Result<u16, ParseError> {
    crate::protocol::StatusCode::parse(buf)
        .map(|sc| sc.as_u16())
        .ok_or(ParseError::InvalidStatusCode(0))
}

/// Parse first line to extract the message-ID range and optional article number.
fn parse_first_line_layout(line: &[u8]) -> Result<(Range<usize>, Option<u64>), ParseError> {
    // Format: "220 <number> <message-id> ..." or "220 0 <message-id> ..."

    // Find first space (after status code)
    let first_space = memchr::memchr(b' ', line)
        .ok_or_else(|| ParseError::InvalidMessageId("No space after status code".to_string()))?;

    // Find second space (after article number)
    let second_space = memchr::memchr(b' ', &line[first_space + 1..])
        .map(|pos| first_space + 1 + pos)
        .ok_or_else(|| ParseError::InvalidMessageId("No article number".to_string()))?;

    // Extract article number
    let number_bytes = &line[first_space + 1..second_space];
    let article_number = std::str::from_utf8(number_bytes)
        .ok()
        .and_then(|s| s.parse::<u64>().ok());

    // Find message-id (starts with '<')
    let msg_id_start = memchr::memchr(b'<', &line[second_space..])
        .map(|pos| second_space + pos)
        .ok_or_else(|| ParseError::InvalidMessageId("No '<' found".to_string()))?;

    // Find end of message-id (ends with '>')
    let msg_id_end = memchr::memchr(b'>', &line[msg_id_start..])
        .map(|pos| msg_id_start + pos + 1)
        .ok_or_else(|| ParseError::InvalidMessageId("No '>' found".to_string()))?;

    // Extract message-id
    let msg_id_bytes = &line[msg_id_start..msg_id_end];
    let msg_id_str = std::str::from_utf8(msg_id_bytes)
        .map_err(|_| ParseError::InvalidMessageId("Invalid UTF-8 in message-id".to_string()))?;
    MessageId::from_borrowed(msg_id_str)?;

    Ok((msg_id_start..msg_id_end, article_number))
}

/// Find end of line (\r in \r\n)
fn find_line_end(buf: &[u8], start: usize) -> Result<usize, ParseError> {
    for i in start..buf.len() {
        if buf[i] == b'\r' && i + 1 < buf.len() && buf[i + 1] == b'\n' {
            return Ok(i);
        }
    }
    Err(ParseError::BufferTooShort)
}

/// Find blank line separator (\r\n\r\n)
fn find_blank_line(buf: &[u8], start: usize) -> Result<usize, ParseError> {
    // Look for \r\n\r\n pattern
    for i in start..buf.len().saturating_sub(3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Ok(i);
        }
    }
    Err(ParseError::MissingSeparator)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_status_code() {
        assert_eq!(parse_status_code(b"220 OK"), Ok(220));
        assert_eq!(parse_status_code(b"221 OK"), Ok(221));
        assert!(parse_status_code(b"X").is_err());
    }

    #[test]
    fn test_find_blank_line() {
        let buf = b"220 0 <msg>\r\nSubject: Test\r\n\r\nBody";
        let pos = find_blank_line(buf, 12).unwrap();
        assert_eq!(&buf[pos..pos + 4], b"\r\n\r\n");
    }

    #[test]
    fn test_parse_article_220() {
        let buf = b"220 100 <test@example.com> article\r\n\
                    Subject: Test\r\n\
                    \r\n\
                    Body content\r\n";

        let article = Article::try_from(&buf[..]).unwrap();
        assert_eq!(article.message_id.as_str(), "<test@example.com>");
        assert_eq!(article.article_number, Some(100));
        assert!(article.headers.is_some());
        assert!(article.body.is_some());
    }

    #[test]
    fn framed_layout_reuses_the_proven_status_line_end() {
        let buf = b"222 100 <test@example.com> body\r\nBody content\r\n";
        let status_line_end = b"222 100 <test@example.com> body\r\n".len();
        let parsed = ArticleLayout::parse(buf).unwrap();
        let framed = ArticleLayout::parse_framed(
            buf,
            crate::protocol::StatusCode::new(222),
            state::StatusLineEnd::new(status_line_end),
            state::ContentEnd::new(buf.len()),
        )
        .unwrap();

        assert_eq!(framed, parsed);
        assert!(
            ArticleLayout::parse_framed(
                buf,
                crate::protocol::StatusCode::new(222),
                state::StatusLineEnd::new(status_line_end - 1),
                state::ContentEnd::new(buf.len()),
            )
            .is_err()
        );
        assert_eq!(
            ArticleLayout::parse_framed(
                buf,
                crate::protocol::StatusCode::new(430),
                state::StatusLineEnd::new(status_line_end),
                state::ContentEnd::new(buf.len()),
            ),
            Err(ParseError::InvalidStatusCode(430))
        );
    }

    #[test]
    fn framed_layout_excludes_packed_response_suffix() {
        let frame = b"222 100 <test@example.com> body\r\nBody content\r\n";
        let suffix = b"223 1 <next@example.com>\r\n";
        let packed = [frame.as_slice(), suffix.as_slice()].concat();
        let status_line_end = b"222 100 <test@example.com> body\r\n".len();

        let layout = ArticleLayout::parse_framed(
            &packed,
            crate::protocol::StatusCode::new(222),
            state::StatusLineEnd::new(status_line_end),
            state::ContentEnd::new(frame.len()),
        )
        .unwrap();

        assert_eq!(
            layout.content,
            ArticleContent::Body {
                body: status_line_end..frame.len(),
            }
        );
    }

    #[test]
    fn test_decode_yenc_body() {
        // Valid yenc example from the validation tests
        let buf = b"222 100 <test@example.com> body\r\n\
                    =ybegin line=128 size=12 name=test.txt\r\n\
                    r\x8f\x96\x96\x99VJ\xa3o\x98\x8dK\r\n\
                    =yend size=12 crc32=0337ab3d\r\n";

        let article = Article::parse(&buf[..], true).unwrap();
        let decoded = article.decode();

        assert!(decoded.is_some());
        let data = decoded.unwrap();
        // The decoded data should be the yenc-decoded version
        assert!(!data.is_empty());
    }

    #[test]
    fn test_decode_non_yenc_returns_none() {
        let buf = b"222 100 <test@example.com> body\r\n\
                    This is plain text, not yenc\r\n";

        let article = Article::parse(&buf[..], false).unwrap();
        let decoded = article.decode();

        assert!(decoded.is_none());
    }

    #[test]
    fn yenc_policy_is_separate_from_article_validation() {
        let buf = b"222 100 <test@example.com> body\r\n\
=ybegin line=128 size=1 name=test.bin\r\n\
not-a-complete-yenc-body\r\n";

        assert!(Article::parse_with_yenc(buf, YencValidation::Disabled).is_ok());
        assert!(matches!(
            Article::parse_with_yenc(buf, YencValidation::Enabled),
            Err(ParseError::InvalidYenc(_))
        ));
    }

    #[test]
    fn test_decode_no_body_returns_none() {
        let buf = b"223 100 <test@example.com>\r\n";

        let article = Article::parse(&buf[..], false).unwrap();
        let decoded = article.decode();

        assert!(decoded.is_none());
    }

    #[test]
    fn compatibility_fixture_matrix_records_proxy_wire_contract() {
        let fixtures = [
            (
                b"220 0 <article@example.com>\r\nSubject: fixture\r\n\r\nbody\r\n".as_slice(),
                Some(0),
                true,
                true,
            ),
            (
                b"221 0 <head@example.com>\r\nSubject: fixture\r\nFrom: test@example.com\r\n"
                    .as_slice(),
                Some(0),
                true,
                false,
            ),
            (
                b"222 0 <body@example.com>\r\nbody\r\n".as_slice(),
                Some(0),
                false,
                true,
            ),
            (
                b"223 0 <stat@example.com>\r\n".as_slice(),
                Some(0),
                false,
                false,
            ),
            (
                b"222 0 <empty@example.com>\r\n".as_slice(),
                Some(0),
                false,
                true,
            ),
        ];

        for (wire, article_number, has_headers, has_body) in fixtures {
            let article = Article::parse(wire, false).expect("proxy fixture remains accepted");
            assert_eq!(article.article_number, article_number);
            assert_eq!(article.headers.is_some(), has_headers);
            assert_eq!(article.body.is_some(), has_body);
        }
    }

    #[test]
    fn compatibility_fixture_matrix_keeps_lenient_article_number_behavior() {
        for number in [b"not-a-number".as_slice(), b"18446744073709551616"] {
            let wire = [
                b"220 ".as_slice(),
                number,
                b" <fixture@example.com>\r\nSubject: fixture\r\n\r\nbody\r\n",
            ]
            .concat();

            let article = Article::parse(&wire, false).expect("proxy parser is permissive here");
            assert_eq!(article.article_number, None);
            assert_eq!(article.message_id.as_str(), "<fixture@example.com>");
        }
    }

    #[test]
    fn compatibility_fixture_matrix_preserves_proxy_wire_sections() {
        let folded = b"220 0 <folded@example.com>\r\nSubject: first\r\n second\r\n\r\nbody\r\n";
        let folded_article = Article::parse(folded, false).unwrap();
        assert_eq!(
            folded_article.headers.unwrap().get("Subject"),
            Some(&b"first"[..])
        );

        let stuffed = b"222 0 <stuffed@example.com>\r\n..wire-dot\r\n";
        let stuffed_article = Article::parse(stuffed, false).unwrap();
        assert_eq!(stuffed_article.body, Some(&b"..wire-dot\r\n"[..]));

        let binary = b"222 0 <binary@example.com>\r\nbinary\0body\r\n";
        assert!(Article::parse(binary, false).is_ok());

        let bare_lf = b"222 0 <bare@example.com>\r\nbody\nnext\r\n";
        assert!(Article::parse(bare_lf, false).is_ok());
    }
}
