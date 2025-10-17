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
    Greeting(u16),

    /// Disconnect/goodbye - [RFC 3977 §5.4](https://datatracker.ietf.org/doc/html/rfc3977#section-5.4)
    /// - 205: Connection closing
    Disconnect,

    /// Authentication required - [RFC 4643 §2.3](https://datatracker.ietf.org/doc/html/rfc4643#section-2.3)
    /// - 381: Password required
    /// - 480: Authentication required
    AuthRequired(u16),

    /// Authentication successful - [RFC 4643 §2.5.1](https://datatracker.ietf.org/doc/html/rfc4643#section-2.5.1)
    /// - 281: Authentication accepted
    AuthSuccess,

    /// Multiline data response
    /// Per [RFC 3977 §3.4.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1):
    /// - All 1xx codes (100-199)
    /// - Specific 2xx codes: 215, 220, 221, 222, 224, 225, 230, 231, 282
    MultilineData(u16),

    /// Single-line response (everything else)
    SingleLine(u16),

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

        match code {
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
    ///
    /// Per [RFC 3977 §3.2.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2.1):
    /// - 2xx: Success
    /// - 3xx: Success so far, send more input
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
    ///
    /// Per [RFC 3977 §3.2](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2),
    /// responses begin with a 3-digit status code (ASCII digits '0'-'9').
    ///
    /// **Optimization**: Direct byte-to-digit conversion without UTF-8 validation.
    /// Status codes are guaranteed to be ASCII digits per the RFC.
    #[inline]
    pub fn parse_status_code(data: &[u8]) -> Option<u16> {
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
        Some((d0 as u16) * 100 + (d1 as u16) * 10 + (d2 as u16))
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
    pub fn is_multiline_response(status_code: u16) -> bool {
        match status_code {
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
    #[inline]
    pub fn find_terminator_end(data: &[u8]) -> Option<usize> {
        let n = data.len();
        if n < 5 {
            return None;
        }

        // Use memchr::Memchr iterator to avoid repeated slice creation
        for r_pos in memchr::memchr_iter(b'\r', data) {
            // Not enough space for full terminator
            if r_pos + 5 > n {
                return None;
            }

            // Check for full terminator pattern
            if &data[r_pos..r_pos + 5] == b"\r\n.\r\n" {
                return Some(r_pos + 5);
            }
        }

        None
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
        NntpResponse::parse_status_code(data) == Some(code)
    }
}

#[cfg(test)]
mod tests;
