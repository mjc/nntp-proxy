//! NNTP Response Parsing and Handling
//!
//! This module implements efficient parsing of NNTP server responses according to
//! [RFC 3977](https://datatracker.ietf.org/doc/html/rfc3977) with optimizations
//! for high-throughput proxy use.
//!
//! # NNTP Protocol References
//!
//! - **[RFC 3977 §3.2]** - Response format and status codes
//! - **[RFC 3977 §3.4.1]** - Multiline data blocks
//! - **[RFC 5536 §3.1.3]** - Message-ID format specification
//!
//! [RFC 3977 §3.2]: https://datatracker.ietf.org/doc/html/rfc3977#section-3.2
//! [RFC 3977 §3.4.1]: https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1
//! [RFC 5536 §3.1.3]: https://datatracker.ietf.org/doc/html/rfc5536#section-3.1.3
//!
//! # Response Format
//!
//! Per [RFC 3977 §3.2](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2):
//! ```text
//! response     = status-line [CRLF multiline-data]
//! status-line  = status-code SP status-text CRLF
//! status-code  = 3DIGIT
//! ```
//!
//! # Multiline Responses
//!
//! Per [RFC 3977 §3.4.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1):
//! ```text
//! Multiline responses end with a line containing a single period:
//! CRLF "." CRLF
//! ```

use crate::types::MessageId;

/// Raw NNTP status code (3-digit number)
///
/// Per [RFC 3977 §3.2](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2),
/// all NNTP responses start with a 3-digit status code (100-599).
///
/// This newtype ensures type safety and prevents mixing status codes with other integers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StatusCode(u16);

impl StatusCode {
    /// Create a new status code (unchecked - for internal use)
    #[inline]
    pub const fn new(code: u16) -> Self {
        Self(code)
    }

    /// Get the raw numeric value
    #[inline]
    #[must_use]
    pub const fn as_u16(self) -> u16 {
        self.0
    }

    /// Check if this is a success code (2xx or 3xx)
    ///
    /// Per [RFC 3977 §3.2.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2.1):
    /// - 2xx: Success
    /// - 3xx: Success so far, send more input
    #[inline]
    #[must_use]
    pub const fn is_success(self) -> bool {
        self.0 >= 200 && self.0 < 400
    }

    /// Check if this is an error code (4xx or 5xx)
    #[inline]
    #[must_use]
    pub const fn is_error(self) -> bool {
        self.0 >= 400 && self.0 < 600
    }

    /// Check if this is an informational code (1xx)
    #[inline]
    #[must_use]
    pub const fn is_informational(self) -> bool {
        self.0 >= 100 && self.0 < 200
    }
}

impl std::fmt::Display for StatusCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Categorized NNTP response code for type-safe handling
///
/// This enum categorizes NNTP response codes based on their semantics and
/// handling requirements per [RFC 3977 §3.2](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2).
///
/// # Response Code Ranges
///
/// Per [RFC 3977 §3.2.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2.1):
/// - **1xx**: Informational (multiline data follows)
/// - **2xx**: Success (may be multiline)
/// - **3xx**: Success so far, further input expected
/// - **4xx**: Temporary failure
/// - **5xx**: Permanent failure
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResponseCode {
    /// Server greeting - [RFC 3977 §5.1](https://datatracker.ietf.org/doc/html/rfc3977#section-5.1)
    /// - 200: Posting allowed
    /// - 201: No posting allowed
    Greeting(StatusCode),

    /// Disconnect/goodbye - [RFC 3977 §5.4](https://datatracker.ietf.org/doc/html/rfc3977#section-5.4)
    /// - 205: Connection closing
    Disconnect,

    /// Authentication required - [RFC 4643 §2.3](https://datatracker.ietf.org/doc/html/rfc4643#section-2.3)
    /// - 381: Password required
    /// - 480: Authentication required
    AuthRequired(StatusCode),

    /// Authentication successful - [RFC 4643 §2.5.1](https://datatracker.ietf.org/doc/html/rfc4643#section-2.5.1)
    /// - 281: Authentication accepted
    AuthSuccess,

    /// Multiline data response
    /// Per [RFC 3977 §3.4.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1):
    /// - All 1xx codes (100-199)
    /// - Specific 2xx codes: 215, 220, 221, 222, 224, 225, 230, 231, 282
    MultilineData(StatusCode),

    /// Single-line response (everything else)
    SingleLine(StatusCode),

    /// Invalid or unparseable response
    Invalid,
}

