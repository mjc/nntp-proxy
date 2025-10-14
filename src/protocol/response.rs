//! NNTP response parsing and handling

/// Categorized NNTP response code for type-safe handling
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseCode {
    /// Server greeting (200 posting allowed, 201 no posting)
    Greeting(u16),
    /// Disconnect/goodbye (205)
    Disconnect,
    /// Authentication required (381, 480)
    AuthRequired(u16),
    /// Authentication successful (281)
    AuthSuccess,
    /// Multiline data response (1xx, specific 2xx codes)
    MultilineData(u16),
    /// Single-line response (everything else)
    SingleLine(u16),
    /// Invalid or unparseable response
    Invalid,
}

impl ResponseCode {
    /// Parse response data into a categorized response code
    #[inline]
    pub fn parse(data: &[u8]) -> Self {
        let code = match NntpResponse::parse_status_code(data) {
            Some(c) => c,
            None => return Self::Invalid,
        };

        match code {
            200 | 201 => Self::Greeting(code),
            205 => Self::Disconnect,
            281 => Self::AuthSuccess,
            381 | 480 => Self::AuthRequired(code),
            // Multiline: 1xx and specific 2xx codes
            100..=199 => Self::MultilineData(code),
            215 | 220 | 221 | 222 | 224 | 225 | 230 | 231 | 282 => Self::MultilineData(code),
            _ => Self::SingleLine(code),
        }
    }

    /// Check if this response type is multiline
    #[inline]
    pub fn is_multiline(&self) -> bool {
        matches!(self, Self::MultilineData(_))
    }

    /// Get the numeric status code if available
    #[inline]
    pub fn status_code(&self) -> Option<u16> {
        match self {
            Self::Greeting(c)
            | Self::AuthRequired(c)
            | Self::MultilineData(c)
            | Self::SingleLine(c) => Some(*c),
            Self::Disconnect => Some(205),
            Self::AuthSuccess => Some(281),
            Self::Invalid => None,
        }
    }

