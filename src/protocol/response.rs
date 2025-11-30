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
pub enum NntpResponse {
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

impl NntpResponse {
    /// Parse response data into a categorized response code
    ///
    /// Per [RFC 3977 §3.2](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2),
    /// responses start with a 3-digit status code.
    ///
    /// **Optimization**: Direct byte-to-digit conversion avoids UTF-8 overhead.
    #[inline]
    pub fn parse(data: &[u8]) -> Self {
        let code = match parse_status_code(data) {
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

/// Response parser for NNTP protocol
pub struct ResponseParser;

impl ResponseParser {
    /// Check if a response starts with a success code (2xx or 3xx)
    #[inline]
    #[allow(dead_code)]
    pub fn is_success_response(data: &[u8]) -> bool {
        NntpResponse::parse(data).is_success()
    }

    /// Check if response is a greeting (200 or 201)
    #[inline]
    #[allow(dead_code)]
    pub fn is_greeting(data: &[u8]) -> bool {
        matches!(NntpResponse::parse(data), NntpResponse::Greeting(_))
    }

    /// Check if response indicates authentication is required (381 or 480)
    #[inline]
    #[allow(dead_code)]
    pub fn is_auth_required(data: &[u8]) -> bool {
        matches!(NntpResponse::parse(data), NntpResponse::AuthRequired(_))
    }

    /// Check if response indicates successful authentication (281)
    #[inline]
    #[allow(dead_code)]
    pub fn is_auth_success(data: &[u8]) -> bool {
        matches!(NntpResponse::parse(data), NntpResponse::AuthSuccess)
    }

    /// Check if response has a specific status code
    ///
    /// This is useful for checking specific response codes like 111 (DATE response),
    /// or any other specific code that doesn't have a dedicated helper.
    #[inline]
    pub fn is_response_code(data: &[u8], code: u16) -> bool {
        parse_status_code(data).is_some_and(|c| c.as_u16() == code)
    }
}

#[cfg(test)]
mod tests;
