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
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::pool::PooledBuffer;
use crate::protocol::RequestContext;

fn duration_micros_u64(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

async fn read_until_response_frame<C>(
    conn: &mut C,
    request: &RequestContext,
    buffer: &mut PooledBuffer,
) -> Result<()>
where
    C: AsyncReadExt + Unpin,
{
    loop {
        match crate::session::multiline_framing::response_status(request, buffer) {
            Ok(_) | Err(crate::session::multiline_framing::ResponseReadError::Invalid(_)) => {
                return Ok(());
            }
            Err(crate::session::multiline_framing::ResponseReadError::Incomplete) => {
                let more = buffer.read_more(conn).await?;
                if more == 0 {
                    anyhow::bail!(
                        "Backend EOF before complete response frame ({} bytes)",
                        buffer.initialized()
                    );
                }
            }
        }
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

/// Send a typed request and read the first response chunk.
pub async fn send_request<C>(
    conn: &mut C,
    request: &RequestContext,
    buffer: &mut PooledBuffer,
) -> Result<()>
where
    C: AsyncReadExt + AsyncWriteExt + Unpin,
{
    request.write_wire_to(conn).await?;

    let n = buffer.read_from(conn).await?;
    if n == 0 {
        anyhow::bail!("Backend connection closed unexpectedly");
    }

    read_until_response_frame(conn, request, buffer).await?;
    Ok(())
}

/// Send a typed request with timing measurements.
pub async fn send_request_timed<C>(
    conn: &mut C,
    request: &RequestContext,
    buffer: &mut PooledBuffer,
) -> Result<(u64, u64, u64)>
where
    C: AsyncReadExt + AsyncWriteExt + Unpin,
{
    use std::time::Instant;

    let start = Instant::now();
    request.write_wire_to(conn).await?;
    let after_send = Instant::now();

    let n = buffer.read_from(conn).await?;
    if n == 0 {
        anyhow::bail!("Backend connection closed unexpectedly");
    }

    read_until_response_frame(conn, request, buffer).await?;
    let after_recv = Instant::now();
    let send_elapsed = after_send.duration_since(start);
    let recv_elapsed = after_recv.duration_since(after_send);
    let elapsed = after_recv.duration_since(start);

    Ok((
        duration_micros_u64(elapsed),
        duration_micros_u64(send_elapsed),
        duration_micros_u64(recv_elapsed),
    ))
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
        let mut buffer = pool.acquire();

        let request = RequestContext::from_verb_args(b"DATE", b"");
        let result = send_request(&mut stream, &request, &mut buffer).await;
        assert!(result.is_ok(), "send_request should handle partial reads");
        let status_code =
            crate::session::multiline_framing::response_status(&request, &buffer).unwrap();
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
        let mut buffer = pool.acquire();

        let request = RequestContext::from_verb_args(b"DATE", b"");
        let result = send_request(&mut stream, &request, &mut buffer).await;
        assert!(
            result.is_ok(),
            "send_request should read through complete response frame"
        );
        let status_code =
            crate::session::multiline_framing::response_status(&request, &buffer).unwrap();
        assert_eq!(status_code, StatusCode::new(111));
        assert!(!request.has_response_body(status_code));
        assert_eq!(&buffer[..buffer.initialized()], b"111 20260501173336\r\n");
    }

    #[tokio::test]
    async fn test_send_request_single_byte_reads() {
        // Extreme case: each byte comes separately
        let data = b"211 Group\r\n";
        let chunks: Vec<Vec<u8>> = data.iter().map(|&b| vec![b]).collect();
        let mut stream = ChunkedStream::new(chunks);

        let pool = crate::pool::BufferPool::for_tests();
        let mut buffer = pool.acquire();

        let request = RequestContext::from_verb_args(b"GROUP", b"alt.test");
        let result = send_request(&mut stream, &request, &mut buffer).await;
        assert!(
            result.is_ok(),
            "send_request should handle single-byte reads"
        );
        let status_code =
            crate::session::multiline_framing::response_status(&request, &buffer).unwrap();
        assert_eq!(status_code, StatusCode::new(211));
        assert!(!request.has_response_body(status_code));
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