    /// Check if this is a success response (2xx or 3xx)
    #[inline]
    pub fn is_success(&self) -> bool {
        self.status_code()
            .is_some_and(|code| (200..400).contains(&code))
    }
}

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
    #[inline]
    pub fn parse_status_code(data: &[u8]) -> Option<u16> {
        if data.len() < 3 {
            return None;
        }

        let code_str = std::str::from_utf8(&data[0..3]).ok()?;
        code_str.parse().ok()
    }

    /// Check if a response indicates a multiline response
    ///
    /// Per RFC 3977 Section 3.2, multiline responses are specific codes
    #[inline]
    pub fn is_multiline_response(status_code: u16) -> bool {
        match status_code {
            100..=199 => true, // All 1xx are multiline
            215 | 220 | 221 | 222 | 224 | 225 | 230 | 231 | 282 => true, // Specific 2xx codes
            _ => false,
        }
    }

    /// Check if data ends with the NNTP multiline terminator
    ///
    /// NNTP multiline responses end with "\r\n.\r\n" (RFC 3977)
    /// Strictly checks for the proper terminator at the end of data.
    #[inline]
    pub fn has_terminator_at_end(data: &[u8]) -> bool {
        let n = data.len();
        // Only check for proper RFC 3977 terminator: \r\n.\r\n
        n >= 5 && data[n - 5..n] == *b"\r\n.\r\n"
    }

    /// Find the position of the NNTP multiline terminator in data
    ///
    /// Returns the position AFTER the terminator (exclusive end), or None if not found.
    /// This handles the case where extra data appears after the terminator in the same chunk.
    /// Strictly follows RFC 3977: only matches "\r\n.\r\n" (CRLF, dot, CRLF).
    #[inline]
    pub fn find_terminator_end(data: &[u8]) -> Option<usize> {
        let n = data.len();
        if n < 5 {
            return None;
        }

        // Search for "\r\n.\r\n" (5 bytes) - RFC 3977 standard terminator
        for i in 0..=(n - 5) {
            if &data[i..i + 5] == b"\r\n.\r\n" {
                return Some(i + 5);
            }
        }

        None
    }

    /// Check if a terminator spans across a boundary between tail and current chunk
    ///
    /// This handles the case where a multiline terminator is split across two read chunks.
    /// For example: previous chunk ends with "\r\n." and current starts with "\r\n"
    /// Strictly follows RFC 3977: only matches "\r\n.\r\n" (CRLF, dot, CRLF).
    #[inline]
    pub fn has_spanning_terminator(
        tail: &[u8],
        tail_len: usize,
        current: &[u8],
        current_len: usize,
    ) -> bool {
        // Only check if we have a tail and current chunk is small enough for spanning
        if tail_len < 2 || !(1..=4).contains(&current_len) {
            return false;
        }

        // Build combined view: tail + start of current chunk
        let mut check_buf = [0u8; 9]; // max: 4 tail + 5 current bytes
        check_buf[..tail_len].copy_from_slice(&tail[..tail_len]);
        let curr_copy = current_len.min(5);
        check_buf[tail_len..tail_len + curr_copy].copy_from_slice(&current[..curr_copy]);
        let total = tail_len + curr_copy;

        // RFC 3977 requires exactly \r\n.\r\n (5 bytes)
        total >= 5 && check_buf[total - 5..total] == *b"\r\n.\r\n"
    }

    /// Check if response is a disconnect/goodbye (205)
    #[inline]
    pub fn is_disconnect(data: &[u8]) -> bool {
        data.len() >= 3 && data.starts_with(b"205")
    }

    /// Extract message-ID from command arguments using fast byte searching
    ///
    /// Message-IDs are ASCII and must be in the format <...@...>
    /// See RFC 5536 Section 3.1.3: https://datatracker.ietf.org/doc/html/rfc5536#section-3.1.3
    #[inline]
    pub fn extract_message_id(command: &str) -> Option<String> {
        let trimmed = command.trim();
        let bytes = trimmed.as_bytes();

        // Find opening '<'
        let start = memchr::memchr(b'<', bytes)?;

        // Find closing '>' after the '<'
        // Since end is relative to &bytes[start..], the actual position is start + end
        let end = memchr::memchr(b'>', &bytes[start + 1..])?;
        let msgid_end = start + end + 2; // +1 for the slice offset, +1 to include '>'

        // Safety: Message-IDs are ASCII, so no need for is_char_boundary checks
        // We already know msgid_end is valid since memchr found '>' at that position
        Some(trimmed[start..msgid_end].to_string())
    }

    /// Validate message-ID format according to RFC 5536 Section 3.1.3
    ///
    /// A valid message-ID must:
    /// - Start with '<' and end with '>'
    /// - Contain exactly one '@' character
    /// - Have content before and after the '@'
    ///
    /// This is a basic validation - full RFC 5536 validation is more complex.
    #[inline]
    pub fn validate_message_id(msgid: &str) -> bool {
        let trimmed = msgid.trim();

        // Must start with < and end with >
        if !trimmed.starts_with('<') || !trimmed.ends_with('>') {
            return false;
        }

        // Extract content between < and >
        let content = &trimmed[1..trimmed.len() - 1];

        // Must contain exactly one @
        let at_count = content.bytes().filter(|&b| b == b'@').count();
        if at_count != 1 {
            return false;
        }

        // Must have content before and after @
        if let Some(at_pos) = content.find('@') {
            at_pos > 0 && at_pos < content.len() - 1
        } else {
            false
        }
    }

    /// Check if data contains the end-of-multiline marker (legacy method)
    #[inline]
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
    /// Check if a response starts with a success code (2xx or 3xx)
    #[allow(dead_code)]
    pub fn is_success_response(data: &[u8]) -> bool {
        ResponseCode::parse(data).is_success()
    }

    /// Check if response is a greeting (200 or 201)
    #[allow(dead_code)]
    pub fn is_greeting(data: &[u8]) -> bool {
        matches!(ResponseCode::parse(data), ResponseCode::Greeting(_))
    }

    /// Check if response indicates authentication is required (381 or 480)
    #[allow(dead_code)]
    pub fn is_auth_required(data: &[u8]) -> bool {
        matches!(ResponseCode::parse(data), ResponseCode::AuthRequired(_))
    }

    /// Check if response indicates successful authentication (281)
    #[allow(dead_code)]
    pub fn is_auth_success(data: &[u8]) -> bool {
        matches!(ResponseCode::parse(data), ResponseCode::AuthSuccess)
    }

    /// Check if response has a specific status code
    ///
    /// This is useful for checking specific response codes like 111 (DATE response),
    /// or any other specific code that doesn't have a dedicated helper.
    pub fn is_response_code(data: &[u8], code: u16) -> bool {
        NntpResponse::parse_status_code(data) == Some(code)
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
    fn test_is_response_code() {
        assert!(ResponseParser::is_response_code(
            b"111 20251010120000\r\n",
            111
        ));
        assert!(ResponseParser::is_response_code(b"200 Ready\r\n", 200));
        assert!(ResponseParser::is_response_code(b"201 Read-only\r\n", 201));
        assert!(ResponseParser::is_response_code(b"281 Auth OK\r\n", 281));
        assert!(ResponseParser::is_response_code(
            b"381 Password required\r\n",
            381
        ));
        assert!(!ResponseParser::is_response_code(b"200 Ready\r\n", 201));
        assert!(!ResponseParser::is_response_code(b"111 Date\r\n", 200));
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

    #[test]
    fn test_response_code_parse() {
        // Greetings
        assert_eq!(
            ResponseCode::parse(b"200 Ready\r\n"),
            ResponseCode::Greeting(200)
        );
        assert_eq!(
            ResponseCode::parse(b"201 No posting\r\n"),
            ResponseCode::Greeting(201)
        );

        // Disconnect
        assert_eq!(
            ResponseCode::parse(b"205 Goodbye\r\n"),
            ResponseCode::Disconnect
        );

        // Auth
        assert_eq!(
            ResponseCode::parse(b"381 Password required\r\n"),
            ResponseCode::AuthRequired(381)
        );
        assert_eq!(
            ResponseCode::parse(b"480 Auth required\r\n"),
            ResponseCode::AuthRequired(480)
        );
        assert_eq!(
            ResponseCode::parse(b"281 Auth success\r\n"),
            ResponseCode::AuthSuccess
        );

        // Multiline
        assert_eq!(
            ResponseCode::parse(b"100 Help\r\n"),
            ResponseCode::MultilineData(100)
        );
        assert_eq!(
            ResponseCode::parse(b"215 LIST\r\n"),
            ResponseCode::MultilineData(215)
        );
        assert_eq!(
            ResponseCode::parse(b"220 Article\r\n"),
            ResponseCode::MultilineData(220)
        );

        // Single-line
        assert_eq!(
            ResponseCode::parse(b"211 Group selected\r\n"),
            ResponseCode::SingleLine(211)
        );
        assert_eq!(
            ResponseCode::parse(b"400 Error\r\n"),
            ResponseCode::SingleLine(400)
        );

        // Invalid
        assert_eq!(ResponseCode::parse(b"XXX\r\n"), ResponseCode::Invalid);
    }

    #[test]
    fn test_response_code_is_multiline() {
        assert!(ResponseCode::parse(b"215 LIST\r\n").is_multiline());
        assert!(ResponseCode::parse(b"220 Article\r\n").is_multiline());
        assert!(!ResponseCode::parse(b"200 Ready\r\n").is_multiline());
        assert!(!ResponseCode::parse(b"211 Group\r\n").is_multiline());
    }

    #[test]
    fn test_response_code_status_code() {
        assert_eq!(ResponseCode::parse(b"200 OK\r\n").status_code(), Some(200));
        assert_eq!(ResponseCode::Disconnect.status_code(), Some(205));
        assert_eq!(ResponseCode::AuthSuccess.status_code(), Some(281));
        assert_eq!(ResponseCode::Invalid.status_code(), None);
    }

    #[test]
    fn test_response_code_is_success() {
        assert!(ResponseCode::parse(b"200 OK\r\n").is_success());
        assert!(ResponseCode::parse(b"215 LIST\r\n").is_success());
        assert!(ResponseCode::parse(b"381 Auth\r\n").is_success());
        assert!(!ResponseCode::parse(b"400 Error\r\n").is_success());
        assert!(!ResponseCode::Invalid.is_success());
    }

    #[test]
    fn test_extract_message_id() {
        // Standard message-ID
        assert_eq!(
            NntpResponse::extract_message_id("ARTICLE <test@example.com>"),
            Some("<test@example.com>".to_string())
        );

        // With extra whitespace
        assert_eq!(
            NntpResponse::extract_message_id("  BODY  <msg123@news.com>  "),
            Some("<msg123@news.com>".to_string())
        );

        // No message-ID
        assert_eq!(NntpResponse::extract_message_id("ARTICLE 123"), None);

        // Malformed (no closing >)
        assert_eq!(
            NntpResponse::extract_message_id("ARTICLE <test@example.com"),
            None
        );

        // Multiple message-IDs (returns first)
        assert_eq!(
            NntpResponse::extract_message_id("TEST <first@example.com> <second@example.com>"),
            Some("<first@example.com>".to_string())
        );
    }

    #[test]
    fn test_validate_message_id() {
        // Valid message-IDs
        assert!(NntpResponse::validate_message_id("<test@example.com>"));
        assert!(NntpResponse::validate_message_id(
            "<msg123@news.server.com>"
        ));
        assert!(NntpResponse::validate_message_id("<a@b>"));
        assert!(NntpResponse::validate_message_id("  <valid@test.com>  "));

        // Invalid: missing brackets
        assert!(!NntpResponse::validate_message_id("test@example.com"));
        assert!(!NntpResponse::validate_message_id("<test@example.com"));
        assert!(!NntpResponse::validate_message_id("test@example.com>"));

        // Invalid: missing @
        assert!(!NntpResponse::validate_message_id("<testexample.com>"));

        // Invalid: multiple @
        assert!(!NntpResponse::validate_message_id("<test@example@com>"));

        // Invalid: @ at start or end
        assert!(!NntpResponse::validate_message_id("<@example.com>"));
        assert!(!NntpResponse::validate_message_id("<test@>"));

        // Invalid: empty
        assert!(!NntpResponse::validate_message_id(""));
        assert!(!NntpResponse::validate_message_id("<>"));

        // Invalid: only brackets
        assert!(!NntpResponse::validate_message_id("<@>"));
    }
}
