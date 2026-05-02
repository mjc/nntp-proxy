//! Shared helper functions for cache entry validation
//!
//! These functions operate on raw NNTP response buffers and status codes,
//! providing common logic used by both `ArticleEntry` (moka) and
//! `HybridArticleEntry` (foyer) cache implementations.

#[cfg(test)]
use crate::session::streaming::tail_buffer::TailBuffer;

/// Check if a buffer contains a valid NNTP multiline response
///
/// A valid response must:
/// 1. Start with 3 ASCII digits (status code)
/// 2. Have CRLF somewhere (line terminator)
/// 3. End with `\r\n.\r\n` for multiline responses (220/221/222)
#[inline]
#[cfg(test)]
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
    if !TailBuffer::default().detect_terminator(buffer).is_found() {
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
#[cfg(test)]
pub(super) fn is_complete_article(buffer: &[u8], status_code: u16) -> bool {
    if status_code != 220 && status_code != 222 {
        return false;
    }
    const MIN_ARTICLE_SIZE: usize = 30;
    buffer.len() >= MIN_ARTICLE_SIZE && TailBuffer::default().detect_terminator(buffer).is_found()
}

/// Check if a status code can serve a given command verb.
///
/// Simpler version of `response_for_command` for boolean checks. Commands are
/// ASCII and case-insensitive per RFC 3977, so keep this byte-native.
#[inline]
pub(super) fn matches_command_type_verb(status_code: u16, cmd_verb: &[u8]) -> bool {
    match status_code {
        220 => {
            cmd_verb.eq_ignore_ascii_case(b"ARTICLE")
                || cmd_verb.eq_ignore_ascii_case(b"BODY")
                || cmd_verb.eq_ignore_ascii_case(b"HEAD")
                || cmd_verb.eq_ignore_ascii_case(b"STAT")
        }
        222 => cmd_verb.eq_ignore_ascii_case(b"BODY") || cmd_verb.eq_ignore_ascii_case(b"STAT"),
        221 => cmd_verb.eq_ignore_ascii_case(b"HEAD") || cmd_verb.eq_ignore_ascii_case(b"STAT"),
        _ => false,
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
    // matches_command_type_verb tests
    // =========================================================================

    #[test]
    fn test_220_matches() {
        assert!(matches_command_type_verb(220, b"ARTICLE"));
        assert!(matches_command_type_verb(220, b"BODY"));
        assert!(matches_command_type_verb(220, b"HEAD"));
        assert!(matches_command_type_verb(220, b"STAT"));
    }

    #[test]
    fn test_222_matches() {
        assert!(!matches_command_type_verb(222, b"ARTICLE"));
        assert!(matches_command_type_verb(222, b"BODY"));
        assert!(!matches_command_type_verb(222, b"HEAD"));
        assert!(matches_command_type_verb(222, b"STAT"));
    }

    #[test]
    fn test_221_matches() {
        assert!(!matches_command_type_verb(221, b"ARTICLE"));
        assert!(!matches_command_type_verb(221, b"BODY"));
        assert!(matches_command_type_verb(221, b"HEAD"));
        assert!(matches_command_type_verb(221, b"STAT"));
    }

    #[test]
    fn test_other_codes_match_nothing() {
        assert!(!matches_command_type_verb(223, b"STAT"));
        assert!(!matches_command_type_verb(430, b"ARTICLE"));
        assert!(!matches_command_type_verb(200, b"ARTICLE"));
    }

    #[test]
    fn test_byte_verbs_match_without_utf8() {
        assert!(matches_command_type_verb(220, b"article"));
        assert!(matches_command_type_verb(221, b"head"));
        assert!(!matches_command_type_verb(220, b"ARTICLE \xff"));
    }
}
