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
use tokio::io::AsyncReadExt;
#[cfg(test)]
use tokio::io::AsyncWriteExt;

use crate::pool::PooledBuffer;
use crate::protocol::RequestContext;

pub(crate) use crate::session::multiline_framing::BackendResponseOrder;

pub(crate) use crate::session::multiline_framing::BackendResponseExchange;
#[cfg(test)]
pub(crate) use crate::session::multiline_framing::ClassifiedResponse;
pub(crate) use crate::session::multiline_framing::Receiving;
pub(crate) use crate::session::multiline_framing::read_exchange_for_already_sent_request;

/// Failure while reading a complete single-line backend reply into caller-owned
/// scratch storage.
#[derive(Debug)]
pub(crate) enum SingleLineReplyReadError {
    /// The scratch buffer filled before the framer accepted a complete reply.
    Full { bytes_read: usize },
    /// The underlying connection returned an I/O error.
    Io(std::io::Error),
    /// The backend closed before a complete reply was accepted.
    Closed,
    /// The bytes read so far cannot be a valid reply for the request.
    Invalid { bytes_read: usize },
}

pub(crate) async fn read_single_line_reply<C>(
    conn: &mut C,
    request: &RequestContext,
    scratch: &mut [u8],
) -> Result<String, SingleLineReplyReadError>
where
    C: tokio::io::AsyncRead + Unpin,
{
    let mut total = 0usize;

    loop {
        if total == scratch.len() {
            return Err(SingleLineReplyReadError::Full { bytes_read: total });
        }

        let n = conn
            .read(&mut scratch[total..])
            .await
            .map_err(SingleLineReplyReadError::Io)?;

        if n == 0 {
            return Err(SingleLineReplyReadError::Closed);
        }

        total += n;

        match crate::session::multiline_framing::unpacked_single_line_response(
            request,
            &scratch[..total],
        ) {
            Ok(bytes) => {
                return Ok(String::from_utf8_lossy(bytes).into_owned());
            }
            Err(crate::session::multiline_framing::ResponseReadError::Incomplete) => {}
            Err(crate::session::multiline_framing::ResponseReadError::Invalid(_)) => {
                return Err(SingleLineReplyReadError::Invalid { bytes_read: total });
            }
        }
    }
}

/// Capabilities response for the local proxy capability command.
#[must_use]
pub(crate) const fn capabilities_response(auth_enabled: bool) -> &'static [u8] {
    if auth_enabled {
        crate::session::multiline_framing::CAPABILITIES_WITH_AUTHINFO_RESPONSE
    } else {
        crate::session::multiline_framing::CAPABILITIES_WITHOUT_AUTHINFO_RESPONSE
    }
}

/// Capabilities response used after authentication has already been satisfied.
#[must_use]
pub(crate) const fn capabilities_without_authinfo_response() -> &'static [u8] {
    crate::session::multiline_framing::CAPABILITIES_WITHOUT_AUTHINFO_RESPONSE
}

fn duration_micros_u64(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
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
/// # use nntp_proxy::session::format_hex_preview;
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

#[cfg(test)]
async fn execute_request_classified<C>(
    conn: &mut C,
    request: &RequestContext,
    mut buffer: PooledBuffer,
) -> Result<ClassifiedResponse>
where
    C: AsyncReadExt + AsyncWriteExt + Unpin,
{
    request.write_wire_to(conn).await?;

    let n = buffer.read_from(conn).await?;
    if n == 0 {
        anyhow::bail!("Backend connection closed unexpectedly");
    }

    ClassifiedResponse::read_for_test(conn, request, buffer).await
}

/// Execute a direct request and return the response bound to the connection
/// that supplied it.  The exchange owns both values until a consuming
/// forwarding, capture, or observation operation is selected.
pub(crate) async fn execute_request_exchange<'pool>(
    mut conn: crate::pool::ConnectionGuard,
    request: &RequestContext,
    mut buffer: PooledBuffer,
    pool: &'pool crate::pool::BufferPool,
    backend_id: crate::types::BackendId,
) -> Result<BackendResponseExchange<'pool>> {
    request.write_wire_to(conn.stream_mut()).await?;

    let n = buffer.read_from(conn.stream_mut()).await?;
    if n == 0 {
        anyhow::bail!("Backend connection closed unexpectedly");
    }

    BackendResponseExchange::read(conn, request, buffer, pool, backend_id).await
}

/// Timed variant of [`execute_request_exchange`] used by the direct backend
/// attempt sampler.
pub(crate) async fn execute_request_exchange_timed<'pool>(
    mut conn: crate::pool::ConnectionGuard,
    request: &RequestContext,
    mut buffer: PooledBuffer,
    pool: &'pool crate::pool::BufferPool,
    backend_id: crate::types::BackendId,
) -> Result<(BackendResponseExchange<'pool>, u64, u64, u64)> {
    use std::time::Instant;

    let start = Instant::now();
    request.write_wire_to(conn.stream_mut()).await?;
    let after_send = Instant::now();

    let n = buffer.read_from(conn.stream_mut()).await?;
    if n == 0 {
        anyhow::bail!("Backend connection closed unexpectedly");
    }

    let exchange = BackendResponseExchange::read(conn, request, buffer, pool, backend_id).await?;
    let after_recv = Instant::now();
    Ok((
        exchange,
        duration_micros_u64(after_recv.duration_since(start)),
        duration_micros_u64(after_send.duration_since(start)),
        duration_micros_u64(after_recv.duration_since(after_send)),
    ))
}

