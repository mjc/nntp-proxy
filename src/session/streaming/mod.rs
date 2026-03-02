//! Client streaming module
//!
//! Handles streaming response data from backend to client.
//! Uses a single pooled buffer for sequential read-write I/O on large transfers.

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

// Public for benchmarks (benches are separate compilation units)
pub mod tail_buffer;
use tail_buffer::TailBuffer;

/// Outcome of a streaming operation that ended in error.
///
/// Callers use `must_remove_connection()` to decide pool fate — no string
/// inspection or downcast needed.
#[derive(Debug)]
pub(crate) enum StreamingError {
    /// Client disconnected; backend was drained and connection is clean.
    /// → Return connection to pool.
    ClientDisconnect(std::io::Error),

    /// Client disconnected AND backend died before terminator during drain.
    /// → Remove connection from pool.
    BackendDirty(anyhow::Error),

    /// Backend closed connection before sending `\r\n.\r\n`.
    /// → Remove connection from pool.
    BackendEof {
        backend_id: crate::types::BackendId,
        bytes_received: u64,
    },

    /// Other I/O / protocol error.
    /// → Remove connection from pool.
    Io(anyhow::Error),
}

impl StreamingError {
    /// True for all variants except `ClientDisconnect`.
    ///
    /// Callers should call `provider.remove_with_cooldown(conn)` when this is true,
    /// and `drop(conn)` (returning to pool) when false.
    pub(crate) const fn must_remove_connection(&self) -> bool {
        !matches!(self, Self::ClientDisconnect(_))
    }
}

impl std::fmt::Display for StreamingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientDisconnect(e) => write!(f, "client disconnected: {e}"),
            Self::BackendDirty(e) => write!(f, "backend dirty after client disconnect: {e}"),
            Self::BackendEof {
                backend_id,
                bytes_received,
            } => write!(
                f,
                "backend {backend_id:?} closed connection before multiline terminator \
                 ({bytes_received} bytes received)"
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StreamingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ClientDisconnect(e) => Some(e),
            Self::BackendDirty(e) | Self::Io(e) => e.source(),
            Self::BackendEof { .. } => None,
        }
    }
}

impl StreamingError {
    /// Convert into an `anyhow::Error` for propagation across API boundaries that
    /// return `anyhow::Result`.
    ///
    /// `ClientDisconnect` unwraps to the bare `io::Error`. At top-level
    /// `anyhow::Result` boundaries, callers can still classify via downcast.
    pub(crate) fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::ClientDisconnect(io_err) => anyhow::Error::from(io_err),
            Self::BackendDirty(e) | Self::Io(e) => e,
            Self::BackendEof {
                backend_id,
                bytes_received,
            } => anyhow::anyhow!(
                "Backend {backend_id:?} closed connection before multiline terminator \
                 ({bytes_received} bytes received)"
            ),
        }
    }
}

/// Classify a client-side write failure into the appropriate `StreamingError` variant.
///
/// Disconnect errors (`BrokenPipe`, `ConnectionReset`) become `ClientDisconnect`
/// (backend is still clean — connection can be returned to pool). All other
/// errors become `Io` (treat the connection as suspect).
pub(crate) fn classify_client_write_err(e: std::io::Error) -> StreamingError {
    use crate::connection_error::is_disconnect_kind;
    if is_disconnect_kind(e.kind()) {
        StreamingError::ClientDisconnect(e)
    } else {
        StreamingError::Io(e.into())
    }
}

/// Identifies where a streaming operation is happening
///
/// Groups the session-level context that is invariant across all chunks
/// of a streaming response, keeping function signatures concise.
pub(crate) struct StreamContext<'a> {
    pub client_addr: crate::types::ClientAddress,
    pub backend_id: crate::types::BackendId,
    pub buffer_pool: &'a crate::pool::BufferPool,
}

/// Context used to drain the backend after a client disconnects mid-stream.
///
/// Carries the state needed by `handle_client_write_error` to seed `TailBuffer`
/// and decide whether a drain is still necessary.
struct DrainContext<'a> {
    write_len: usize,
    chunk_len: usize,
    total_bytes: u64,
    /// The slice already written (or attempted) — used to seed `TailBuffer.update()`
    /// so cross-chunk terminator detection works correctly across the drain boundary.
    tail_data: &'a [u8],
    terminator_found: bool,
    ctx: &'a StreamContext<'a>,
}

