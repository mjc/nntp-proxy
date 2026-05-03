//! Backend communication module
//!
//! This module handles NNTP client operations when proxying to backend servers.
//! The proxy acts as an NNTP client to upstream servers.
//!
//! # Structure
//!
//! - Response status parsing functions (pure, easily testable)
//! - Command execution helpers - Send command, read response
//!
//! Backend requests and response parsing are driven by `RequestContext`; callers
//! should not rebuild command strings after request validation.

use anyhow::Result;
use smallvec::SmallVec;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::pool::PooledBuffer;
use crate::protocol::{RequestContext, StatusCode};

// ─── Response status parsing ────────────────────────────────────────────────

/// Response status parse warnings (pure data)
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResponseWarning {
    /// Response too short to be valid
    ShortResponse { bytes: usize, min: usize },
    /// Response code is invalid
    InvalidResponse,
    /// Response code is unusual (0 or >= 600)
    UnusualStatusCode(u16),
}

/// Parsed backend response status (pure data)
#[derive(Debug)]
pub(crate) struct BackendStatusParse {
    pub(crate) status_code: Option<StatusCode>,
    pub(crate) warnings: SmallVec<[ResponseWarning; 0]>,
}

/// Parse backend response status (pure function - easily testable)
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
/// `BackendStatusParse` with parsed status and any warnings
#[must_use]
pub(crate) fn parse_backend_status(
    chunk: &[u8],
    bytes_read: usize,
    min_length: usize,
) -> BackendStatusParse {
    let mut warnings = SmallVec::new();

    // Check minimum length
    if bytes_read < min_length {
        warnings.push(ResponseWarning::ShortResponse {
            bytes: bytes_read,
            min: min_length,
        });
    }

    let status_code = StatusCode::parse(&chunk[..bytes_read]);
    // Check for invalid response
    if let Some(code) = status_code {
        // Check for unusual status codes
        let raw_code = code.as_u16();
        if raw_code == 0 || raw_code >= 600 {
            warnings.push(ResponseWarning::UnusualStatusCode(raw_code));
        }
    } else {
        warnings.push(ResponseWarning::InvalidResponse);
    }

    BackendStatusParse {
        status_code,
        warnings,
    }
}

/// Return the byte offset immediately after the response status line.
///
/// NNTP status lines are CRLF-terminated. We also treat a bare LF as a line
/// boundary here so callers do not classify a partial status line as complete.
#[must_use]
pub(crate) fn status_line_end(data: &[u8]) -> Option<usize> {
    memchr::memchr(b'\n', data).map(|pos| pos + 1)
}

