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
use smallvec::SmallVec;
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
    pub warnings: SmallVec<[ResponseWarning; 0]>,
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
/// `ValidatedResponse` with parsed response and any warnings
#[must_use]
pub fn validate_backend_response(
    chunk: &[u8],
    bytes_read: usize,
    min_length: usize,
) -> ValidatedResponse {
    let mut warnings = SmallVec::new();

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

/// Format a hex preview of response bytes for debugging Invalid responses
///
/// # Arguments
/// * `data` - Raw response bytes
/// * `max_bytes` - Maximum number of bytes to include in preview
///
/// # Returns
/// Hex string with space-separated bytes (e.g., "41 42 43" for "ABC")
///
/// # Examples
/// ```
/// # use nntp_proxy::session::backend::format_hex_preview;
/// let data = b"430 No such article\r\n";
/// let hex = format_hex_preview(data, 256);
/// assert!(hex.starts_with("34 33 30 20")); // "430 "
/// ```
#[must_use]
pub fn format_hex_preview(data: &[u8], max_bytes: usize) -> String {
    let preview = &data[..data.len().min(max_bytes)];
    preview
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
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
    pub warnings: SmallVec<[ResponseWarning; 0]>,
}

impl CommandResponse {
    /// Get status code if valid
    #[inline]
    pub fn status_code(&self) -> Option<crate::protocol::StatusCode> {
        self.response.status_code()
    }

    /// Log validation warnings with context
    pub fn log_warnings(
        &self,
        buffer: &[u8],
        client_addr: impl std::fmt::Display,
        backend_id: crate::types::BackendId,
    ) {
        use tracing::warn;

        for warning in &self.warnings {
            match warning {
                ResponseWarning::ShortResponse { bytes, min } => {
                    let clamped_len = self.bytes_read.min(buffer.len());
                    warn!(
                        "Client {} got short response from backend {:?} ({} bytes < {} min): {:02x?}",
                        client_addr,
                        backend_id,
                        bytes,
                        min,
                        &buffer[..clamped_len]
                    );
                }
                ResponseWarning::InvalidResponse => {
                    let clamped_len = self.bytes_read.min(buffer.len());
                    warn!(
                        client = %client_addr,
                        backend = ?backend_id,
                        bytes_read = self.bytes_read,
                        first_bytes_hex = %format_hex_preview(&buffer[..clamped_len], 256),
                        first_bytes_utf8 = %String::from_utf8_lossy(&buffer[..clamped_len.min(256)]),
                        "Backend returned invalid response"
                    );
                }
                ResponseWarning::UnusualStatusCode(code) => {
                    let clamped_len = self.bytes_read.min(buffer.len());
                    warn!(
                        client = %client_addr,
                        backend = ?backend_id,
                        status_code = code,
                        bytes_read = self.bytes_read,
                        first_bytes_hex = %format_hex_preview(&buffer[..clamped_len], 256),
                        first_bytes_utf8 = %String::from_utf8_lossy(&buffer[..clamped_len.min(256)]),
                        "Backend returned unusual status code"
                    );
                }
            }
        }
    }
}

/// Send command to backend and read first response chunk
///
/// This is the core NNTP client operation: send a command, get a response.
/// For multiline responses, only the first chunk is read - caller must
/// stream the rest.
///
/// # Arguments
/// * `conn` - Backend connection (anything implementing `AsyncRead` + `AsyncWrite`)
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
/// Tuple of (`CommandResponse`, `ttfb_micros`, `send_micros`, `recv_micros`)
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

    // H5: If first read returned fewer bytes than needed to parse a status code,
    // accumulate more data. This handles the rare case where a TCP segment
    // boundary splits the 3-digit status code across reads.
    let min_len = crate::protocol::MIN_RESPONSE_LENGTH;
    if n < min_len {
        loop {
            let more = buffer.read_more(conn).await?;
            if more == 0 {
                break; // EOF — validate what we have
            }
            if buffer.initialized() >= min_len {
                break; // Enough data to validate
            }
        }
    }
    let total = buffer.initialized();
    let recv_elapsed = recv_start.elapsed();

    // Validate response
    let validated = validate_backend_response(&buffer[..total], total, min_len);

    let elapsed = start.elapsed();

    Ok((
        CommandResponse {
            bytes_read: total,
            response: validated.response,
            is_multiline: validated.is_multiline,
            warnings: validated.warnings,
        },
        elapsed.as_micros() as u64,
        send_elapsed.as_micros() as u64,
        recv_elapsed.as_micros() as u64,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Mock stream that returns data in configurable chunks
    struct ChunkedStream {
        chunks: VecDeque<Vec<u8>>,
        written: Vec<u8>,
    }

    impl ChunkedStream {
        fn new(chunks: Vec<Vec<u8>>) -> Self {
            Self {
                chunks: chunks.into(),
                written: Vec::new(),
            }
        }
    }

    impl tokio::io::AsyncRead for ChunkedStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if let Some(chunk) = self.chunks.pop_front() {
                let len = chunk.len().min(buf.remaining());
                buf.put_slice(&chunk[..len]);
                if len < chunk.len() {
                    // Put remainder back
                    self.chunks.push_front(chunk[len..].to_vec());
                }
                Poll::Ready(Ok(()))
            } else {
                // EOF
                Poll::Ready(Ok(()))
            }
        }
    }

    impl tokio::io::AsyncWrite for ChunkedStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            self.written.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn test_send_command_partial_read_accumulates() {
        // Simulate a backend that sends "200 OK\r\n" in two chunks:
        // first read returns "20", second returns "0 OK\r\n"
        let mut stream = ChunkedStream::new(vec![b"20".to_vec(), b"0 OK\r\n".to_vec()]);

        let pool = crate::pool::BufferPool::for_tests();
        let mut buffer = pool.acquire().await;

        let result = send_command(&mut stream, "DATE\r\n", &mut buffer).await;
        assert!(result.is_ok(), "send_command should handle partial reads");
        let resp = result.unwrap();
        assert!(
            matches!(resp.response, NntpResponse::Greeting(_)),
            "Expected 200 greeting, got {:?}",
            resp.response
        );
    }

    #[tokio::test]
    async fn test_send_command_single_byte_reads() {
        // Extreme case: each byte comes separately
        let data = b"211 Group\r\n";
        let chunks: Vec<Vec<u8>> = data.iter().map(|&b| vec![b]).collect();
        let mut stream = ChunkedStream::new(chunks);

        let pool = crate::pool::BufferPool::for_tests();
        let mut buffer = pool.acquire().await;

        let result = send_command(&mut stream, "GROUP alt.test\r\n", &mut buffer).await;
        assert!(
            result.is_ok(),
            "send_command should handle single-byte reads"
        );
        let resp = result.unwrap();
        assert!(
            matches!(resp.response, NntpResponse::SingleLine(_)),
            "Expected 211 single-line, got {:?}",
            resp.response
        );
    }

    // ─── Command response tests ─────────────────────────────────────────────

    #[test]
    fn test_command_response_is_430() {
        // Create a 430 response
        let response = CommandResponse {
            bytes_read: 20,
            response: NntpResponse::parse(b"430 No such article\r\n"),
            is_multiline: false,
            warnings: SmallVec::new(),
        };
        assert!(response.response.is_430());

        // Create a 220 response
        let response = CommandResponse {
            bytes_read: 30,
            response: NntpResponse::parse(b"220 0 <msg@example.com>\r\n"),
            is_multiline: true,
            warnings: SmallVec::new(),
        };
        assert!(!response.response.is_430());
    }

    #[test]
    fn test_command_response_status_code() {
        let response = CommandResponse {
            bytes_read: 10,
            response: NntpResponse::parse(b"211 Group\r\n"),
            is_multiline: false,
            warnings: SmallVec::new(),
        };
        assert_eq!(response.status_code().map(|c| c.as_u16()), Some(211));
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

    // ─── Hex preview tests ──────────────────────────────────────────────────

    #[test]
    fn test_format_hex_preview_empty() {
        let data = b"";
        let result = format_hex_preview(data, 256);
        assert_eq!(result, "");
    }

    #[test]
    fn test_format_hex_preview_small() {
        let data = b"ABC";
        let result = format_hex_preview(data, 256);
        // A=41, B=42, C=43
        assert_eq!(result, "41 42 43");
    }

    #[test]
    fn test_format_hex_preview_respects_max_bytes() {
        let data = b"ABCDEFGH";
        let result = format_hex_preview(data, 4);
        // Only first 4 bytes: A=41, B=42, C=43, D=44
        assert_eq!(result, "41 42 43 44");
    }

    #[test]
    fn test_format_hex_preview_full_response() {
        let data = b"430 No such article\r\n";
        let result = format_hex_preview(data, 256);
        // Should show full response in hex
        assert!(result.starts_with("34 33 30 20")); // "430 "
        assert!(result.ends_with("0d 0a")); // \r\n
        assert_eq!(result.split_whitespace().count(), data.len());
    }

    #[test]
    fn test_format_hex_preview_non_ascii() {
        let data = &[0xFF, 0xFE, 0x00, 0x01];
        let result = format_hex_preview(data, 256);
        assert_eq!(result, "ff fe 00 01");
    }

    #[test]
    fn test_format_hex_preview_256_bytes() {
        // Create 300 byte array
        let data: Vec<u8> = (0..=255).chain(0..44).collect();
        assert_eq!(data.len(), 300);

        let result = format_hex_preview(&data, 256);
        // Should only include first 256 bytes
        assert_eq!(result.split_whitespace().count(), 256);

        // Verify last hex is "ff" (byte 255)
        assert!(result.ends_with("ff"));
    }
}
