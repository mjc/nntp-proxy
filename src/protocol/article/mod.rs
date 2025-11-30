//! Article parsing and validation
//!
//! Provides zero-copy parsing of NNTP article responses (ARTICLE, HEAD, BODY, STAT)
//! with comprehensive validation of structure and format.

mod error;
mod headers;
mod yenc;

pub use error::ParseError;
pub use headers::{HeaderIter, Headers};

use crate::types::protocol::MessageId;
use yenc::validate_yenc_structure;

/// Parsed NNTP article response (zero-copy)
///
/// Different response codes populate different fields:
/// - 220 ARTICLE: headers + body
/// - 221 HEAD: headers only
/// - 222 BODY: body only
/// - 223 STAT: neither (just metadata)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Article<'a> {
    pub message_id: MessageId<'a>,
    pub article_number: Option<u64>,
    pub headers: Option<Headers<'a>>,
    pub body: Option<&'a [u8]>,
}

impl<'a> TryFrom<&'a [u8]> for Article<'a> {
    type Error = ParseError;

    fn try_from(buf: &'a [u8]) -> Result<Self, Self::Error> {
        // Parse status code from first line
        let status_code = parse_status_code(buf)?;

        // Dispatch to appropriate parser
        match status_code {
            220 => Self::parse_article(buf),
            221 => Self::parse_head(buf),
            222 => Self::parse_body(buf),
            223 => Self::parse_stat(buf),
            _ => Err(ParseError::InvalidStatusCode(status_code)),
        }
    }
}

impl<'a> Article<'a> {
    /// Parse 220 ARTICLE response (headers + body)
    fn parse_article(buf: &'a [u8]) -> Result<Self, ParseError> {
        // 220 <article-number> <message-id> ...
        let first_line_end = find_line_end(buf, 0)?;
        let first_line = &buf[..first_line_end];

        let (message_id, article_number) = parse_first_line(first_line)?;

        // Find blank line separator
        let content_start = first_line_end + 2;
        let separator_pos = find_blank_line(buf, content_start)?;

        // Headers are between content_start and separator
        let headers_data = &buf[content_start..separator_pos];
        let headers = Some(Headers::parse(headers_data)?);

        // Body starts after blank line (\r\n\r\n is 4 bytes)
        let body_start = separator_pos + 4;

        // Find terminator
        let terminator_start = find_terminator(buf, body_start)?;
        let body_data = &buf[body_start..terminator_start];

        // Validate yenc if present
        if body_data.starts_with(b"=ybegin") {
            validate_yenc_structure(body_data)?;
        }

        let body = Some(body_data);

        Ok(Article {
            message_id,
            article_number,
            headers,
            body,
        })
    }

    /// Parse 221 HEAD response (headers only)
    fn parse_head(buf: &'a [u8]) -> Result<Self, ParseError> {
        // 221 <article-number> <message-id> ...
        let first_line_end = find_line_end(buf, 0)?;
        let first_line = &buf[..first_line_end];

        let (message_id, article_number) = parse_first_line(first_line)?;

        // Headers start after first line
        let content_start = first_line_end + 2;

        // HEAD must NOT have a blank line separator (that would indicate body)
        // Check for \r\n\r\n before the terminator
        if find_blank_line(buf, content_start).is_ok() {
            return Err(ParseError::UnexpectedBody);
        }

        // Find terminator (headers end at terminator)
        let terminator_start = find_terminator(buf, content_start)?;

        // Headers are between content_start and terminator
        let headers_data = &buf[content_start..terminator_start];
        let headers = Some(Headers::parse(headers_data)?);

        Ok(Article {
            message_id,
            article_number,
            headers,
            body: None,
        })
    }

    /// Parse 222 BODY response (body only)
    fn parse_body(buf: &'a [u8]) -> Result<Self, ParseError> {
        // 222 <article-number> <message-id> ...
        let first_line_end = find_line_end(buf, 0)?;
        let first_line = &buf[..first_line_end];

        let (message_id, article_number) = parse_first_line(first_line)?;

        // Body starts immediately after first line (no headers)
        let body_start = first_line_end + 2;

        // Find terminator
        let terminator_start = find_terminator(buf, body_start)?;
        let body_data = &buf[body_start..terminator_start];

        // Validate yenc if present
        if body_data.starts_with(b"=ybegin") {
            validate_yenc_structure(body_data)?;
        }

        let body = Some(body_data);

        Ok(Article {
            message_id,
            article_number,
            headers: None,
            body,
        })
    }

    /// Parse 223 STAT response (metadata only)
    fn parse_stat(buf: &'a [u8]) -> Result<Self, ParseError> {
        // 223 <article-number> <message-id> ...
        let first_line_end = find_line_end(buf, 0)?;
        let first_line = &buf[..first_line_end];

        let (message_id, article_number) = parse_first_line(first_line)?;

        // STAT has no content - verify terminator immediately follows
        let content_start = first_line_end + 2;

        // For STAT, there might not be a terminator, or it's at the end
        // Check if we have more data
        if content_start < buf.len() {
            // Verify it's the terminator
            if !buf[content_start..].starts_with(b".\r\n") {
                return Err(ParseError::UnexpectedBody);
            }
        }

        Ok(Article {
            message_id,
            article_number,
            headers: None,
            body: None,
        })
    }
}

/// Parse status code from buffer
fn parse_status_code(buf: &[u8]) -> Result<u16, ParseError> {
    if buf.len() < 3 {
        return Err(ParseError::BufferTooShort);
    }

    // Status code is first 3 bytes
    let code_bytes = &buf[..3];

    // Parse as ASCII digits
    let code = std::str::from_utf8(code_bytes)
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or(ParseError::InvalidStatusCode(0))?;

    Ok(code)
}

/// Parse first line to extract message-id and optional article number
fn parse_first_line(line: &[u8]) -> Result<(MessageId<'_>, Option<u64>), ParseError> {
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
    let message_id = MessageId::from_borrowed(msg_id_str)?;

    Ok((message_id, article_number))
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

/// Find multiline terminator (.\r\n)
fn find_terminator(buf: &[u8], start: usize) -> Result<usize, ParseError> {
    // Look for \r\n.\r\n pattern
    for i in start..buf.len().saturating_sub(4) {
        if &buf[i..i + 5] == b"\r\n.\r\n" {
            return Ok(i + 2); // Return position after \r\n (start of .)
        }
    }

    // Also check if it starts with .\r\n (empty body)
    if buf[start..].starts_with(b".\r\n") {
        return Ok(start);
    }

    Err(ParseError::MissingTerminator)
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
    fn test_find_terminator() {
        let buf = b"Body content\r\n.\r\n";
        let pos = find_terminator(buf, 0).unwrap();
        assert_eq!(&buf[pos..pos + 3], b".\r\n");
    }

    #[test]
    fn test_parse_article_220() {
        let buf = b"220 100 <test@example.com> article\r\n\
                    Subject: Test\r\n\
                    \r\n\
                    Body content\r\n\
                    .\r\n";

        let article = Article::try_from(&buf[..]).unwrap();
        assert_eq!(article.message_id.as_str(), "<test@example.com>");
        assert_eq!(article.article_number, Some(100));
        assert!(article.headers.is_some());
        assert!(article.body.is_some());
    }
}