async fn read_until_status_line<C>(conn: &mut C, buffer: &mut PooledBuffer) -> Result<()>
where
    C: AsyncReadExt + Unpin,
{
    while status_line_end(buffer).is_none() {
        let more = buffer.read_more(conn).await?;
        if more == 0 {
            anyhow::bail!(
                "Backend EOF before complete status line ({} bytes)",
                buffer.initialized()
            );
        }
    }
    Ok(())
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
pub(crate) fn format_hex_preview(data: &[u8], max_bytes: usize) -> String {
    let preview = &data[..data.len().min(max_bytes)];
    preview
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

// ─── Command execution ──────────────────────────────────────────────────────

/// Metadata for the first backend response chunk.
#[derive(Debug)]
pub(crate) struct BackendFirstResponse {
    /// Number of bytes read into buffer
    pub(crate) bytes_read: usize,
    /// Parsed status code, if present
    pub(crate) status_code: Option<StatusCode>,
    /// Any validation warnings
    pub(crate) warnings: SmallVec<[ResponseWarning; 0]>,
}

impl BackendFirstResponse {
    /// Get status code if valid
    #[inline]
    #[must_use]
    pub(crate) const fn status_code(&self) -> Option<StatusCode> {
        self.status_code
    }

    /// Log validation warnings with context
    pub(crate) fn log_warnings(
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

/// Write a typed request to a backend without building a temporary command buffer.
pub(crate) async fn write_request<C>(conn: &mut C, request: &RequestContext) -> std::io::Result<()>
where
    C: tokio::io::AsyncWrite + Unpin,
{
    request.write_wire_to(conn).await
}

/// Send a typed request and read the first response chunk.
pub(crate) async fn send_request<C>(
    conn: &mut C,
    request: &RequestContext,
    buffer: &mut PooledBuffer,
) -> Result<BackendFirstResponse>
where
    C: AsyncReadExt + AsyncWriteExt + Unpin,
{
    write_request(conn, request).await?;

    let n = buffer.read_from(conn).await?;
    if n == 0 {
        anyhow::bail!("Backend connection closed unexpectedly");
    }

    read_until_status_line(conn, buffer).await?;
    let min_len = crate::protocol::MIN_RESPONSE_LENGTH;
    let total = buffer.initialized();
    let validated = parse_backend_status(&buffer[..total], total, min_len);

    Ok(BackendFirstResponse {
        bytes_read: total,
        status_code: validated.status_code,
        warnings: validated.warnings,
    })
}

/// Send a typed request with timing measurements.
pub(crate) async fn send_request_timed<C>(
    conn: &mut C,
    request: &RequestContext,
    buffer: &mut PooledBuffer,
) -> Result<(BackendFirstResponse, u64, u64, u64)>
where
    C: AsyncReadExt + AsyncWriteExt + Unpin,
{
    use std::time::Instant;

    let start = Instant::now();
    write_request(conn, request).await?;
    let after_send = Instant::now();

    let n = buffer.read_from(conn).await?;
    if n == 0 {
        anyhow::bail!("Backend connection closed unexpectedly");
    }

    read_until_status_line(conn, buffer).await?;
    let min_len = crate::protocol::MIN_RESPONSE_LENGTH;
    let total = buffer.initialized();
    let after_recv = Instant::now();
    let send_elapsed = after_send.duration_since(start);
    let recv_elapsed = after_recv.duration_since(after_send);
    let elapsed = after_recv.duration_since(start);

    let validated = parse_backend_status(&buffer[..total], total, min_len);

    Ok((
        BackendFirstResponse {
            bytes_read: total,
            status_code: validated.status_code,
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
    use proptest::prelude::*;
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
            }
            Poll::Ready(Ok(()))
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
    async fn test_send_request_partial_read_accumulates() {
        // Simulate a backend that sends "200 OK\r\n" in two chunks:
        // first read returns "20", second returns "0 OK\r\n"
        let mut stream = ChunkedStream::new(vec![b"20".to_vec(), b"0 OK\r\n".to_vec()]);

        let pool = crate::pool::BufferPool::for_tests();
        let mut buffer = pool.acquire().await;

        let request = RequestContext::from_verb_args(b"DATE", b"");
        let result = send_request(&mut stream, &request, &mut buffer).await;
        assert!(result.is_ok(), "send_request should handle partial reads");
        let resp = result.unwrap();
        assert_eq!(resp.status_code(), Some(StatusCode::new(200)));
        assert!(
            !request
                .response_body_kind(resp.status_code().unwrap())
                .is_multiline()
        );
    }

    #[tokio::test]
    async fn test_send_request_reads_complete_status_line_when_code_arrives_first() {
        // RFC-compliant servers can split a single-line response across TCP reads.
        // Reading only the 3-byte status code would leave the rest of the line
        // in the socket for the next command and desynchronize the connection.
        let mut stream = ChunkedStream::new(vec![b"111".to_vec(), b" 20260501173336\r\n".to_vec()]);

        let pool = crate::pool::BufferPool::for_tests();
        let mut buffer = pool.acquire().await;

        let request = RequestContext::from_verb_args(b"DATE", b"");
        let result = send_request(&mut stream, &request, &mut buffer).await;
        assert!(
            result.is_ok(),
            "send_request should read through status-line CRLF"
        );
        let resp = result.unwrap();
        assert_eq!(resp.bytes_read, b"111 20260501173336\r\n".len());
        assert_eq!(resp.status_code(), Some(StatusCode::new(111)));
        assert!(
            !request
                .response_body_kind(resp.status_code().unwrap())
                .is_multiline()
        );
        assert_eq!(&buffer[..resp.bytes_read], b"111 20260501173336\r\n");
    }

    #[tokio::test]
    async fn test_send_request_single_byte_reads() {
        // Extreme case: each byte comes separately
        let data = b"211 Group\r\n";
        let chunks: Vec<Vec<u8>> = data.iter().map(|&b| vec![b]).collect();
        let mut stream = ChunkedStream::new(chunks);

        let pool = crate::pool::BufferPool::for_tests();
        let mut buffer = pool.acquire().await;

        let request = RequestContext::from_verb_args(b"GROUP", b"alt.test");
        let result = send_request(&mut stream, &request, &mut buffer).await;
        assert!(
            result.is_ok(),
            "send_request should handle single-byte reads"
        );
        let resp = result.unwrap();
        assert_eq!(resp.status_code(), Some(StatusCode::new(211)));
        assert!(
            !request
                .response_body_kind(resp.status_code().unwrap())
                .is_multiline()
        );
    }

    // ─── Command response tests ─────────────────────────────────────────────

    #[test]
    fn test_backend_first_response_is_430() {
        // Create a 430 response
        let response = BackendFirstResponse {
            bytes_read: 20,
            status_code: Some(StatusCode::new(430)),
            warnings: SmallVec::new(),
        };
        assert_eq!(response.status_code(), Some(StatusCode::new(430)));

        // Create a 220 response
        let response = BackendFirstResponse {
            bytes_read: 30,
            status_code: Some(StatusCode::new(220)),
            warnings: SmallVec::new(),
        };
        assert_ne!(response.status_code(), Some(StatusCode::new(430)));
    }

    #[test]
    fn test_backend_first_response_status_code() {
        let response = BackendFirstResponse {
            bytes_read: 10,
            status_code: Some(StatusCode::new(211)),
            warnings: SmallVec::new(),
        };
        assert_eq!(response.status_code().map(|c| c.as_u16()), Some(211));
    }

    // ─── Validation tests ───────────────────────────────────────────────────

    #[test]
    fn test_parse_backend_status_valid() {
        let data = b"200 OK\r\n";
        let validated = parse_backend_status(data, data.len(), 7);

        assert_eq!(
            validated.status_code,
            Some(crate::protocol::StatusCode::new(200))
        );
        assert!(validated.warnings.is_empty());
    }

    #[test]
    fn test_parse_backend_status_short() {
        let data = b"200";
        let validated = parse_backend_status(data, data.len(), 7);

        assert_eq!(validated.warnings.len(), 1);
        assert_eq!(
            validated.warnings[0],
            ResponseWarning::ShortResponse { bytes: 3, min: 7 }
        );
    }

    #[test]
    fn test_parse_backend_status_invalid() {
        let data = b"garbage response";
        let validated = parse_backend_status(data, data.len(), 7);

        assert_eq!(validated.status_code, None);
        assert!(
            validated
                .warnings
                .contains(&ResponseWarning::InvalidResponse)
        );
    }

    #[test]
    fn test_parse_backend_status_unusual_status_zero() {
        let data = b"000 Weird\r\n";
        let validated = parse_backend_status(data, data.len(), 7);

        assert!(
            validated
                .warnings
                .contains(&ResponseWarning::UnusualStatusCode(0))
        );
    }

    #[test]
    fn test_parse_backend_status_unusual_status_high() {
        let data = b"700 Too High\r\n";
        let validated = parse_backend_status(data, data.len(), 7);

        assert!(
            validated
                .warnings
                .contains(&ResponseWarning::UnusualStatusCode(700))
        );
    }

    #[test]
    fn test_parse_backend_status_multiple_warnings() {
        let data = b"000";
        let validated = parse_backend_status(data, data.len(), 7);

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
    fn test_parse_backend_status_normal_codes() {
        // Test various normal response codes don't generate warnings
        let test_cases = vec![
            b"200 OK\r\n".as_ref(),
            b"211 Group info\r\n".as_ref(),
            b"480 Auth required\r\n".as_ref(),
        ];

        for data in test_cases {
            let validated = parse_backend_status(data, data.len(), 7);
            assert!(validated.status_code.is_some());
            assert!(
                validated
                    .warnings
                    .iter()
                    .all(|w| !matches!(w, ResponseWarning::UnusualStatusCode(_))),
                "Normal status codes should not generate unusual code warnings"
            );
        }
    }

    proptest! {
        #[test]
        fn prop_parse_backend_status_never_panics(
            data in prop::collection::vec(any::<u8>(), 0..200)
        ) {
            let _ = parse_backend_status(&data, data.len(), crate::protocol::MIN_RESPONSE_LENGTH);
        }

        #[test]
        fn prop_valid_nntp_response_never_classified_invalid(
            code in 100u16..=599u16,
            text in r"[A-Za-z0-9 ]{1,30}"
        ) {
            let response = format!("{code} {text}\r\n");
            let data = response.as_bytes();
            let parsed = parse_backend_status(data, data.len(), crate::protocol::MIN_RESPONSE_LENGTH);

            prop_assert_eq!(
                parsed.status_code.map(|status| status.as_u16()),
                Some(code),
                "parsed status code should match generated code"
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
        // Should show every byte in the preview
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
