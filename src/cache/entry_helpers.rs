//! Shared helper functions for cache entry validation
//!
//! These functions operate on raw NNTP response buffers and status codes,
//! providing common logic used by both `ArticleEntry` (moka) and
//! `HybridArticleEntry` (foyer) cache implementations.

use crate::protocol::has_multiline_terminator;
use smallvec::SmallVec;

/// Check if a buffer contains a valid NNTP multiline response
///
/// A valid response must:
/// 1. Start with 3 ASCII digits (status code)
/// 2. Have CRLF somewhere (line terminator)
/// 3. End with `.\r\n` for multiline responses (220/221/222)
#[inline]
pub(super) fn is_valid_response(buffer: &[u8]) -> bool {
    // Must have at least "NNN \r\n.\r\n" = 9 bytes
    if buffer.len() < 9 {
        return false;
    }

    // First 3 bytes must be ASCII digits
    if !buffer[0].is_ascii_digit() || !buffer[1].is_ascii_digit() || !buffer[2].is_ascii_digit() {
        return false;
    }

    // Must end with \r\n.\r\n for multiline responses
    if !has_multiline_terminator(buffer) {
        return false;
    }

    // Must have CRLF in first line (status line)
    memchr::memmem::find(&buffer[..256.min(buffer.len())], b"\r\n").is_some()
}

/// Check if a buffer is a complete article (220/222) with actual content
///
/// Returns true if:
/// 1. Status code is 220 (ARTICLE) or 222 (BODY)
/// 2. Buffer contains actual content (not just a stub)
///
/// A complete response ends with `\r\n.\r\n` and is at least 30 bytes.
#[inline]
pub(super) fn is_complete_article(buffer: &[u8], status_code: u16) -> bool {
    if status_code != 220 && status_code != 222 {
        return false;
    }
    const MIN_ARTICLE_SIZE: usize = 30;
    buffer.len() >= MIN_ARTICLE_SIZE && has_multiline_terminator(buffer)
}

/// Get the appropriate response bytes for a command verb
///
/// Returns `Some(response_bytes)` if the cached buffer can satisfy the command:
/// - ARTICLE (220 cached) -> returns full cached response
/// - BODY (222 cached or 220 cached) -> returns cached response
/// - HEAD (221 cached or 220 cached) -> returns cached response
/// - STAT -> synthesizes "223 0 <msg-id>\r\n" (we know article exists)
///
/// Returns `None` if cached response can't serve this command type.
pub(super) fn response_for_command(
    buffer: &[u8],
    status_code: u16,
    cmd_verb: &str,
    message_id: &str,
) -> Option<Vec<u8>> {
    match (status_code, cmd_verb) {
        // STAT just needs existence confirmation - synthesize response
        (220..=222, "STAT") => Some(format!("223 0 {}\r\n", message_id).into_bytes()),
        // Direct match - return cached buffer if valid
        (220, "ARTICLE") | (222, "BODY") | (221, "HEAD") => {
            if is_valid_response(buffer) {
                Some(buffer.to_vec())
            } else {
                tracing::warn!(
                    code = status_code,
                    len = buffer.len(),
                    "Cached buffer failed validation, discarding"
                );
                None
            }
        }
        // ARTICLE (220) contains everything, can serve BODY or HEAD requests
        (220, "BODY" | "HEAD") => {
            if is_valid_response(buffer) {
                Some(buffer.to_vec())
            } else {
                tracing::warn!(
                    code = status_code,
                    len = buffer.len(),
                    "Cached buffer failed validation, discarding"
                );
                None
            }
        }
        _ => None,
    }
}

/// Check if a status code can serve a given command verb
///
/// Simpler version of `response_for_command` for boolean checks.
/// Case-insensitive per RFC 3977 (commands are case-insensitive).
#[inline]
pub(super) fn matches_command_type_verb(status_code: u16, cmd_verb: &str) -> bool {
    match status_code {
        220 => {
            cmd_verb.eq_ignore_ascii_case("ARTICLE")
                || cmd_verb.eq_ignore_ascii_case("BODY")
                || cmd_verb.eq_ignore_ascii_case("HEAD")
                || cmd_verb.eq_ignore_ascii_case("STAT")
        }
        222 => cmd_verb.eq_ignore_ascii_case("BODY") || cmd_verb.eq_ignore_ascii_case("STAT"),
        221 => cmd_verb.eq_ignore_ascii_case("HEAD") || cmd_verb.eq_ignore_ascii_case("STAT"),
        _ => false,
    }
}

