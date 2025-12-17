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

use nutype::nutype;

/// Raw NNTP status code (3-digit number)
///
/// Per [RFC 3977 §3.2](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2),
/// all NNTP responses start with a 3-digit status code (100-599).
#[nutype(derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Display, AsRef, Deref
))]
pub struct StatusCode(u16);

impl StatusCode {
    /// Get the raw numeric value
    #[inline]
    #[must_use]
    pub fn as_u16(&self) -> u16 {
        self.into_inner()
    }

    /// Check if this is a success code (2xx or 3xx)
    ///
    /// Per [RFC 3977 §3.2.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2.1):
    /// - 2xx: Success
    /// - 3xx: Success so far, send more input
    #[inline]
    #[must_use]
    pub fn is_success(&self) -> bool {
        let code = self.into_inner();
        (200..400).contains(&code)
    }

    /// Check if this is an error code (4xx or 5xx)
    #[inline]
    #[must_use]
    pub fn is_error(&self) -> bool {
        let code = self.into_inner();
        (400..600).contains(&code)
    }

    /// Check if this is an informational code (1xx)
    #[inline]
    #[must_use]
    pub fn is_informational(&self) -> bool {
        let code = self.into_inner();
        (100..200).contains(&code)
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
        let code = match StatusCode::parse(data) {
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
    pub fn status_code(&self) -> Option<StatusCode> {
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

impl StatusCode {
    /// Parse a status code from response data
    ///
    /// Per [RFC 3977 §3.2](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2),
    /// responses begin with a 3-digit status code (ASCII digits '0'-'9').
    ///
    /// **Optimization**: Direct byte-to-digit conversion without UTF-8 validation.
    /// Status codes are guaranteed to be ASCII digits per the RFC.
    #[inline]
    pub fn parse(data: &[u8]) -> Option<Self> {
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
        Some(Self::new(code))
    }

    /// Check if this code indicates a multiline response
    ///
    /// Per [RFC 3977 §3.4.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.4.1),
    /// certain status codes indicate multiline data follows.
    ///
    /// # Multiline Response Codes
    /// - **1xx**: All informational responses (100-199)
    /// - **2xx**: Specific codes - 215, 220, 221, 222, 224, 225, 230, 231, 282
    #[inline]
    #[must_use]
    pub fn is_multiline(&self) -> bool {
        match **self {
            100..=199 => true, // All 1xx are multiline
            215 | 220 | 221 | 222 | 224 | 225 | 230 | 231 | 282 => true, // Specific 2xx codes
            _ => false,
        }
    }
}

#[cfg(test)]
mod inline_tests {
    use super::*;

    #[test]
    fn test_status_code_categories() {
        assert!(StatusCode::new(100).is_informational());
        assert!(StatusCode::new(200).is_success());
        assert!(StatusCode::new(381).is_success()); // 3xx counts as success
        assert!(StatusCode::new(400).is_error());
        assert!(StatusCode::new(500).is_error());
        assert!(!StatusCode::new(200).is_error());
    }

    #[test]
    fn test_status_code_multiline() {
        // All 1xx are multiline
        assert!(StatusCode::new(111).is_multiline());
        // Specific 2xx multiline codes
        assert!(StatusCode::new(220).is_multiline());
        assert!(!StatusCode::new(200).is_multiline());
        assert!(!StatusCode::new(400).is_multiline());
    }

    #[test]
    fn test_status_code_parsing() {
        assert_eq!(StatusCode::parse(b"200"), Some(StatusCode::new(200)));
        assert_eq!(StatusCode::parse(b""), None);
        assert_eq!(StatusCode::parse(b"XX"), None);
    }

    #[test]
    fn test_nntp_response_categorization() {
        assert!(matches!(
            NntpResponse::parse(b"200 OK\r\n"),
            NntpResponse::Greeting(_)
        ));
        assert_eq!(
            NntpResponse::parse(b"205 Bye\r\n"),
            NntpResponse::Disconnect
        );
        assert!(matches!(
            NntpResponse::parse(b"381 Pass\r\n"),
            NntpResponse::AuthRequired(_)
        ));
        assert_eq!(
            NntpResponse::parse(b"281 OK\r\n"),
            NntpResponse::AuthSuccess
        );
        assert!(matches!(
            NntpResponse::parse(b"220 Art\r\n"),
            NntpResponse::MultilineData(_)
        ));
        assert!(matches!(
            NntpResponse::parse(b"211 Group\r\n"),
            NntpResponse::SingleLine(_)
        ));
        assert_eq!(NntpResponse::parse(b""), NntpResponse::Invalid);
    }

    #[test]
    fn test_response_is_success() {
        assert!(NntpResponse::parse(b"200 OK\r\n").is_success());
        assert!(NntpResponse::parse(b"281 Auth\r\n").is_success());
        assert!(!NntpResponse::parse(b"400 Error\r\n").is_success());
        assert!(!NntpResponse::parse(b"500 Error\r\n").is_success());
    }

    #[test]
    fn test_response_is_multiline() {
        assert!(NntpResponse::parse(b"220 Article\r\n").is_multiline());
        assert!(!NntpResponse::parse(b"200 OK\r\n").is_multiline());
    }
}

#[cfg(test)]
mod tests;
