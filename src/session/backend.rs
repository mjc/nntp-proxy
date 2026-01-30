//! Backend communication module
//!
//! This module handles NNTP client operations when proxying to backend servers.
//! The proxy acts as an NNTP client to upstream servers.
//!
//! # Structure
//!
//! - Response validation functions (pure, easily testable)
//! - Command execution helpers - Send command, read response
//!
//! # Usage
//!
//! ```ignore
//! use crate::session::backend::{send_command, CommandResponse};
//!
//! let response = send_command(&mut conn, command, &mut buffer).await?;
//! if response.is_430() {
//!     // Article not found
//! }
//! ```

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::pool::PooledBuffer;
use crate::protocol::NntpResponse;

// ─── Response validation ────────────────────────────────────────────────────

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

// ─── Command execution ──────────────────────────────────────────────────────

/// Result of sending a command to a backend
#[derive(Debug)]
pub struct CommandResponse {
    /// Number of bytes read into buffer
    pub bytes_read: usize,
    /// Parsed NNTP response
    pub response: NntpResponse,
    /// Whether this is a multiline response
    pub is_multiline: bool,
    /// Any validation warnings
    pub warnings: Vec<ResponseWarning>,
}

impl CommandResponse {
    /// Check if response is 430 (article not found)
    #[inline]
    pub fn is_430(&self) -> bool {
        self.response
            .status_code()
            .is_some_and(|code| code.as_u16() == 430)
    }

    /// Get status code if valid
    #[inline]
    pub fn status_code(&self) -> Option<u16> {
        self.response.status_code().map(|c| c.as_u16())
    }
}

/// Send command to backend and read first response chunk
///
/// This is the core NNTP client operation: send a command, get a response.
/// For multiline responses, only the first chunk is read - caller must
/// stream the rest.
///
/// # Arguments
/// * `conn` - Backend connection (anything implementing AsyncRead + AsyncWrite)
/// * `command` - NNTP command to send (should include \r\n)
/// * `buffer` - Buffer to read response into
///
/// # Returns
/// `CommandResponse` with parsed response and buffer position
///
/// # Errors
/// Returns error if write fails, read fails, or connection closes unexpectedly
pub async fn send_command<C>(
    conn: &mut C,
    command: &str,
    buffer: &mut PooledBuffer,
) -> Result<CommandResponse>
where
    C: AsyncReadExt + AsyncWriteExt + Unpin,
{
    // Delegate to timed version, discard timing
    let (response, _, _, _) = send_command_timed(conn, command, buffer).await?;
    Ok(response)
}

/// Send command with timing measurements
///
/// Like `send_command` but also returns timing information for metrics.
///
/// # Returns
/// Tuple of (CommandResponse, ttfb_micros, send_micros, recv_micros)
pub async fn send_command_timed<C>(
    conn: &mut C,
    command: &str,
    buffer: &mut PooledBuffer,
) -> Result<(CommandResponse, u64, u64, u64)>
where
    C: AsyncReadExt + AsyncWriteExt + Unpin,
{
    use std::time::Instant;

    let start = Instant::now();

    // Send command
    let send_start = Instant::now();
    conn.write_all(command.as_bytes()).await?;
    let send_elapsed = send_start.elapsed();

    // Read first chunk
    let recv_start = Instant::now();
    let n = buffer.read_from(conn).await?;
    if n == 0 {
        anyhow::bail!("Backend connection closed unexpectedly");
    }
    let recv_elapsed = recv_start.elapsed();

    // Validate response
    let validated =
        validate_backend_response(&buffer[..n], n, crate::protocol::MIN_RESPONSE_LENGTH);

    let elapsed = start.elapsed();

    Ok((
        CommandResponse {
            bytes_read: n,
            response: validated.response,
            is_multiline: validated.is_multiline,
            warnings: validated.warnings,
        },
        elapsed.as_micros() as u64,
        send_elapsed.as_micros() as u64,
        recv_elapsed.as_micros() as u64,
    ))
}

/// Log validation warnings with context
pub fn log_warnings(
    warnings: &[ResponseWarning],
    buffer: &[u8],
    bytes_read: usize,
    client_addr: impl std::fmt::Display,
    backend_id: crate::types::BackendId,
) {
    use tracing::warn;

    for warning in warnings {
        match warning {
            ResponseWarning::ShortResponse { bytes, min } => {
                warn!(
                    "Client {} got short response from backend {:?} ({} bytes < {} min): {:02x?}",
                    client_addr,
                    backend_id,
                    bytes,
                    min,
                    &buffer[..bytes_read]
                );
            }
            ResponseWarning::InvalidResponse => {
                warn!(
                    "Client {} got invalid response from backend {:?} ({} bytes): {:?}",
                    client_addr,
                    backend_id,
                    bytes_read,
                    String::from_utf8_lossy(&buffer[..bytes_read.min(50)])
                );
            }
            ResponseWarning::UnusualStatusCode(code) => {
                warn!(
                    "Client {} got unusual status code {} from backend {:?}: {:?}",
                    client_addr,
                    code,
                    backend_id,
                    String::from_utf8_lossy(&buffer[..bytes_read.min(50)])
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── Command response tests ─────────────────────────────────────────────

    #[test]
    fn test_command_response_is_430() {
        // Create a 430 response
        let response = CommandResponse {
            bytes_read: 20,
            response: NntpResponse::parse(b"430 No such article\r\n"),
            is_multiline: false,
            warnings: vec![],
        };
        assert!(response.is_430());

        // Create a 220 response
        let response = CommandResponse {
            bytes_read: 30,
            response: NntpResponse::parse(b"220 0 <msg@example.com>\r\n"),
            is_multiline: true,
            warnings: vec![],
        };
        assert!(!response.is_430());
    }

    #[test]
    fn test_command_response_status_code() {
        let response = CommandResponse {
            bytes_read: 10,
            response: NntpResponse::parse(b"211 Group\r\n"),
            is_multiline: false,
            warnings: vec![],
        };
        assert_eq!(response.status_code(), Some(211));
    }

    // ─── Validation tests ───────────────────────────────────────────────────

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
