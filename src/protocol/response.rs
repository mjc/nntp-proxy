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

    /// Check if this is a continuation code (3xx)
    ///
    /// Per [RFC 3977 §3.2.1](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2.1):
    /// - 3xx: Success so far, send more input
    #[inline]
    #[must_use]
    pub fn is_continuation(&self) -> bool {
        let code = self.into_inner();
        (300..400).contains(&code)
    }

    /// Check if this is an informational code (1xx)
    #[inline]
    #[must_use]
    pub fn is_informational(&self) -> bool {
        let code = self.into_inner();
        (100..200).contains(&code)
    }

    /// Backend greeting accepted by RFC 3977 connection setup.
    #[inline]
    #[must_use]
    pub fn is_greeting(&self) -> bool {
        matches!(self.into_inner(), 200 | 201)
    }

    /// AUTHINFO response indicating a password or authentication is required.
    #[inline]
    #[must_use]
    pub fn requires_auth_credentials(&self) -> bool {
        matches!(self.into_inner(), 381 | 480)
    }

    /// AUTHINFO response indicating authentication succeeded.
    #[inline]
    #[must_use]
    pub fn is_auth_accepted(&self) -> bool {
        self.into_inner() == 281
    }

    /// Article-not-found response.
    #[inline]
    #[must_use]
    pub fn is_article_missing(&self) -> bool {
        self.into_inner() == 430
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
    #[must_use]
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
        let code = u16::from(d0) * 100 + u16::from(d1) * 10 + u16::from(d2);
        Some(Self::new(code))
    }
}

#[cfg(test)]
mod tests {
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
    fn test_status_code_parsing() {
        assert_eq!(StatusCode::parse(b"200"), Some(StatusCode::new(200)));
        assert_eq!(
            StatusCode::parse(b"200 Ready\r\n"),
            Some(StatusCode::new(200))
        );
        assert_eq!(
            StatusCode::parse(b"381 Password required\r\n"),
            Some(StatusCode::new(381))
        );
        assert_eq!(
            StatusCode::parse(b"500 Error\r\n"),
            Some(StatusCode::new(500))
        );
        assert_eq!(StatusCode::parse(b""), None);
        assert_eq!(StatusCode::parse(b"XX"), None);
        assert_eq!(StatusCode::parse(b"ABC Invalid\r\n"), None);
        assert_eq!(StatusCode::parse(b"20"), None);
        assert_eq!(StatusCode::parse(b"2X0 Error\r\n"), None);
    }

    #[test]
    fn test_status_code_setup_helpers() {
        assert!(StatusCode::new(200).is_greeting());
        assert!(StatusCode::new(201).is_greeting());
        assert!(!StatusCode::new(205).is_greeting());

        assert!(StatusCode::new(381).requires_auth_credentials());
        assert!(StatusCode::new(480).requires_auth_credentials());
        assert!(!StatusCode::new(281).requires_auth_credentials());

        assert!(StatusCode::new(281).is_auth_accepted());
        assert!(!StatusCode::new(381).is_auth_accepted());

        assert!(StatusCode::new(430).is_article_missing());
        assert!(!StatusCode::new(400).is_article_missing());
    }

    #[test]
    fn test_edge_cases() {
        // UTF-8 in responses
        let utf8_response = "200 Привет мир\r\n".as_bytes();
        assert_eq!(StatusCode::parse(utf8_response), Some(StatusCode::new(200)));

        // Response with binary data
        let with_null = b"200 Test\x00Message\r\n";
        assert_eq!(StatusCode::parse(with_null), Some(StatusCode::new(200)));

        // Boundary status codes
        assert_eq!(
            StatusCode::parse(b"100 Info\r\n"),
            Some(StatusCode::new(100))
        );
        assert_eq!(
            StatusCode::parse(b"599 Error\r\n"),
            Some(StatusCode::new(599))
        );
    }
}