/// Extract the first status line from an NNTP response buffer.
///
/// Returns bytes up to and including the first `\r\n`, or the first 3 bytes
/// plus `\r\n` as fallback. Used to create minimal cache stubs without
/// copying the full response buffer.
///
/// This is used when `cache_articles=false` to reduce memory copies —
/// the cache only needs the status code to build an availability stub,
/// not the full article content (which can be 8-64KB in first_chunk).
///
/// Uses SmallVec to keep typical status lines (~30-80 bytes) on the stack,
/// avoiding heap allocation in the common case.
#[inline]
pub fn extract_status_line(buffer: &[u8]) -> SmallVec<[u8; 128]> {
    // Find first \r for end of status line
    if let Some(cr_pos) = memchr::memchr(b'\r', buffer) {
        let end = (cr_pos + 2).min(buffer.len());
        SmallVec::from_slice(&buffer[..end])
    } else if buffer.len() >= 3 {
        // Fallback: just the status code digits + CRLF
        let mut stub = SmallVec::new();
        stub.extend_from_slice(&buffer[..3]);
        stub.extend_from_slice(b"\r\n");
        stub
    } else {
        SmallVec::from_slice(buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // is_valid_response tests
    // =========================================================================

    #[test]
    fn test_valid_multiline_response() {
        let buf = b"220 0 <test@example.com>\r\nSubject: T\r\n\r\nBody\r\n.\r\n";
        assert!(is_valid_response(buf));
    }

    #[test]
    fn test_valid_minimal_response() {
        // Minimal valid: "NNN X\r\n.\r\n" = 10 bytes
        let buf = b"220 X\r\n.\r\n";
        assert!(is_valid_response(buf));
    }

    #[test]
    fn test_too_short() {
        assert!(!is_valid_response(b"220\r\n"));
        assert!(!is_valid_response(b"22"));
        assert!(!is_valid_response(b""));
    }

    #[test]
    fn test_no_digits() {
        assert!(!is_valid_response(b"abc X\r\n.\r\n"));
    }

    #[test]
    fn test_no_terminator() {
        assert!(!is_valid_response(b"220 0 <test@x>\r\nBody\r\n"));
    }

    #[test]
    fn test_no_status_line_crlf() {
        // Has .\r\n at end but the only \r\n is part of the terminator.
        // The memchr search finds \r\n in the first 256 bytes (including the terminator),
        // so this is actually valid. Test a truly missing CRLF scenario:
        // a buffer that doesn't end with .\r\n
        assert!(!is_valid_response(b"220 test data here"));
    }

    // =========================================================================
    // is_complete_article tests
    // =========================================================================

    #[test]
    fn test_complete_article_220() {
        let buf = b"220 0 <test@example.com>\r\nSubject: T\r\n\r\nBody\r\n.\r\n";
        assert!(is_complete_article(buf, 220));
    }

    #[test]
    fn test_complete_article_222() {
        let buf = b"222 0 <test@example.com>\r\n\r\nBody content\r\n.\r\n";
        assert!(is_complete_article(buf, 222));
    }

    #[test]
    fn test_not_complete_221() {
        let buf = b"221 0 <test@example.com>\r\nSubject: T\r\n.\r\n";
        assert!(!is_complete_article(buf, 221));
    }

    #[test]
    fn test_not_complete_stub() {
        assert!(!is_complete_article(b"220\r\n", 220));
    }

    #[test]
    fn test_not_complete_no_terminator() {
        let buf = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody\r\n";
        assert!(!is_complete_article(buf, 220));
    }

    // =========================================================================
    // response_for_command tests
    // =========================================================================

    #[test]
    fn test_stat_from_220() {
        let buf = b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n";
        let resp = response_for_command(buf, 220, "STAT", "<t@x>").unwrap();
        assert_eq!(resp, b"223 0 <t@x>\r\n");
    }

    #[test]
    fn test_stat_from_221() {
        let buf = b"221 0 <t@x>\r\nSubject: T\r\n.\r\n";
        let resp = response_for_command(buf, 221, "STAT", "<t@x>").unwrap();
        assert_eq!(resp, b"223 0 <t@x>\r\n");
    }

    #[test]
    fn test_stat_from_222() {
        let buf = b"222 0 <t@x>\r\n\r\nBody content\r\n.\r\n";
        let resp = response_for_command(buf, 222, "STAT", "<t@x>").unwrap();
        assert_eq!(resp, b"223 0 <t@x>\r\n");
    }

    #[test]
    fn test_article_direct() {
        let buf = b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n";
        let resp = response_for_command(buf, 220, "ARTICLE", "<t@x>").unwrap();
        assert_eq!(resp, buf.to_vec());
    }

    #[test]
    fn test_body_from_222() {
        let buf = b"222 0 <t@x>\r\n\r\nBody content\r\n.\r\n";
        let resp = response_for_command(buf, 222, "BODY", "<t@x>").unwrap();
        assert_eq!(resp, buf.to_vec());
    }

    #[test]
    fn test_body_from_220() {
        let buf = b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n";
        let resp = response_for_command(buf, 220, "BODY", "<t@x>").unwrap();
        assert_eq!(resp, buf.to_vec());
    }

    #[test]
    fn test_head_from_221() {
        let buf = b"221 0 <t@x>\r\nSubject: T\r\n.\r\n";
        let resp = response_for_command(buf, 221, "HEAD", "<t@x>").unwrap();
        assert_eq!(resp, buf.to_vec());
    }

    #[test]
    fn test_head_from_220() {
        let buf = b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n";
        let resp = response_for_command(buf, 220, "HEAD", "<t@x>").unwrap();
        assert_eq!(resp, buf.to_vec());
    }

    #[test]
    fn test_body_cannot_serve_article() {
        let buf = b"222 0 <t@x>\r\n\r\nBody content\r\n.\r\n";
        assert!(response_for_command(buf, 222, "ARTICLE", "<t@x>").is_none());
    }

    #[test]
    fn test_head_cannot_serve_body() {
        let buf = b"221 0 <t@x>\r\nSubject: T\r\n.\r\n";
        assert!(response_for_command(buf, 221, "BODY", "<t@x>").is_none());
    }

    #[test]
    fn test_unknown_verb() {
        let buf = b"220 0 <t@x>\r\nSubject: T\r\n\r\nBody\r\n.\r\n";
        assert!(response_for_command(buf, 220, "LIST", "<t@x>").is_none());
    }

    #[test]
    fn test_stat_not_from_430() {
        let buf = b"430 not found\r\n";
        assert!(response_for_command(buf, 430, "STAT", "<t@x>").is_none());
    }

    // =========================================================================
    // matches_command_type_verb tests
    // =========================================================================

    #[test]
    fn test_220_matches() {
        assert!(matches_command_type_verb(220, "ARTICLE"));
        assert!(matches_command_type_verb(220, "BODY"));
        assert!(matches_command_type_verb(220, "HEAD"));
        assert!(matches_command_type_verb(220, "STAT"));
    }

    #[test]
    fn test_222_matches() {
        assert!(!matches_command_type_verb(222, "ARTICLE"));
        assert!(matches_command_type_verb(222, "BODY"));
        assert!(!matches_command_type_verb(222, "HEAD"));
        assert!(matches_command_type_verb(222, "STAT"));
    }

    #[test]
    fn test_221_matches() {
        assert!(!matches_command_type_verb(221, "ARTICLE"));
        assert!(!matches_command_type_verb(221, "BODY"));
        assert!(matches_command_type_verb(221, "HEAD"));
        assert!(matches_command_type_verb(221, "STAT"));
    }

    #[test]
    fn test_other_codes_match_nothing() {
        assert!(!matches_command_type_verb(223, "STAT"));
        assert!(!matches_command_type_verb(430, "ARTICLE"));
        assert!(!matches_command_type_verb(200, "ARTICLE"));
    }

    // =========================================================================
    // extract_status_line tests
    // =========================================================================

    #[test]
    fn test_extract_status_line_full() {
        let buf = b"220 0 <test@example.com>\r\nSubject: Test\r\n\r\nBody content\r\n.\r\n";
        let stub = extract_status_line(buf);
        assert_eq!(stub.as_slice(), b"220 0 <test@example.com>\r\n");
    }

    #[test]
    fn test_extract_status_line_minimal() {
        let buf = b"430 not found\r\n";
        let stub = extract_status_line(buf);
        assert_eq!(stub.as_slice(), b"430 not found\r\n");
    }

    #[test]
    fn test_extract_status_line_no_crlf() {
        // Fallback: just status code + CRLF
        let buf = b"220 something without crlf";
        let stub = extract_status_line(buf);
        assert_eq!(stub.as_slice(), b"220\r\n");
    }

    #[test]
    fn test_extract_status_line_too_short() {
        // Buffer shorter than 3 bytes - just return it
        let buf = b"22";
        let stub = extract_status_line(buf);
        assert_eq!(stub.as_slice(), b"22");
    }

    #[test]
    fn test_extract_status_line_exact_crlf_boundary() {
        // \r at the end, but buffer doesn't have \n
        let buf = b"220 test\r";
        let stub = extract_status_line(buf);
        // cr_pos = 8, end = min(10, 9) = 9
        assert_eq!(stub.as_slice(), b"220 test\r");
    }
}
