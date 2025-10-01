//! NNTP response parsing and handling

/// Represents a parsed NNTP response
#[derive(Debug, Clone, PartialEq)]
pub struct NntpResponse {
    /// Status code (e.g., 200, 381, 500)
    pub status_code: u16,
    /// Whether this is a multiline response
    pub is_multiline: bool,
    /// Complete response data including status line
    pub data: Vec<u8>,
}

impl NntpResponse {
    /// Parse a status code from response data
    pub fn parse_status_code(data: &[u8]) -> Option<u16> {
        if data.len() < 3 {
            return None;
        }

        let code_str = std::str::from_utf8(&data[0..3]).ok()?;
        code_str.parse().ok()
    }

    /// Check if a response indicates a multiline response
    #[allow(dead_code)]
    pub fn is_multiline_response(status_code: u16) -> bool {
        // Multiline responses in NNTP typically have codes like:
        // 1xx informational (multiline)
        // 2xx success with data (some multiline: 215, 220, 221, 222, 224, 225, 230, 231, 282)
        // 4xx/5xx errors are single line
        match status_code {
            100..=199 => true, // Informational multiline
            215 | 220 | 221 | 222 | 224 | 225 | 230 | 231 | 282 => true, // Article/list data
            _ => false,
        }
    }

    /// Check if data contains the end-of-multiline marker
    #[allow(dead_code)]
    pub fn has_multiline_terminator(data: &[u8]) -> bool {
        // NNTP multiline responses end with "\r\n.\r\n"
        if data.len() < 5 {
            return false;
        }

        // Look for the terminator at the end
        data.ends_with(b"\r\n.\r\n") || data.ends_with(b"\n.\r\n")
    }
}

/// Response parser for NNTP protocol
pub struct ResponseParser;

impl ResponseParser {
    /// Check if a response starts with a success code
    #[allow(dead_code)]
    pub fn is_success_response(data: &[u8]) -> bool {
        if let Some(code) = NntpResponse::parse_status_code(data) {
            code >= 200 && code < 400
        } else {
            false
        }
    }

    /// Check if response is a greeting (200 or 201)
    pub fn is_greeting(data: &[u8]) -> bool {
        let response_str = String::from_utf8_lossy(data);
        response_str.starts_with("200") || response_str.starts_with("201")
    }

    /// Check if response indicates authentication is required
    pub fn is_auth_required(data: &[u8]) -> bool {
        if let Some(code) = NntpResponse::parse_status_code(data) {
            code == 381 || code == 480
        } else {
            false
        }
    }