impl ResponseCode {
    /// Parse response data into a categorized response code
    ///
    /// Per [RFC 3977 §3.2](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2),
    /// responses start with a 3-digit status code.
    ///
    /// **Optimization**: Direct byte-to-digit conversion avoids UTF-8 overhead.
    #[inline]
    pub fn parse(data: &[u8]) -> Self {
        let code = match NntpResponse::parse_status_code(data) {
            Some(c) => c,
            None => return Self::Invalid,
        };

        match code.as_u16() {
            // [RFC 3977 §5.1](https://datatracker.ietf.org/doc/html/rfc3977#section-5.1)
            200 | 201 => Self::Greeting(code),

            // [RFC 3977 §5.4](https://datatracker.ietf.org/doc/html/rfc3977#section-5.4)
            205 => Self::Disconnect,

            // [RFC 4643 §2.5.1](https://datatracker.ietf.org/doc/html/rfc4643#section-2.5.1)
            281 => Self::AuthSuccess,

            // [RFC 4643 §2.3](https://datatracker.ietf.org/doc/html/rfc4643#section-2.3)
            381 | 480 => Self::AuthRequired(code),

            // Multiline responses per [RFC 3977 §3.4.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1)
            // All 1xx are informational multiline
            100..=199 => Self::MultilineData(code),
            // Specific 2xx multiline responses
            215 | 220 | 221 | 222 | 224 | 225 | 230 | 231 | 282 => Self::MultilineData(code),

            // Everything else is a single-line response
            _ => Self::SingleLine(code),
        }
    }

    /// Check if this response type is multiline
    ///
    /// Per [RFC 3977 §3.4.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1),
    /// multiline responses require special handling with terminator detection.
    #[inline]
    #[must_use]
    pub const fn is_multiline(&self) -> bool {
        matches!(self, Self::MultilineData(_))
    }

    /// Get the numeric status code if available
    #[inline]
    #[must_use]
    pub const fn status_code(&self) -> Option<StatusCode> {
        match self {
            Self::Greeting(c)
            | Self::AuthRequired(c)
            | Self::MultilineData(c)
            | Self::SingleLine(c) => Some(*c),
            Self::Disconnect => Some(StatusCode::new(205)),
            Self::AuthSuccess => Some(StatusCode::new(281)),
            Self::Invalid => None,
        }
    }

    /// Check if this is a success response (2xx or 3xx)
    ///
    /// Per [RFC 3977 §3.2.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2.1):
    /// - 2xx: Success
    /// - 3xx: Success so far, send more input
    #[inline]
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.status_code().is_some_and(|code| code.is_success())
    }
}

/// Represents a parsed NNTP response
#[derive(Debug, Clone, PartialEq)]
pub struct NntpResponse {
    /// Status code (e.g., 200, 381, 500)
    pub status_code: StatusCode,
    /// Whether this is a multiline response
    pub is_multiline: bool,
    /// Complete response data including status line
    pub data: Vec<u8>,
}

impl NntpResponse {
    /// Parse a status code from response data
    ///
    /// Per [RFC 3977 §3.2](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2),
    /// responses begin with a 3-digit status code (ASCII digits '0'-'9').
    ///
    /// **Optimization**: Direct byte-to-digit conversion without UTF-8 validation.
    /// Status codes are guaranteed to be ASCII digits per the RFC.
    #[inline]
    pub fn parse_status_code(data: &[u8]) -> Option<StatusCode> {
        if data.len() < 3 {
            return None;
        }

        // Fast path: Direct ASCII digit conversion without UTF-8 overhead
        // Per RFC 3977, status codes are exactly 3 ASCII digits
        let d0 = data[0].wrapping_sub(b'0');
        let d1 = data[1].wrapping_sub(b'0');
        let d2 = data[2].wrapping_sub(b'0');

        // Validate all three are digits (0-9)
        if d0 > 9 || d1 > 9 || d2 > 9 {
            return None;
        }

        // Combine into u16: d0*100 + d1*10 + d2
        let code = (d0 as u16) * 100 + (d1 as u16) * 10 + (d2 as u16);
        Some(StatusCode::new(code))
    }

