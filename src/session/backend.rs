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
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::pool::PooledBuffer;
use crate::protocol::RequestContext;

pub(crate) use crate::session::multiline_framing::BackendResponseOrder;

/// Opaque result of reading enough backend bytes to classify one response.
///
/// Callers can inspect status and single-line bytes through methods, but they do
/// not receive framing internals or boundary offsets. Any caller that needs to
/// transfer or capture a body must pass the same buffer back through this module.
pub(crate) struct BackendReadResult {
    inner: BackendReadResultInner,
}

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

enum BackendReadResultInner {
    Response(crate::session::multiline_framing::BackendResponseRead),
    Invalid(crate::session::multiline_framing::ResponseReadError),
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

/// Return the cache-wire terminator used when rendering stored multiline
/// payloads back to a client.
#[must_use]
pub(crate) fn cached_response_completion() -> std::io::IoSlice<'static> {
    crate::session::multiline_framing::cached_response_completion()
}

/// Return the payload body from a response that was already captured as a
/// complete multiline payload.
///
/// This is a facade over the framer-owned payload split logic; cache code should
/// not inspect multiline terminators directly.
#[must_use]
pub(crate) fn captured_multiline_payload_body(payload: &[u8]) -> Option<&[u8]> {
    crate::session::multiline_framing::captured_multiline_payload_body(payload)
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

impl BackendReadResult {
    #[must_use]
    pub(crate) fn status_code(&self) -> Option<crate::protocol::StatusCode> {
        match &self.inner {
            BackendReadResultInner::Response(response) => Some(response.status_code()),
            BackendReadResultInner::Invalid(_) => None,
        }
    }

    #[must_use]
    pub(crate) fn single_line_bytes<'a>(&self, buffer: &'a PooledBuffer) -> Option<&'a [u8]> {
        match &self.inner {
            BackendReadResultInner::Response(response) => response.single_line_bytes(buffer),
            BackendReadResultInner::Invalid(_) => None,
        }
    }

    pub(crate) fn log_warnings(
        &self,
        buffer: &[u8],
        client_addr: impl std::fmt::Display,
        backend_id: crate::types::BackendId,
    ) {
        if let BackendReadResultInner::Invalid(err) = &self.inner {
            err.log_warnings(buffer, client_addr, backend_id);
        }
    }
}