    /// Check if response indicates successful authentication
    pub fn is_auth_success(data: &[u8]) -> bool {
        if let Some(code) = NntpResponse::parse_status_code(data) {
            code == 281
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_status_code() {
        assert_eq!(NntpResponse::parse_status_code(b"200 Ready\r\n"), Some(200));
        assert_eq!(
            NntpResponse::parse_status_code(b"381 Password required\r\n"),
            Some(381)
        );
        assert_eq!(NntpResponse::parse_status_code(b"500 Error\r\n"), Some(500));
        assert_eq!(NntpResponse::parse_status_code(b"XX"), None);
        assert_eq!(NntpResponse::parse_status_code(b""), None);
    }

    #[test]
    fn test_is_multiline_response() {
        assert!(NntpResponse::is_multiline_response(100));
        assert!(NntpResponse::is_multiline_response(215)); // LIST
        assert!(NntpResponse::is_multiline_response(220)); // ARTICLE
        assert!(NntpResponse::is_multiline_response(221)); // HEAD
        assert!(NntpResponse::is_multiline_response(222)); // BODY
        assert!(!NntpResponse::is_multiline_response(200)); // Greeting
        assert!(!NntpResponse::is_multiline_response(381)); // Auth required
        assert!(!NntpResponse::is_multiline_response(500)); // Error
    }

    #[test]
    fn test_has_multiline_terminator() {
        assert!(NntpResponse::has_multiline_terminator(
            b"data\r\nmore data\r\n.\r\n"
        ));
        assert!(NntpResponse::has_multiline_terminator(
            b"single line\n.\r\n"
        ));
        assert!(!NntpResponse::has_multiline_terminator(b"incomplete\r\n"));
        assert!(!NntpResponse::has_multiline_terminator(b".\r\n")); // Too short
    }

    #[test]
    fn test_is_success_response() {
        assert!(ResponseParser::is_success_response(b"200 Ready\r\n"));
        assert!(ResponseParser::is_success_response(b"281 Auth OK\r\n"));
        assert!(!ResponseParser::is_success_response(b"400 Error\r\n"));
        assert!(!ResponseParser::is_success_response(b"500 Error\r\n"));
    }

    #[test]
    fn test_is_greeting() {
        assert!(ResponseParser::is_greeting(
            b"200 news.example.com ready\r\n"
        ));
        assert!(ResponseParser::is_greeting(b"201 read-only\r\n"));
        assert!(!ResponseParser::is_greeting(b"381 Password required\r\n"));
    }

    #[test]
    fn test_auth_responses() {
        assert!(ResponseParser::is_auth_required(
            b"381 Password required\r\n"
        ));
        assert!(ResponseParser::is_auth_required(b"480 Auth required\r\n"));
        assert!(!ResponseParser::is_auth_required(b"200 Ready\r\n"));

        assert!(ResponseParser::is_auth_success(b"281 Auth accepted\r\n"));
        assert!(!ResponseParser::is_auth_success(
            b"381 Password required\r\n"
        ));
    }

    #[test]
    fn test_malformed_status_codes() {
        // Non-numeric status
        assert_eq!(NntpResponse::parse_status_code(b"ABC Invalid\r\n"), None);

        // Missing status code
        assert_eq!(NntpResponse::parse_status_code(b"Missing code\r\n"), None);

        // Incomplete status code
        assert_eq!(NntpResponse::parse_status_code(b"20"), None);
        assert_eq!(NntpResponse::parse_status_code(b"2"), None);

        // Status code with invalid characters
        assert_eq!(NntpResponse::parse_status_code(b"2X0 Error\r\n"), None);

        // Negative status (shouldn't parse)
        assert_eq!(NntpResponse::parse_status_code(b"-200 Invalid\r\n"), None);
    }

    #[test]
    fn test_incomplete_responses() {
        // Empty response
        assert_eq!(NntpResponse::parse_status_code(b""), None);

        // Only newline
        assert_eq!(NntpResponse::parse_status_code(b"\r\n"), None);

        // Status without message
        assert_eq!(NntpResponse::parse_status_code(b"200"), Some(200));

        // Status with just space
        assert_eq!(NntpResponse::parse_status_code(b"200 "), Some(200));
    }

    #[test]
    fn test_boundary_status_codes() {
        // Minimum valid code
        assert_eq!(NntpResponse::parse_status_code(b"100 Info\r\n"), Some(100));

        // Maximum valid code
        assert_eq!(NntpResponse::parse_status_code(b"599 Error\r\n"), Some(599));

        // Out of range codes (but still parse)
        assert_eq!(NntpResponse::parse_status_code(b"000 Zero\r\n"), Some(0));
        assert_eq!(NntpResponse::parse_status_code(b"999 Max\r\n"), Some(999));

        // Four digit code (only first 3 parsed)
        assert_eq!(
            NntpResponse::parse_status_code(b"1234 Invalid\r\n"),
            Some(123)
        );
    }

    #[test]
    fn test_greeting_variations() {
        // Standard greeting
        assert!(ResponseParser::is_greeting(
            b"200 news.example.com ready\r\n"
        ));

        // Read-only greeting
        assert!(ResponseParser::is_greeting(b"201 read-only access\r\n"));

        // Minimal greeting
        assert!(ResponseParser::is_greeting(b"200\r\n"));

        // Greeting without CRLF
        assert!(ResponseParser::is_greeting(b"200 Ready"));

        // Not a greeting (4xx/5xx codes)
        assert!(!ResponseParser::is_greeting(b"400 Service unavailable\r\n"));
        assert!(!ResponseParser::is_greeting(b"502 Service unavailable\r\n"));

        // Auth-related but not greeting
        assert!(!ResponseParser::is_greeting(b"281 Auth accepted\r\n"));
        assert!(!ResponseParser::is_greeting(b"381 Password required\r\n"));
    }

    #[test]
    fn test_auth_required_edge_cases() {
        // Standard auth required
        assert!(ResponseParser::is_auth_required(
            b"381 Password required\r\n"
        ));
        assert!(ResponseParser::is_auth_required(
            b"480 Authentication required\r\n"
        ));

        // Without message
        assert!(ResponseParser::is_auth_required(b"381\r\n"));
        assert!(ResponseParser::is_auth_required(b"480\r\n"));

        // Without CRLF
        assert!(ResponseParser::is_auth_required(b"381 Password required"));

        // Not auth required
        assert!(!ResponseParser::is_auth_required(b"200 Ready\r\n"));
        assert!(!ResponseParser::is_auth_required(b"281 Auth accepted\r\n"));
        assert!(!ResponseParser::is_auth_required(b"482 Auth rejected\r\n"));
    }

    #[test]
    fn test_auth_success_edge_cases() {
        // Standard success
        assert!(ResponseParser::is_auth_success(
            b"281 Authentication accepted\r\n"
        ));

        // Minimal success
        assert!(ResponseParser::is_auth_success(b"281\r\n"));

        // Without CRLF
        assert!(ResponseParser::is_auth_success(b"281 OK"));

        // Not success (other 2xx codes)
        assert!(!ResponseParser::is_auth_success(b"200 Ready\r\n"));
        assert!(!ResponseParser::is_auth_success(b"211 Group selected\r\n"));

        // Failures
        assert!(!ResponseParser::is_auth_success(
            b"381 Password required\r\n"
        ));
        assert!(!ResponseParser::is_auth_success(
            b"481 Authentication failed\r\n"
        ));
        assert!(!ResponseParser::is_auth_success(
            b"482 Authentication rejected\r\n"
        ));
    }

    #[test]
    fn test_multiline_terminator_variations() {
        // Standard terminator
        assert!(NntpResponse::has_multiline_terminator(
            b"line1\r\nline2\r\n.\r\n"
        ));

        // Terminator with just LF
        assert!(NntpResponse::has_multiline_terminator(b"data\n.\r\n"));

        // No terminator
        assert!(!NntpResponse::has_multiline_terminator(
            b"line1\r\nline2\r\n"
        ));

        // Terminator in middle (should not match - ends_with check)
        assert!(!NntpResponse::has_multiline_terminator(b".\r\nmore data"));

        // Just the terminator (exactly 5 bytes)
        assert!(NntpResponse::has_multiline_terminator(b"\r\n.\r\n"));
        // This is only 4 bytes, so should be false
        assert!(!NntpResponse::has_multiline_terminator(b"\n.\r\n"));

        // Too short
        assert!(!NntpResponse::has_multiline_terminator(b".\r\n"));
        assert!(!NntpResponse::has_multiline_terminator(b"abc"));
        assert!(!NntpResponse::has_multiline_terminator(b""));
    }

    #[test]
    fn test_success_response_edge_cases() {
        // 2xx codes (success)
        assert!(ResponseParser::is_success_response(b"200 OK\r\n"));
        assert!(ResponseParser::is_success_response(
            b"211 Group selected\r\n"
        ));
        assert!(ResponseParser::is_success_response(
            b"220 Article follows\r\n"
        ));
        assert!(ResponseParser::is_success_response(b"281 Auth OK\r\n"));

        // 1xx codes (informational - not success)
        assert!(!ResponseParser::is_success_response(b"100 Info\r\n"));

        // 3xx codes (further action - still considered success in NNTP context)
        assert!(ResponseParser::is_success_response(
            b"381 Password required\r\n"
        ));

        // 4xx codes (client error)
        assert!(!ResponseParser::is_success_response(b"400 Bad request\r\n"));
        assert!(!ResponseParser::is_success_response(
            b"430 No such article\r\n"
        ));

        // 5xx codes (server error)
        assert!(!ResponseParser::is_success_response(
            b"500 Internal error\r\n"
        ));
        assert!(!ResponseParser::is_success_response(
            b"502 Service unavailable\r\n"
        ));
    }

    #[test]
    fn test_multiline_response_categories() {
        // 1xx informational (all multiline)
        assert!(NntpResponse::is_multiline_response(100));
        assert!(NntpResponse::is_multiline_response(150));
        assert!(NntpResponse::is_multiline_response(199));

        // 2xx specific multiline codes
        assert!(NntpResponse::is_multiline_response(215)); // LIST
        assert!(NntpResponse::is_multiline_response(220)); // ARTICLE
        assert!(NntpResponse::is_multiline_response(221)); // HEAD
        assert!(NntpResponse::is_multiline_response(222)); // BODY
        assert!(NntpResponse::is_multiline_response(224)); // OVER
        assert!(NntpResponse::is_multiline_response(225)); // HDR
        assert!(NntpResponse::is_multiline_response(230)); // NEWNEWS
        assert!(NntpResponse::is_multiline_response(231)); // NEWGROUPS

        // 2xx non-multiline codes
        assert!(!NntpResponse::is_multiline_response(200)); // Greeting
        assert!(!NntpResponse::is_multiline_response(205)); // Goodbye
        assert!(!NntpResponse::is_multiline_response(211)); // Group selected
        assert!(!NntpResponse::is_multiline_response(281)); // Auth accepted

        // 3xx, 4xx, 5xx are not multiline
        assert!(!NntpResponse::is_multiline_response(381));
        assert!(!NntpResponse::is_multiline_response(430));
        assert!(!NntpResponse::is_multiline_response(500));
    }

    #[test]
    fn test_utf8_in_responses() {
        // Response with UTF-8 characters in message
        let utf8_response = "200 Привет мир\r\n".as_bytes();
        assert_eq!(NntpResponse::parse_status_code(utf8_response), Some(200));
        assert!(ResponseParser::is_greeting(utf8_response));

        // Auth response with UTF-8
        let utf8_auth = "281 认证成功\r\n".as_bytes();
        assert_eq!(NntpResponse::parse_status_code(utf8_auth), Some(281));
        assert!(ResponseParser::is_auth_success(utf8_auth));
    }

    #[test]
    fn test_response_with_binary_data() {
        // Response with null bytes in message part (after status code)
        let with_null = b"200 Test\x00Message\r\n";
        assert_eq!(NntpResponse::parse_status_code(with_null), Some(200));

        // Response with high bytes
        let with_high_bytes = b"200 \xFF\xFE\r\n";
        assert_eq!(NntpResponse::parse_status_code(with_high_bytes), Some(200));
    }
}