    /// Check if a response indicates a multiline response
    ///
    /// Per [RFC 3977 §3.4.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1),
    /// certain status codes indicate multiline data follows.
    ///
    /// # Multiline Response Codes
    /// - **1xx**: All informational responses (100-199)
    /// - **2xx**: Specific codes - 215, 220, 221, 222, 224, 225, 230, 231, 282
    #[inline]
    pub fn is_multiline_response(status_code: StatusCode) -> bool {
        let code = status_code.as_u16();
        match code {
            100..=199 => true, // All 1xx are multiline
            215 | 220 | 221 | 222 | 224 | 225 | 230 | 231 | 282 => true, // Specific 2xx codes
            _ => false,
        }
    }

    /// Check if data ends with the NNTP multiline terminator
    ///
    /// Per [RFC 3977 §3.4.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1):
    /// ```text
    /// Multiline blocks are terminated by a line containing only a period:
    /// CRLF "." CRLF
    /// Which appears in the data stream as: \r\n.\r\n
    /// ```
    ///
    /// **Optimization**: Single suffix check, no scanning.
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
    ///
    /// Per [RFC 3977 §3.4.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1),
    /// the terminator is exactly "\r\n.\r\n" (CRLF, dot, CRLF).
    ///
    /// **Optimization**: Uses `memchr::memchr_iter()` to find '\r' bytes (SIMD-accelerated),
    /// then validates the full 5-byte pattern. This eliminates the need to create new slices
    /// on each iteration (which the manual loop approach requires with `&data[pos..]`).
    ///
    /// Benchmarks show this is **72% faster for small responses** (37ns → 13ns) and
    /// **64% faster for medium responses** (109ns → 40ns) compared to the manual loop
    /// that creates a new slice on each iteration.
    ///
    /// **Hot path optimization**: Check end first (99% case for streaming chunks),
    /// then scan forward if needed. Compiler optimizes slice comparison to memcmp.
    #[inline]
    pub fn find_terminator_end(data: &[u8]) -> Option<usize> {
        const TERMINATOR: [u8; 5] = *b"\r\n.\r\n";

        let n = data.len();
        if n < 5 {
            return None;
        }

        // Fast path: terminator at end (99% of streaming chunks)
        if data[n - 5..n] == TERMINATOR {
            return Some(n);
        }

        // Slow path: scan for terminator mid-chunk (rare - only when responses batched)
        memchr::memchr_iter(b'\r', data)
            .take_while(|&pos| pos + 5 <= n)
            .find(|&pos| data[pos..pos + 5] == TERMINATOR)
            .map(|pos| pos + 5)
    }

    /// Check if a terminator spans across a boundary between tail and current chunk
    ///
    /// This handles the case where a multiline terminator is split across two read chunks.
    /// For example: previous chunk ends with "\r\n." and current starts with "\r\n"
    ///
    /// Per [RFC 3977 §3.4.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1),
    /// the terminator is exactly "\r\n.\r\n" (CRLF, dot, CRLF).
    #[inline]
    pub fn has_spanning_terminator(
        tail: &[u8],
        tail_len: usize,
        current: &[u8],
        current_len: usize,
    ) -> bool {
        // Only check if we have a tail and current chunk is small enough for spanning
        if tail_len < 1 || !(1..=4).contains(&current_len) {
            return false;
        }

        // Check all possible split positions of the 5-byte terminator "\r\n.\r\n"
        // Split after byte 1: tail ends with "\r", current starts with "\n.\r\n"
        if tail_len >= 1
            && current_len >= 4
            && tail[tail_len - 1] == b'\r'
            && current[..4] == *b"\n.\r\n"
        {
            return true;
        }
        // Split after byte 2: tail ends with "\r\n", current starts with ".\r\n"
        if tail_len >= 2
            && current_len >= 3
            && tail[tail_len - 2..tail_len] == *b"\r\n"
            && current[..3] == *b".\r\n"
        {
            return true;
        }
        // Split after byte 3: tail ends with "\r\n.", current starts with "\r\n"
        if tail_len >= 3
            && current_len >= 2
            && tail[tail_len - 3..tail_len] == *b"\r\n."
            && current[..2] == *b"\r\n"
        {
            return true;
        }
        // Split after byte 4: tail ends with "\r\n.\r", current starts with "\n"
        if tail_len >= 4
            && current_len >= 1
            && tail[tail_len - 4..tail_len] == *b"\r\n.\r"
            && current[0] == b'\n'
        {
            return true;
        }

        false
    }