fn duration_micros_u64(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

async fn read_until_backend_reply<C>(
    conn: &mut C,
    request: &RequestContext,
    buffer: &mut PooledBuffer,
) -> Result<BackendReadResult>
where
    C: AsyncReadExt + Unpin,
{
    loop {
        match crate::session::multiline_framing::backend_response_read(request, buffer) {
            Ok(response) => {
                return Ok(BackendReadResult {
                    inner: BackendReadResultInner::Response(response),
                });
            }
            Err(err @ crate::session::multiline_framing::ResponseReadError::Invalid(_)) => {
                return Ok(BackendReadResult {
                    inner: BackendReadResultInner::Invalid(err),
                });
            }
            Err(crate::session::multiline_framing::ResponseReadError::Incomplete) => {
                let more = buffer.read_more(conn).await?;
                if more == 0 {
                    anyhow::bail!(
                        "Backend EOF before complete backend response ({} bytes)",
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

pub(crate) async fn execute_request_classified<C>(
    conn: &mut C,
    request: &RequestContext,
    buffer: &mut PooledBuffer,
) -> Result<BackendReadResult>
where
    C: AsyncReadExt + AsyncWriteExt + Unpin,
{
    request.write_wire_to(conn).await?;

    let n = buffer.read_from(conn).await?;
    if n == 0 {
        anyhow::bail!("Backend connection closed unexpectedly");
    }

    read_until_backend_reply(conn, request, buffer).await
}

pub(crate) async fn execute_request_classified_timed<C>(
    conn: &mut C,
    request: &RequestContext,
    buffer: &mut PooledBuffer,
) -> Result<(BackendReadResult, u64, u64, u64)>
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

    let response = read_until_backend_reply(conn, request, buffer).await?;
    let after_recv = Instant::now();
    let send_elapsed = after_send.duration_since(start);
    let recv_elapsed = after_recv.duration_since(after_send);
    let elapsed = after_recv.duration_since(start);

    Ok((
        response,
        duration_micros_u64(elapsed),
        duration_micros_u64(send_elapsed),
        duration_micros_u64(recv_elapsed),
    ))
}

/// Capture one backend response into owned pooled chunks.
///
/// This is used for payload cache ingestion and other paths that explicitly need
/// ownership of the complete response. It still delegates all response boundary
/// detection to the framer.
pub(crate) async fn write_response_with_optional_capture<W>(
    request: &RequestContext,
    buffer: &mut PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    writer: &mut W,
    captured: &mut crate::pool::ChunkedResponse,
    pool: &crate::pool::BufferPool,
    backend_id: crate::types::BackendId,
) -> Result<(u64, bool), crate::session::response_transfer::ResponseTransferError>
where
    W: AsyncWrite + Unpin,
{
    crate::session::multiline_framing::write_response_with_optional_capture(
        request, buffer, conn, writer, captured, pool, backend_id,
    )
    .await
}

/// Observe and drain one backend response without retaining its bytes.
///
/// This is for retry/control paths that must consume the backend response to
/// keep the connection reusable but do not need ownership of the response body.
pub(crate) async fn observe_response(
    request: &RequestContext,
    buffer: &mut PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    pool: &crate::pool::BufferPool,
    backend_id: crate::types::BackendId,
) -> Result<(), crate::session::response_transfer::ResponseTransferError> {
    crate::session::multiline_framing::observe_response(request, buffer, conn, pool, backend_id)
        .await
}

/// Capture an isolated multiline response into a single pooled capture buffer.
///
/// Used by client and precheck paths that issue commands outside the normal
/// per-command transfer loop.
pub(crate) async fn capture_complete_multiline_response(
    conn: &mut crate::stream::ConnectionStream,
    buffer: &mut PooledBuffer,
    capture: &mut PooledBuffer,
) -> anyhow::Result<()> {
    crate::session::multiline_framing::capture_isolated_multiline_response(conn, buffer, capture)
        .await
}

/// Capture an isolated multiline response into chunked pooled storage while it
/// remains within the framer-owned retention limit.
pub(crate) async fn capture_complete_multiline_response_chunked_optional(
    conn: &mut crate::stream::ConnectionStream,
    buffer: &mut PooledBuffer,
    pool: &crate::pool::BufferPool,
    response: &mut crate::pool::ChunkedResponse,
) -> anyhow::Result<bool> {
    crate::session::multiline_framing::capture_isolated_multiline_response_chunked_optional(
        conn, buffer, pool, response,
    )
    .await
    .map_err(|err| anyhow::anyhow!("backend multiline response capture failed: {err:?}"))
}

/// Drain an isolated multiline response without retaining the bytes.
pub(crate) async fn observe_complete_multiline_response(
    conn: &mut crate::stream::ConnectionStream,
    buffer: &mut PooledBuffer,
) -> anyhow::Result<()> {
    crate::session::multiline_framing::observe_isolated_multiline_response(conn, buffer)
        .await
        .map_err(|err| anyhow::anyhow!("backend multiline response drain failed: {err:?}"))
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
        let response = execute_request_classified(&mut stream, &request, &mut buffer)
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
        let mut buffer = pool.acquire();

        let request = RequestContext::from_verb_args(b"DATE", b"");
        let response = execute_request_classified(&mut stream, &request, &mut buffer)
            .await
            .expect("send_request should read through complete backend response");
        let status_code = response
            .status_code()
            .expect("DATE response should be valid");
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
        let response = execute_request_classified(&mut stream, &request, &mut buffer)
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