/// Handle client write error and drain backend if needed.
///
/// Returns `StreamingError::ClientDisconnect` when the backend was successfully
/// drained (connection is clean and can be returned to pool).
/// Returns `StreamingError::BackendDirty` when the backend died during drain
/// (connection must be removed from pool).
async fn handle_client_write_error<R>(
    error: std::io::Error,
    backend_read: &mut R,
    ctx: DrainContext<'_>,
) -> StreamingError
where
    R: AsyncReadExt + Unpin,
{
    // Determine if client received all the data before disconnecting
    let received_complete_chunk = ctx.write_len == ctx.chunk_len;

    // Log appropriately based on whether disconnect was expected
    if received_complete_chunk {
        debug!(
            "Client {} disconnected after receiving complete data ({} total) → backend {:?}",
            ctx.ctx.client_addr,
            crate::formatting::format_bytes(ctx.total_bytes),
            ctx.ctx.backend_id
        );
    } else {
        warn!(
            "Client {} disconnected mid-chunk while streaming ({} of {}, total {} so far) → backend {:?}",
            ctx.ctx.client_addr,
            crate::formatting::format_bytes(ctx.write_len as u64),
            crate::formatting::format_bytes(ctx.chunk_len as u64),
            crate::formatting::format_bytes(ctx.total_bytes),
            ctx.ctx.backend_id
        );
    }

    // Drain backend if terminator not yet found to keep connection clean.
    // tail_data seeds TailBuffer so cross-chunk boundary detection works correctly.
    if !ctx.terminator_found {
        match drain_until_terminator(backend_read, ctx.tail_data, ctx.ctx).await {
            Ok(()) => {} // Drain succeeded — backend is clean
            Err(drain_err) => {
                warn!(
                    "Client {} failed to drain backend {:?} after disconnect: {}",
                    ctx.ctx.client_addr, ctx.ctx.backend_id, drain_err
                );
                // Return drain error (NOT a client disconnect error) so callers know
                // the backend connection is dirty and must be removed from pool.
                return StreamingError::BackendDirty(
                    drain_err.context("Backend connection dirty after client disconnect"),
                );
            }
        }
    }

    StreamingError::ClientDisconnect(error)
}

/// Drain remaining response from backend until terminator is found.
///
/// This is called when the client disconnects mid-stream to ensure the backend
/// connection is left in a clean state and can be recycled.
///
/// Uses `TailBuffer` for correct cross-chunk terminator detection.
async fn drain_until_terminator<R>(
    backend_read: &mut R,
    initial_tail: &[u8],
    ctx: &StreamContext<'_>,
) -> Result<()>
where
    R: AsyncReadExt + Unpin,
{
    let mut chunk = ctx.buffer_pool.acquire().await;
    let mut tail = TailBuffer::default();
    tail.update(initial_tail);
    loop {
        let n = chunk
            .read_from(backend_read)
            .await
            .context("Failed to read from backend while draining response")?;
        if n == 0 {
            anyhow::bail!("Backend closed connection before multiline terminator during drain");
        }
        let data = &chunk[..n];
        if tail.detect_terminator(data).is_found() {
            break;
        }
        tail.update(data);
    }
    debug!(
        "Client {} drained remaining response from backend {:?}, connection is clean",
        ctx.client_addr, ctx.backend_id
    );
    Ok(())
}

/// Stream multiline response from backend to client using pipelined double-buffering
///
/// This uses two buffers to enable concurrent read/write operations for maximum throughput.
/// Essential for large article downloads (50MB+) where buffering would kill performance.
///
/// If `capture` is Some, the response will be captured into the Vec for caching.
pub(crate) async fn stream_multiline_response<R, W>(
    backend_read: &mut R,
    client_write: &mut W,
    first_chunk: &[u8],
    ctx: &StreamContext<'_>,
) -> Result<u64, StreamingError>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    stream_multiline_response_impl(backend_read, client_write, first_chunk, ctx, None, None).await
}