#[cfg(response_contract = "exchange_constructor")]
#[allow(dead_code)]
fn exchange_constructor_is_not_a_caller_api() {
    let _constructor = crate::session::multiline_framing::BackendResponseExchange::new;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::StatusCode;
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
        let buffer = pool.acquire();

        let request = RequestContext::from_verb_args(b"DATE", b"");
        let response = execute_request_classified(&mut stream, &request, buffer)
            .await
            .expect("send_request should handle partial reads");
        let status_code = response
            .status_code()
            .expect("DATE response should be valid");
        assert_eq!(status_code, StatusCode::new(200));
        assert!(!request.has_response_body(status_code));
    }

    #[tokio::test]
    async fn test_send_request_reads_complete_initial_reply_when_code_arrives_first() {
        // RFC-compliant servers can split a single-line response across TCP reads.
        // Reading only the 3-byte status code would leave the rest of the line
        // in the socket for the next command and desynchronize the connection.
        let mut stream = ChunkedStream::new(vec![b"111".to_vec(), b" 20260501173336\r\n".to_vec()]);

        let pool = crate::pool::BufferPool::for_tests();
        let buffer = pool.acquire();

        let request = RequestContext::from_verb_args(b"DATE", b"");
        let response = execute_request_classified(&mut stream, &request, buffer)
            .await
            .expect("send_request should read through complete backend response");
        let status_code = response
            .status_code()
            .expect("DATE response should be valid");
        assert_eq!(status_code, StatusCode::new(111));
        assert!(!request.has_response_body(status_code));
        assert_eq!(response.received_len(), b"111 20260501173336\r\n".len());
    }

    #[tokio::test]
    async fn test_send_request_single_byte_reads() {
        // Extreme case: each byte comes separately
        let data = b"211 Group\r\n";
        let chunks: Vec<Vec<u8>> = data.iter().map(|&b| vec![b]).collect();
        let mut stream = ChunkedStream::new(chunks);

        let pool = crate::pool::BufferPool::for_tests();
        let buffer = pool.acquire();

        let request = RequestContext::from_verb_args(b"GROUP", b"alt.test");
        let response = execute_request_classified(&mut stream, &request, buffer)
            .await
            .expect("send_request should handle single-byte reads");
        let status_code = response
            .status_code()
            .expect("GROUP response should be valid");
        assert_eq!(status_code, StatusCode::new(211));
        assert!(!request.has_response_body(status_code));
    }

    #[tokio::test]
    async fn read_single_line_reply_uses_caller_scratch_for_split_reply() {
        let mut stream = ChunkedStream::new(vec![b"111 20260501".to_vec(), b"173336\r\n".to_vec()]);
        let request = RequestContext::from_verb_args(b"DATE", b"");
        let mut scratch = [0u8; 32];

        let response = read_single_line_reply(&mut stream, &request, &mut scratch)
            .await
            .expect("split single-line reply should complete");

        assert_eq!(response, "111 20260501173336\r\n");
        assert_eq!(&scratch[..response.len()], response.as_bytes());
    }

    #[tokio::test]
    async fn read_single_line_reply_reports_full_scratch_with_bytes_read() {
        let mut stream = ChunkedStream::new(vec![b"111 ".to_vec()]);
        let request = RequestContext::from_verb_args(b"DATE", b"");
        let mut scratch = [0u8; 4];

        let err = read_single_line_reply(&mut stream, &request, &mut scratch)
            .await
            .expect_err("unterminated reply should fill scratch");

        assert!(matches!(
            err,
            SingleLineReplyReadError::Full { bytes_read: 4 }
        ));
        assert_eq!(&scratch, b"111 ");
    }

    #[tokio::test]
    async fn read_single_line_reply_reports_invalid_bytes_read() {
        let mut stream = ChunkedStream::new(vec![b"abc\r\n".to_vec()]);
        let request = RequestContext::from_verb_args(b"DATE", b"");
        let mut scratch = [0u8; 16];

        let err = read_single_line_reply(&mut stream, &request, &mut scratch)
            .await
            .expect_err("nonnumeric reply should be invalid");

        assert!(matches!(
            err,
            SingleLineReplyReadError::Invalid { bytes_read: 5 }
        ));
        assert_eq!(&scratch[..5], b"abc\r\n");
    }

    #[tokio::test]
    async fn read_single_line_reply_rejects_packed_trailing_bytes() {
        let packed_reply = b"111 20260501173336\r\n222 next\r\n";
        let mut stream = ChunkedStream::new(vec![packed_reply.to_vec()]);
        let request = RequestContext::from_verb_args(b"DATE", b"");
        let mut scratch = [0u8; 64];

        let err = read_single_line_reply(&mut stream, &request, &mut scratch)
            .await
            .expect_err("packed setup reply must not wait for more bytes");

        assert!(matches!(
            err,
            SingleLineReplyReadError::Invalid { bytes_read } if bytes_read == packed_reply.len()
        ));
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
    fn test_format_hex_preview_response_bytes() {
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
