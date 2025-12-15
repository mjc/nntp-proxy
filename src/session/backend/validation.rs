//! Backend response validation
//!
//! Provides pure functions for validating backend NNTP responses.
//! Does NOT handle I/O - callers are responsible for actual communication.

use crate::protocol::NntpResponse;

/// Response validation warnings (pure data)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseWarning {
    /// Response too short to be valid
    ShortResponse { bytes: usize, min: usize },
    /// Response code is invalid
    InvalidResponse,
    /// Response code is unusual (0 or >= 600)
    UnusualStatusCode(u16),
}

/// Validated backend response (pure data)
#[derive(Debug)]
pub struct ValidatedResponse {
    pub response: NntpResponse,
    pub is_multiline: bool,
    pub warnings: Vec<ResponseWarning>,
}

/// Validate backend response data (pure function - easily testable)
///
/// Checks response for:
/// - Minimum length requirement
/// - Valid response code
/// - Unusual status codes (0 or >= 600)
///
/// # Arguments
/// * `chunk` - Raw response bytes
/// * `bytes_read` - Number of bytes read
/// * `min_length` - Minimum expected length
///
/// # Returns
/// ValidatedResponse with parsed response and any warnings
#[must_use]
pub fn validate_backend_response(
    chunk: &[u8],
    bytes_read: usize,
    min_length: usize,
) -> ValidatedResponse {
    let mut warnings = Vec::new();

    // Check minimum length
    if bytes_read < min_length {
        warnings.push(ResponseWarning::ShortResponse {
            bytes: bytes_read,
            min: min_length,
        });
    }

    // Parse response code
    let response = NntpResponse::parse(&chunk[..bytes_read]);
    let is_multiline = response.is_multiline();

    // Check for invalid response
    if response == NntpResponse::Invalid {
        warnings.push(ResponseWarning::InvalidResponse);
    } else if let Some(code) = response.status_code() {
        // Check for unusual status codes
        let raw_code = code.as_u16();
        if raw_code == 0 || raw_code >= 600 {
            warnings.push(ResponseWarning::UnusualStatusCode(raw_code));
        }
    }

    ValidatedResponse {
        response,
        is_multiline,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_backend_response_valid() {
        let data = b"200 OK\r\n";
        let validated = validate_backend_response(data, data.len(), 7);

        assert!(matches!(validated.response, NntpResponse::Greeting(_)));
        assert!(!validated.is_multiline);
        assert!(validated.warnings.is_empty());
    }

    #[test]
    fn test_validate_backend_response_short() {
        let data = b"200";
        let validated = validate_backend_response(data, data.len(), 7);

        assert_eq!(validated.warnings.len(), 1);
        assert_eq!(
            validated.warnings[0],
            ResponseWarning::ShortResponse { bytes: 3, min: 7 }
        );
    }

    #[test]
    fn test_validate_backend_response_invalid() {
        let data = b"garbage response";
        let validated = validate_backend_response(data, data.len(), 7);

        assert_eq!(validated.response, NntpResponse::Invalid);
        assert!(
            validated
                .warnings
                .contains(&ResponseWarning::InvalidResponse)
        );
    }

    #[test]
    fn test_validate_backend_response_unusual_status_zero() {
        let data = b"000 Weird\r\n";
        let validated = validate_backend_response(data, data.len(), 7);

        assert!(
            validated
                .warnings
                .contains(&ResponseWarning::UnusualStatusCode(0))
        );
    }

    #[test]
    fn test_validate_backend_response_unusual_status_high() {
        let data = b"700 Too High\r\n";
        let validated = validate_backend_response(data, data.len(), 7);

        assert!(
            validated
                .warnings
                .contains(&ResponseWarning::UnusualStatusCode(700))
        );
    }

    #[test]
    fn test_validate_backend_response_multiline() {
        let data = b"220 0 <article@example.com>\r\n";
        let validated = validate_backend_response(data, data.len(), 7);

        assert!(validated.is_multiline);
        assert!(validated.warnings.is_empty());
    }

    #[test]
    fn test_validate_backend_response_multiple_warnings() {
        let data = b"000";
        let validated = validate_backend_response(data, data.len(), 7);

        // Should have both short response AND unusual status
        assert!(!validated.warnings.is_empty());
        assert!(
            validated
                .warnings
                .iter()
                .any(|w| matches!(w, ResponseWarning::ShortResponse { .. }))
        );
    }

    #[test]
    fn test_validate_backend_response_normal_codes() {
        // Test various normal response codes don't generate warnings
        let test_cases = vec![
            b"200 OK\r\n".as_ref(),
            b"211 Group info\r\n".as_ref(),
            b"480 Auth required\r\n".as_ref(),
        ];

        for data in test_cases {
            let validated = validate_backend_response(data, data.len(), 7);
            assert!(!matches!(validated.response, NntpResponse::Invalid));
            assert!(
                validated
                    .warnings
                    .iter()
                    .all(|w| !matches!(w, ResponseWarning::UnusualStatusCode(_))),
                "Normal status codes should not generate unusual code warnings"
            );
        }
    }

    #[test]
    fn test_response_warning_equality() {
        let w1 = ResponseWarning::ShortResponse { bytes: 3, min: 7 };
        let w2 = ResponseWarning::ShortResponse { bytes: 3, min: 7 };
        let w3 = ResponseWarning::InvalidResponse;

        assert_eq!(w1, w2);
        assert_ne!(w1, w3);
    }

    #[test]
    fn test_response_warning_clone() {
        let w1 = ResponseWarning::UnusualStatusCode(999);
        let w2 = w1.clone();
        assert_eq!(w1, w2);
    }
}