/// Stream multiline response and optionally capture for caching
pub(crate) async fn stream_and_capture_multiline_response<R, W>(
    backend_read: &mut R,
    client_write: &mut W,
    first_chunk: &[u8],
    ctx: &StreamContext<'_>,
    capture: &mut crate::pool::PooledBuffer,
) -> Result<u64, StreamingError>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    stream_multiline_response_impl(
        backend_read,
        client_write,
        first_chunk,
        ctx,
        Some(capture),
        None,
    )
    .await
}

/// Stream multiline response from backend to client during pipelined batch execution.
///
/// Like `stream_multiline_response`, but captures leftover bytes after the terminator
/// into `leftover` for use as the start of the next response in the pipeline.
pub(crate) async fn stream_multiline_response_pipelined<R, W>(
    backend_read: &mut R,
    client_write: &mut W,
    first_chunk: &[u8],
    ctx: &StreamContext<'_>,
    leftover: &mut bytes::BytesMut,
) -> Result<u64, StreamingError>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    stream_multiline_response_impl(
        backend_read,
        client_write,
        first_chunk,
        ctx,
        None,
        Some(leftover),
    )
    .await
}

/// Result of processing a single chunk in the streaming pipeline
enum ChunkResult {
    /// Terminator found — streaming is complete.
    /// `write_len` is bytes written from this chunk (up to and including terminator).
    Done { write_len: usize },
    /// Chunk processed, continue reading
    Continue,
}

/// Process a single chunk: detect terminator, capture, write to client.
///
/// Uses `TailBuffer` for stateful cross-chunk terminator detection.
/// Returns `ChunkResult::Done` if terminator found, or
/// `ChunkResult::Continue` to keep streaming. Total bytes are tracked
/// via the `total_bytes` mutable reference.
#[inline(always)]
async fn process_chunk<R, W>(
    data: &[u8],
    tail: &mut TailBuffer,
    capture: &mut Option<&mut crate::pool::PooledBuffer>,
    client_write: &mut W,
    backend_read: &mut R,
    total_bytes: &mut u64,
    ctx: &StreamContext<'_>,
) -> Result<ChunkResult, StreamingError>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    // Detect terminator location: within chunk or spanning boundary
    let status = tail.detect_terminator(data);
    let write_len = status.write_len(data.len());

    // Capture data if requested (for caching)
    if let Some(cap) = capture {
        cap.extend_from_slice(&data[..write_len]);
    }

    // Write current chunk (or portion up to terminator) to client
    if let Err(e) = client_write.write_all(&data[..write_len]).await {
        return Err(handle_client_write_error(
            e,
            backend_read,
            DrainContext {
                write_len,
                chunk_len: data.len(),
                total_bytes: *total_bytes,
                tail_data: &data[..write_len],
                terminator_found: status.is_found(),
                ctx,
            },
        )
        .await);
    }
    *total_bytes += write_len as u64;

    if status.is_found() {
        return Ok(ChunkResult::Done { write_len });
    }

    // Update tail for next iteration
    tail.update(&data[..write_len]);
    Ok(ChunkResult::Continue)
}

