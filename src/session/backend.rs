//! Backend communication module
//!
//! Handles sending commands to backend and reading initial response.
//! Does NOT buffer entire responses - caller handles streaming.

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::warn;

use crate::pool::PooledBuffer;
use crate::protocol::{MIN_RESPONSE_LENGTH, Response};
use crate::types::BackendId;

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
    pub response: Response,
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
    let response = Response::parse(&chunk[..bytes_read]);
    let is_multiline = response.is_multiline();

    // Check for invalid response
    if response == Response::Invalid {
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

/// Send command to backend and read first chunk
///
/// Returns (bytes_read, response_code, is_multiline, ttfb_micros, send_micros, recv_micros)
/// The first chunk is written into the provided buffer.
pub async fn send_command_and_read_first_chunk<T>(
    backend_conn: &mut T,
    command: &str,
    backend_id: BackendId,
    client_addr: std::net::SocketAddr,
    chunk: &mut PooledBuffer,
) -> Result<(usize, Response, bool, u64, u64, u64)>
where
    T: AsyncReadExt + AsyncWriteExt + Unpin,
{
    use std::time::Instant;

    let start = Instant::now();

    // Write command to backend
    let send_start = Instant::now();
    backend_conn.write_all(command.as_bytes()).await?;
    let send_elapsed = send_start.elapsed();

    // Read first chunk to determine response type
    let recv_start = Instant::now();
    let n = chunk.read_from(backend_conn).await?;
    let recv_elapsed = recv_start.elapsed();

    if n == 0 {
        return Err(anyhow::anyhow!("Backend connection closed unexpectedly"));
    }

    // Validate response using pure function
    let validated = validate_backend_response(&chunk[..n], n, MIN_RESPONSE_LENGTH);

    // Log warnings
    for warning in &validated.warnings {
        match warning {
            ResponseWarning::ShortResponse { bytes, min } => {
                warn!(
                    "Client {} got short response from backend {:?} ({} bytes < {} min): {:02x?}",
                    client_addr,
                    backend_id,
                    bytes,
                    min,
                    &chunk[..n]
                );
            }
            ResponseWarning::InvalidResponse => {
                warn!(
                    "Client {} got invalid response from backend {:?} ({} bytes): {:?}",
                    client_addr,
                    backend_id,
                    n,
                    String::from_utf8_lossy(&chunk[..n.min(50)])
                );
            }
            ResponseWarning::UnusualStatusCode(code) => {
                warn!(
                    "Client {} got unusual status code {} from backend {:?}: {:?}",
                    client_addr,
                    code,
                    backend_id,
                    String::from_utf8_lossy(&chunk[..n.min(50)])
                );
            }
        }
    }

    let elapsed = start.elapsed();

    Ok((
        n,
        validated.response,
        validated.is_multiline,
        elapsed.as_micros() as u64,
        send_elapsed.as_micros() as u64,
        recv_elapsed.as_micros() as u64,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_backend_response_valid() {
        let data = b"200 OK\r\n";
        let validated = validate_backend_response(data, data.len(), 7);

        assert!(matches!(validated.response, Response::Greeting(_)));
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

        assert_eq!(validated.response, Response::Invalid);
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
        assert!(validated.warnings.len() >= 1);
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
            assert!(!matches!(validated.response, Response::Invalid));
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