    /// Check if response is a disconnect/goodbye (205)
    ///
    /// Per [RFC 3977 §5.4](https://datatracker.ietf.org/doc/html/rfc3977#section-5.4),
    /// code 205 indicates "Connection closing" / "Goodbye".
    ///
    /// **Optimization**: Direct byte prefix check, no parsing.
    #[inline]
    pub fn is_disconnect(data: &[u8]) -> bool {
        data.len() >= 3 && data.starts_with(b"205")
    }

    /// Extract message-ID from command arguments using fast byte searching
    ///
    /// Per [RFC 5536 §3.1.3](https://datatracker.ietf.org/doc/html/rfc5536#section-3.1.3),
    /// message-IDs have the format `<local-part@domain>`.
    ///
    /// Examples:
    /// - `<article123@news.example.com>`
    /// - `<20231014.123456@server.domain>`
    ///
    /// **Optimization**: Uses `memchr` for fast '<' and '>' detection.
    #[inline]
    pub fn extract_message_id(command: &str) -> Option<MessageId<'_>> {
        let trimmed = command.trim();
        let bytes = trimmed.as_bytes();

        // Find opening '<' using fast memchr
        let start = memchr::memchr(b'<', bytes)?;

        // Find closing '>' after the '<'
        // Since end is relative to &bytes[start..], the actual position is start + end
        let end = memchr::memchr(b'>', &bytes[start + 1..])?;
        let msgid_end = start + end + 2; // +1 for the slice offset, +1 to include '>'

        // Safety: Message-IDs are ASCII, so no need for is_char_boundary checks
        // We already know msgid_end is valid since memchr found '>' at that position
        MessageId::from_borrowed(&trimmed[start..msgid_end]).ok()
    }

    /// Validate message-ID format according to [RFC 5536 §3.1.3](https://datatracker.ietf.org/doc/html/rfc5536#section-3.1.3)
    ///
    /// A valid message-ID must:
    /// - Start with '<' and end with '>'
    /// - Contain exactly one '@' character
    /// - Have content before and after the '@' (local-part and domain)
    ///
    /// Per [RFC 5536 §3.1.3](https://datatracker.ietf.org/doc/html/rfc5536#section-3.1.3):
    /// ```text
    /// msg-id = [CFWS] "<" id-left "@" id-right ">" [CFWS]
    /// ```
    ///
    /// **Note**: This is a basic validation - full RFC 5536 validation is more complex.
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
    #[inline]
    #[allow(dead_code)]
    pub fn is_success_response(data: &[u8]) -> bool {
        ResponseCode::parse(data).is_success()
    }

    /// Check if response is a greeting (200 or 201)
    #[inline]
    #[allow(dead_code)]
    pub fn is_greeting(data: &[u8]) -> bool {
        matches!(ResponseCode::parse(data), ResponseCode::Greeting(_))
    }

    /// Check if response indicates authentication is required (381 or 480)
    #[inline]
    #[allow(dead_code)]
    pub fn is_auth_required(data: &[u8]) -> bool {
        matches!(ResponseCode::parse(data), ResponseCode::AuthRequired(_))
    }

    /// Check if response indicates successful authentication (281)
    #[inline]
    #[allow(dead_code)]
    pub fn is_auth_success(data: &[u8]) -> bool {
        matches!(ResponseCode::parse(data), ResponseCode::AuthSuccess)
    }

    /// Check if response has a specific status code
    ///
    /// This is useful for checking specific response codes like 111 (DATE response),
    /// or any other specific code that doesn't have a dedicated helper.
    #[inline]
    pub fn is_response_code(data: &[u8], code: u16) -> bool {
        NntpResponse::parse_status_code(data).is_some_and(|c| c.as_u16() == code)
    }
}

#[cfg(test)]
mod tests;