/// Internal implementation that optionally captures while streaming.
///
/// Phase 1: Process `first_chunk` directly (zero-copy, no pooled buffer needed).
/// Phase 2: For multi-chunk responses, acquire two pooled buffers and double-buffer.
///
/// All terminator detection is delegated to `TailBuffer` — one instance spans
/// the entire response to handle terminators that arrive split across chunk boundaries.
async fn stream_multiline_response_impl<R, W>(
    backend_read: &mut R,
    client_write: &mut W,
    first_chunk: &[u8],
    ctx: &StreamContext<'_>,
    mut capture: Option<&mut crate::pool::PooledBuffer>,
    mut leftover_out: Option<&mut bytes::BytesMut>,
) -> Result<u64, StreamingError>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut total_bytes = 0u64;
    let mut tail = TailBuffer::default();

    // Phase 1: Process first chunk directly — no copy into pooled buffer
    match process_chunk(
        first_chunk,
        &mut tail,
        &mut capture,
        client_write,
        backend_read,
        &mut total_bytes,
        ctx,
    )
    .await?
    {
        ChunkResult::Done { write_len } => {
            if let Some(leftover) = leftover_out.as_mut()
                && write_len < first_chunk.len()
            {
                let remainder = &first_chunk[write_len..];
                if remainder.len() > crate::constants::buffer::MAX_LEFTOVER_BYTES {
                    return Err(StreamingError::Io(anyhow::anyhow!(
                        "Leftover exceeds maximum ({} bytes)",
                        remainder.len()
                    )));
                }
                leftover.extend_from_slice(remainder);
            }
            debug!(
                "Client {} multiline response complete ({})",
                ctx.client_addr,
                crate::formatting::format_bytes(total_bytes)
            );
            return Ok(total_bytes);
        }
        ChunkResult::Continue => {}
    }

    // Phase 2: Multi-chunk response — acquire pooled buffers for double-buffering
    let mut buffers = [
        ctx.buffer_pool.acquire().await,
        ctx.buffer_pool.acquire().await,
    ];
    let mut idx: usize = 0;

    loop {
        let n = buffers[idx].read_from(backend_read).await.map_err(|e| {
            StreamingError::Io(
                anyhow::Error::from(e).context("Failed to read next chunk from backend"),
            )
        })?;

        if n == 0 {
            return Err(StreamingError::BackendEof {
                backend_id: ctx.backend_id,
                bytes_received: total_bytes,
            });
        }

        let data = &buffers[idx][..n];
        match process_chunk(
            data,
            &mut tail,
            &mut capture,
            client_write,
            backend_read,
            &mut total_bytes,
            ctx,
        )
        .await?
        {
            ChunkResult::Done { write_len } => {
                if let Some(leftover) = leftover_out.as_mut()
                    && write_len < n
                {
                    let remainder = &buffers[idx][write_len..n];
                    if remainder.len() > crate::constants::buffer::MAX_LEFTOVER_BYTES {
                        return Err(StreamingError::Io(anyhow::anyhow!(
                            "Leftover exceeds maximum ({} bytes)",
                            remainder.len()
                        )));
                    }
                    leftover.extend_from_slice(remainder);
                }
                debug!(
                    "Client {} multiline response complete ({})",
                    ctx.client_addr,
                    crate::formatting::format_bytes(total_bytes)
                );
                return Ok(total_bytes);
            }
            ChunkResult::Continue => {}
        }

        idx ^= 1; // Toggle buffer index
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[cfg(test)]
    mod test_helpers {
        use super::super::*;
        use crate::types::BufferSize;

        pub(super) fn make_pool() -> crate::pool::BufferPool {
            crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2)
        }

        pub(super) fn make_ctx(pool: &crate::pool::BufferPool) -> StreamContext<'_> {
            let addr = "127.0.0.1:8000".parse::<std::net::SocketAddr>().unwrap();
            StreamContext {
                client_addr: crate::types::ClientAddress::from(addr),
                backend_id: crate::types::BackendId::from_index(1),
                buffer_pool: pool,
            }
        }
    }

    #[tokio::test]
    async fn test_drain_until_terminator_immediate() {
        // Response with terminator already present
        let data = b"220 Article follows\r\nLine 1\r\nLine 2\r\n.\r\n";
        let mut reader = Cursor::new(data);
        let pool = test_helpers::make_pool();
        let ctx = test_helpers::make_ctx(&pool);

        let result = drain_until_terminator(&mut reader, b"", &ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_drain_until_terminator_spanning() {
        // Response where terminator spans chunks
        let data = b"220 Article follows\r\nLine 1\r\n.\r\n";
        let mut reader = Cursor::new(data);
        let pool = test_helpers::make_pool();
        let ctx = test_helpers::make_ctx(&pool);

        // Start with tail that could span
        let result = drain_until_terminator(&mut reader, b"\r\n", &ctx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_drain_until_terminator_eof() {
        // Response without terminator (EOF)
        let data = b"220 Article follows\r\nLine 1\r\nLine 2\r\n";
        let mut reader = Cursor::new(data);
        let pool = test_helpers::make_pool();
        let ctx = test_helpers::make_ctx(&pool);

        let result = drain_until_terminator(&mut reader, b"", &ctx).await;
        assert!(result.is_err(), "EOF before terminator must be an error");
    }

    #[tokio::test]
    async fn test_stream_multiline_response_backend_eof_before_terminator() {
        // Backend sends partial response (no terminator) then closes connection.
        // Phase 1 processes `partial` (no terminator found), Phase 2 reads EOF immediately.
        let partial = b"220 Article follows\r\nIncomplete body\r\n";
        let mut reader = Cursor::new(&[] as &[u8]);
        let mut writer = Vec::new();
        let pool = test_helpers::make_pool();
        let ctx = test_helpers::make_ctx(&pool);

        let result = stream_multiline_response(&mut reader, &mut writer, partial, &ctx).await;
        assert!(
            matches!(result, Err(StreamingError::BackendEof { .. })),
            "EOF before terminator must be BackendEof, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_handle_client_write_error_dirty_drain() {
        // Client disconnects AND backend also dies before sending terminator.
        // drain_until_terminator returns Err (EOF without terminator).
        // handle_client_write_error must return BackendDirty so callers know
        // the backend connection is dirty and must be removed from pool.
        let mut backend = Cursor::new(b"partial data without terminator" as &[u8]);
        let error = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        let pool = test_helpers::make_pool();
        let stream_ctx = test_helpers::make_ctx(&pool);

        let ctx = DrainContext {
            write_len: 10,
            chunk_len: 10,
            total_bytes: 10,
            tail_data: b"",
            terminator_found: false, // Must drain
            ctx: &stream_ctx,
        };

        let result = handle_client_write_error(error, &mut backend, ctx).await;
        assert!(
            matches!(result, StreamingError::BackendDirty(_)),
            "Should return BackendDirty when drain fails, got: {result:?}"
        );
        assert!(
            result.must_remove_connection(),
            "BackendDirty must remove connection"
        );
    }

    #[tokio::test]
    async fn test_stream_multiline_response_simple() {
        // Simple multiline response
        let response = b"220 Article follows\r\nLine 1\r\nLine 2\r\n.\r\n";
        let mut reader = Cursor::new(response);
        let mut writer = Vec::new();
        let pool = test_helpers::make_pool();
        let ctx = test_helpers::make_ctx(&pool);

        let result = stream_multiline_response(&mut reader, &mut writer, response, &ctx).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), response.len() as u64);
        assert_eq!(&writer[..], response);
    }

    #[tokio::test]
    async fn test_stream_multiline_response_terminator_in_middle() {
        // Response with terminator not at end of chunk
        let response = b"220 Article\r\nData\r\n.\r\nExtra";
        let mut reader = Cursor::new(&response[22..]); // Everything after terminator
        let mut writer = Vec::new();
        let pool = test_helpers::make_pool();
        let ctx = test_helpers::make_ctx(&pool);

        // First chunk includes terminator and extra data
        let first_chunk = &response[..27]; // All data including extra
        let result = stream_multiline_response(&mut reader, &mut writer, first_chunk, &ctx).await;

        assert!(result.is_ok());
        // Should only write up to and including terminator (position 22)
        assert_eq!(result.unwrap(), 22);
        assert_eq!(&writer[..], b"220 Article\r\nData\r\n.\r\n");
    }

    #[tokio::test]
    async fn test_stream_multiline_response_empty_body() {
        // Response with just status and terminator
        let response = b"220 0 Article follows\r\n.\r\n";
        let mut reader = Cursor::new(b"");
        let mut writer = Vec::new();
        let pool = test_helpers::make_pool();
        let ctx = test_helpers::make_ctx(&pool);

        let result = stream_multiline_response(&mut reader, &mut writer, response, &ctx).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), response.len() as u64);
        assert_eq!(&writer[..], response);
    }

    #[tokio::test]
    async fn test_stream_multiline_response_large_article() {
        // Simulate a large article that spans multiple chunks
        let header = b"220 Article follows\r\n";
        let mut body = Vec::new();
        for i in 0..1000 {
            body.extend_from_slice(format!("Line {i}\r\n").as_bytes());
        }
        let terminator = b".\r\n";

        let mut full_response = Vec::new();
        full_response.extend_from_slice(header);
        full_response.extend_from_slice(&body);
        full_response.extend_from_slice(terminator);

        let mut reader = Cursor::new(&full_response[header.len()..]);
        let mut writer = Vec::new();
        let pool = test_helpers::make_pool();
        let ctx = test_helpers::make_ctx(&pool);

        let result = stream_multiline_response(&mut reader, &mut writer, header, &ctx).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), full_response.len() as u64);
        assert_eq!(&writer[..], &full_response[..]);
    }

    #[tokio::test]
    async fn test_handle_client_write_error_complete_chunk() {
        // Test that client disconnect after complete chunk returns ClientDisconnect
        let mut backend = Cursor::new(b"");
        let error = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        let pool = test_helpers::make_pool();
        let stream_ctx = test_helpers::make_ctx(&pool);

        let ctx = DrainContext {
            write_len: 100,
            chunk_len: 100, // Same as write_len = complete chunk
            total_bytes: 100,
            tail_data: b"test",
            terminator_found: true,
            ctx: &stream_ctx,
        };

        let result = handle_client_write_error(error, &mut backend, ctx).await;
        assert!(
            matches!(result, StreamingError::ClientDisconnect(_)),
            "Should return ClientDisconnect, got: {result:?}"
        );
        assert!(!result.must_remove_connection());
    }

    #[tokio::test]
    async fn test_handle_client_write_error_incomplete_chunk() {
        // Test that client disconnect mid-chunk with successful drain returns ClientDisconnect
        let mut backend = Cursor::new(b"remaining data\r\n.\r\n");
        let error = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        let pool = test_helpers::make_pool();
        let stream_ctx = test_helpers::make_ctx(&pool);

        let ctx = DrainContext {
            write_len: 50,
            chunk_len: 100, // Different from write_len = incomplete
            total_bytes: 500,
            tail_data: b"",
            terminator_found: false, // Need to drain
            ctx: &stream_ctx,
        };

        let result = handle_client_write_error(error, &mut backend, ctx).await;
        assert!(
            matches!(result, StreamingError::ClientDisconnect(_)),
            "Should return ClientDisconnect after successful drain, got: {result:?}"
        );
        assert!(!result.must_remove_connection());
    }

    #[tokio::test]
    async fn test_handle_client_write_error_with_draining() {
        // Test that backend is drained when terminator not found
        let remaining_data = b"more data\r\neven more\r\n.\r\n";
        let mut backend = Cursor::new(remaining_data);
        let error = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        let pool = test_helpers::make_pool();
        let stream_ctx = test_helpers::make_ctx(&pool);

        let ctx = DrainContext {
            write_len: 10,
            chunk_len: 10,
            total_bytes: 10,
            tail_data: b"",
            terminator_found: false,
            ctx: &stream_ctx,
        };

        let result = handle_client_write_error(error, &mut backend, ctx).await;
        assert!(
            matches!(result, StreamingError::ClientDisconnect(_)),
            "Should return ClientDisconnect after successful drain, got: {result:?}"
        );

        // Verify backend was drained (cursor should be at or near end)
        let pos = backend.position();
        assert!(pos >= remaining_data.len() as u64 - 5); // Near end after draining
    }

    // =========================================================================
    // Pipelined streaming leftover tests
    // =========================================================================

    #[tokio::test]
    async fn test_stream_pipelined_leftover_in_first_chunk() {
        // Two pipelined multiline responses in one buffer.
        // First response ends at terminator, second response starts right after.
        let response1 = b"220 Article follows\r\nBody1\r\n.\r\n";
        let response2_start = b"220 Article follows\r\nBody2";
        let mut combined = Vec::new();
        combined.extend_from_slice(response1);
        combined.extend_from_slice(response2_start);

        // No backend reads needed — everything is in first chunk
        let mut reader = Cursor::new(b"" as &[u8]);
        let mut writer = Vec::new();
        let mut leftover = bytes::BytesMut::new();
        let pool = test_helpers::make_pool();
        let ctx = test_helpers::make_ctx(&pool);

        let result = stream_multiline_response_pipelined(
            &mut reader,
            &mut writer,
            &combined,
            &ctx,
            &mut leftover,
        )
        .await;

        assert!(result.is_ok());
        // Should only write first response (up to and including terminator)
        assert_eq!(result.unwrap(), response1.len() as u64);
        assert_eq!(&writer[..], response1.as_slice());
        // Leftover should contain the start of the second response
        assert_eq!(&leftover[..], response2_start.as_slice());
    }

    #[tokio::test]
    async fn test_stream_pipelined_leftover_in_later_chunk() {
        // First chunk is just the header (no terminator).
        // Backend read returns terminator + start of next response.
        let first_chunk = b"220 Article follows\r\nLong body content here";
        let second_read = b" more body\r\n.\r\n430 No such article\r\n";

        let mut reader = Cursor::new(second_read.as_slice());
        let mut writer = Vec::new();
        let mut leftover = bytes::BytesMut::new();
        let pool = test_helpers::make_pool();
        let ctx = test_helpers::make_ctx(&pool);

        let result = stream_multiline_response_pipelined(
            &mut reader,
            &mut writer,
            first_chunk,
            &ctx,
            &mut leftover,
        )
        .await;

        assert!(result.is_ok());
        // Total bytes = first_chunk + " more body\r\n.\r\n" (15 bytes)
        let expected_body = b" more body\r\n.\r\n";
        let expected_total = first_chunk.len() as u64 + expected_body.len() as u64;
        assert_eq!(result.unwrap(), expected_total);
        // Writer should have first_chunk + body up to terminator
        let mut expected_written = Vec::new();
        expected_written.extend_from_slice(first_chunk);
        expected_written.extend_from_slice(expected_body);
        assert_eq!(&writer[..], &expected_written[..]);
        // Leftover should contain "430 No such article\r\n"
        assert_eq!(&leftover[..], b"430 No such article\r\n");
    }

    #[tokio::test]
    async fn test_stream_pipelined_no_leftover() {
        // Terminator at exact end of chunk — no leftover bytes
        let response = b"220 Article follows\r\nBody\r\n.\r\n";
        let mut reader = Cursor::new(b"" as &[u8]);
        let mut writer = Vec::new();
        let mut leftover = bytes::BytesMut::new();
        let pool = test_helpers::make_pool();
        let ctx = test_helpers::make_ctx(&pool);

        let result = stream_multiline_response_pipelined(
            &mut reader,
            &mut writer,
            response,
            &ctx,
            &mut leftover,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), response.len() as u64);
        assert_eq!(&writer[..], response.as_slice());
        // No leftover — terminator was at exact end
        assert!(leftover.is_empty());
    }

    #[tokio::test]
    async fn test_streaming_leftover_exceeds_max_size() {
        use crate::constants::buffer::MAX_LEFTOVER_BYTES;

        // Create a response where leftover after terminator exceeds MAX_LEFTOVER_BYTES
        let response = b"220 Article follows\r\nBody\r\n.\r\n";
        let mut combined = Vec::new();
        combined.extend_from_slice(response);
        // Add more than MAX_LEFTOVER_BYTES of extra data after terminator
        combined.extend_from_slice(&vec![b'X'; MAX_LEFTOVER_BYTES + 1]);

        let mut reader = Cursor::new(b"" as &[u8]);
        let mut writer = Vec::new();
        let mut leftover = bytes::BytesMut::new();
        let pool = test_helpers::make_pool();
        let ctx = test_helpers::make_ctx(&pool);

        let result = stream_multiline_response_pipelined(
            &mut reader,
            &mut writer,
            &combined,
            &ctx,
            &mut leftover,
        )
        .await;

        // Should fail with Io variant containing the bounds check error
        assert!(
            matches!(result, Err(StreamingError::Io(_))),
            "Should return Io error when leftover exceeds max size, got: {result:?}"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Leftover exceeds") || err.contains("maximum"),
            "Error should mention leftover size limit: {err}"
        );
    }
}
