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

/// Context for handling client write errors during streaming
struct ClientWriteErrorContext<'a> {
    write_len: usize,
    current_n: usize,
    total_bytes: u64,
    partial_data: &'a [u8],
    terminator_found: bool,
    client_addr: crate::types::ClientAddress,
    backend_id: crate::types::BackendId,
    buffer_pool: &'a crate::pool::BufferPool,
}

/// Handle client write error and drain backend if needed
///
/// When a client disconnects mid-stream, we need to drain the remaining
/// response from the backend to keep the connection clean for reuse.
async fn handle_client_write_error<R>(
    error: std::io::Error,
    backend_read: &mut R,
    ctx: ClientWriteErrorContext<'_>,
) -> anyhow::Error
where
    R: AsyncReadExt + Unpin,
{
    // Determine if client received all the data before disconnecting
    let received_complete_chunk = ctx.write_len == ctx.current_n;

    // Log appropriately based on whether disconnect was expected
    if received_complete_chunk {
        debug!(
            "Client {} disconnected after receiving complete data ({} total) → backend {:?}",
            ctx.client_addr,
            crate::formatting::format_bytes(ctx.total_bytes),
            ctx.backend_id
        );
    } else {
        warn!(
            "Client {} disconnected mid-chunk while streaming ({} of {}, total {} so far) → backend {:?}",
            ctx.client_addr,
            crate::formatting::format_bytes(ctx.write_len as u64),
            crate::formatting::format_bytes(ctx.current_n as u64),
            crate::formatting::format_bytes(ctx.total_bytes),
            ctx.backend_id
        );
    }

    // Drain backend if terminator not yet found to keep connection clean
    if !ctx.terminator_found
        && let Err(drain_err) = drain_until_terminator(
            backend_read,
            ctx.partial_data,
            ctx.client_addr,
            ctx.backend_id,
            ctx.buffer_pool,
        )
        .await
    {
        warn!(
            "Client {} failed to drain backend {:?} after disconnect: {}",
            ctx.client_addr, ctx.backend_id, drain_err
        );
    }

    error.into()
}

/// Drain remaining response from backend until terminator is found
///
/// This is called when the client disconnects mid-stream to ensure the backend
/// connection is left in a clean state and can be recycled.
async fn drain_until_terminator<R>(
    backend_read: &mut R,
    initial_tail: &[u8],
    client_addr: crate::types::ClientAddress,
    backend_id: crate::types::BackendId,
    buffer_pool: &crate::pool::BufferPool,
) -> Result<()>
where
    R: AsyncReadExt + Unpin,
{
    let mut chunk = buffer_pool.acquire().await;
    let mut tail = TailBuffer::default();
    tail.update(initial_tail);
    loop {
        let n = chunk
            .read_from(backend_read)
            .await
            .context("Failed to read from backend while draining response")?;
        if n == 0 {
            break; // EOF
        }
        let data = &chunk[..n];
        if tail.detect_terminator(data).is_found() {
            break;
        }
        tail.update(data);
    }
    debug!(
        "Client {} drained remaining response from backend {:?}, connection is clean",
        client_addr, backend_id
    );
    Ok(())
}

/// Stream multiline response from backend to client using pipelined double-buffering
///
/// This uses two buffers to enable concurrent read/write operations for maximum throughput.
/// Essential for large article downloads (50MB+) where buffering would kill performance.
///
/// If `capture` is Some, the response will be captured into the Vec for caching.
pub async fn stream_multiline_response<R, W>(
    backend_read: &mut R,
    client_write: &mut W,
    first_chunk: &[u8],
    first_n: usize,
    client_addr: crate::types::ClientAddress,
    backend_id: crate::types::BackendId,
    buffer_pool: &crate::pool::BufferPool,
) -> Result<u64>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    stream_multiline_response_impl(
        backend_read,
        client_write,
        first_chunk,
        first_n,
        client_addr,
        backend_id,
        buffer_pool,
        None,
        None,
    )
    .await
}

/// Stream multiline response and optionally capture for caching
#[allow(clippy::too_many_arguments)]
pub async fn stream_and_capture_multiline_response<R, W>(
    backend_read: &mut R,
    client_write: &mut W,
    first_chunk: &[u8],
    first_n: usize,
    client_addr: crate::types::ClientAddress,
    backend_id: crate::types::BackendId,
    buffer_pool: &crate::pool::BufferPool,
    capture: &mut crate::pool::PooledBuffer,
) -> Result<u64>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    stream_multiline_response_impl(
        backend_read,
        client_write,
        first_chunk,
        first_n,
        client_addr,
        backend_id,
        buffer_pool,
        Some(capture),
        None,
    )
    .await
}

/// Stream multiline response from backend to client during pipelined batch execution.
///
/// Like `stream_multiline_response`, but captures leftover bytes after the terminator
/// into `leftover` for use as the start of the next response in the pipeline.
#[allow(clippy::too_many_arguments)]
pub async fn stream_multiline_response_pipelined<W>(
    backend_read: &mut crate::stream::ConnectionStream,
    client_write: &mut W,
    first_chunk: &[u8],
    first_n: usize,
    client_addr: crate::types::ClientAddress,
    backend_id: crate::types::BackendId,
    buffer_pool: &crate::pool::BufferPool,
) -> Result<u64>
where
    W: AsyncWriteExt + Unpin,
{
    let mut total_bytes = 0u64;
    let mut tail = TailBuffer::default();

    let data = &first_chunk[..first_n];
    match process_chunk(
        data,
        first_n,
        &mut tail,
        &mut None,
        client_write,
        backend_read,
        &mut total_bytes,
        client_addr,
        backend_id,
        buffer_pool,
    )
    .await?
    {
        ChunkResult::Done { write_len } => {
            if write_len < first_n {
                backend_read.stash_leftover(&first_chunk[write_len..first_n])?;
            }
            debug!(
                "Client {} multiline response complete ({})",
                client_addr,
                crate::formatting::format_bytes(total_bytes)
            );
            return Ok(total_bytes);
        }
        ChunkResult::Continue => {}
    }

    let mut buffers = [buffer_pool.acquire().await, buffer_pool.acquire().await];
    let mut idx: usize = 0;

    loop {
        let n = buffers[idx]
            .read_from(backend_read)
            .await
            .context("Failed to read next chunk from backend")?;

        if n == 0 {
            debug!(
                "Client {} multiline streaming complete ({}, EOF)",
                client_addr,
                crate::formatting::format_bytes(total_bytes)
            );
            break;
        }

        let data = &buffers[idx][..n];
        match process_chunk(
            data,
            n,
            &mut tail,
            &mut None,
            client_write,
            backend_read,
            &mut total_bytes,
            client_addr,
            backend_id,
            buffer_pool,
        )
        .await?
        {
            ChunkResult::Done { write_len } => {
                if write_len < n {
                    backend_read.stash_leftover(&buffers[idx][write_len..n])?;
                }
                debug!(
                    "Client {} multiline response complete ({})",
                    client_addr,
                    crate::formatting::format_bytes(total_bytes)
                );
                return Ok(total_bytes);
            }
            ChunkResult::Continue => {}
        }

        idx ^= 1;
    }

    Ok(total_bytes)
}

/// Stream multiline response from backend to client during pipelined batch execution.
///
/// Like `stream_multiline_response`, but captures leftover bytes after the terminator
/// into `leftover` for use as the start of the next response in the pipeline.
#[cfg(test)]
#[allow(clippy::too_many_arguments, dead_code)]
pub(crate) async fn stream_multiline_response_pipelined_for_test<W>(
    backend_read: &mut crate::stream::ConnectionStream,
    client_write: &mut W,
    first_chunk: &[u8],
    first_n: usize,
    client_addr: crate::types::ClientAddress,
    backend_id: crate::types::BackendId,
    buffer_pool: &crate::pool::BufferPool,
    leftover: &mut bytes::BytesMut,
) -> Result<u64>
where
    W: AsyncWriteExt + Unpin,
{
    stream_multiline_response_impl(
        backend_read,
        client_write,
        first_chunk,
        first_n,
        client_addr,
        backend_id,
        buffer_pool,
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
/// Returns `ChunkResult::Done` if terminator found, or
/// `ChunkResult::Continue` to keep streaming. Total bytes are tracked
/// via the `total_bytes` mutable reference.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
async fn process_chunk<R, W>(
    data: &[u8],
    current_n: usize,
    tail: &mut TailBuffer,
    capture: &mut Option<&mut crate::pool::PooledBuffer>,
    client_write: &mut W,
    backend_read: &mut R,
    total_bytes: &mut u64,
    client_addr: crate::types::ClientAddress,
    backend_id: crate::types::BackendId,
    buffer_pool: &crate::pool::BufferPool,
) -> Result<ChunkResult>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    // Detect terminator location: within chunk or spanning boundary
    let status = tail.detect_terminator(data);
    let write_len = status.write_len(current_n);

    // Capture data if requested (for caching)
    if let Some(cap) = capture {
        cap.extend_from_slice(&data[..write_len]);
    }

    // Write current chunk (or portion up to terminator) to client
    if let Err(e) = client_write.write_all(&data[..write_len]).await {
        return Err(handle_client_write_error(
            e,
            backend_read,
            ClientWriteErrorContext {
                write_len,
                current_n,
                total_bytes: *total_bytes,
                partial_data: &data[..write_len],
                terminator_found: status.is_found(),
                client_addr,
                backend_id,
                buffer_pool,
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
/// Phase 1: Process first_chunk directly (zero-copy, no pooled buffer needed).
/// Phase 2: For multi-chunk responses, acquire two pooled buffers and double-buffer.
#[allow(clippy::too_many_arguments)]
async fn stream_multiline_response_impl<R, W>(
    backend_read: &mut R,
    client_write: &mut W,
    first_chunk: &[u8],
    first_n: usize,
    client_addr: crate::types::ClientAddress,
    backend_id: crate::types::BackendId,
    buffer_pool: &crate::pool::BufferPool,
    mut capture: Option<&mut crate::pool::PooledBuffer>,
    mut leftover_out: Option<&mut bytes::BytesMut>,
) -> Result<u64>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut total_bytes = 0u64;
    let mut tail = TailBuffer::default();

    // Phase 1: Process first chunk directly — no copy into pooled buffer
    let data = &first_chunk[..first_n];
    match process_chunk(
        data,
        first_n,
        &mut tail,
        &mut capture,
        client_write,
        backend_read,
        &mut total_bytes,
        client_addr,
        backend_id,
        buffer_pool,
    )
    .await?
    {
        ChunkResult::Done { write_len } => {
            if let Some(leftover) = leftover_out.as_mut()
                && write_len < first_n
            {
                let remainder = &first_chunk[write_len..first_n];
                anyhow::ensure!(
                    remainder.len() <= crate::constants::buffer::MAX_LEFTOVER_BYTES,
                    "Leftover exceeds maximum ({} bytes)",
                    remainder.len()
                );
                leftover.extend_from_slice(remainder);
            }
            debug!(
                "Client {} multiline response complete ({})",
                client_addr,
                crate::formatting::format_bytes(total_bytes)
            );
            return Ok(total_bytes);
        }
        ChunkResult::Continue => {}
    }

    // Phase 2: Multi-chunk response — acquire pooled buffers for double-buffering
    let mut buffers = [buffer_pool.acquire().await, buffer_pool.acquire().await];
    let mut idx: usize = 0;

    loop {
        let n = buffers[idx]
            .read_from(backend_read)
            .await
            .context("Failed to read next chunk from backend")?;

        if n == 0 {
            debug!(
                "Client {} multiline streaming complete ({}, EOF)",
                client_addr,
                crate::formatting::format_bytes(total_bytes)
            );
            break;
        }

        let data = &buffers[idx][..n];
        match process_chunk(
            data,
            n,
            &mut tail,
            &mut capture,
            client_write,
            backend_read,
            &mut total_bytes,
            client_addr,
            backend_id,
            buffer_pool,
        )
        .await?
        {
            ChunkResult::Done { write_len } => {
                if let Some(leftover) = leftover_out.as_mut()
                    && write_len < n
                {
                    let remainder = &buffers[idx][write_len..n];
                    anyhow::ensure!(
                        remainder.len() <= crate::constants::buffer::MAX_LEFTOVER_BYTES,
                        "Leftover exceeds maximum ({} bytes)",
                        remainder.len()
                    );
                    leftover.extend_from_slice(remainder);
                }
                debug!(
                    "Client {} multiline response complete ({})",
                    client_addr,
                    crate::formatting::format_bytes(total_bytes)
                );
                return Ok(total_bytes);
            }
            ChunkResult::Continue => {}
        }

        idx ^= 1; // Toggle buffer index
    }

    Ok(total_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn test_drain_until_terminator_immediate() {
        use crate::types::BufferSize;
        // Response with terminator already present
        let data = b"220 Article follows\r\nLine 1\r\nLine 2\r\n.\r\n";
        let mut reader = Cursor::new(data);
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let client_addr = crate::types::ClientAddress::from(socket_addr);
        let backend_id = crate::types::BackendId::from_index(1);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2);

        let result =
            drain_until_terminator(&mut reader, b"", client_addr, backend_id, &buffer_pool).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_drain_until_terminator_spanning() {
        use crate::types::BufferSize;
        // Response where terminator spans chunks
        let data = b"220 Article follows\r\nLine 1\r\n.\r\n";
        let mut reader = Cursor::new(data);
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let client_addr = crate::types::ClientAddress::from(socket_addr);
        let backend_id = crate::types::BackendId::from_index(1);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2);

        // Start with tail that could span
        let result =
            drain_until_terminator(&mut reader, b"\r\n", client_addr, backend_id, &buffer_pool)
                .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_drain_until_terminator_eof() {
        use crate::types::BufferSize;
        // Response without terminator (EOF)
        let data = b"220 Article follows\r\nLine 1\r\nLine 2\r\n";
        let mut reader = Cursor::new(data);
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let client_addr = crate::types::ClientAddress::from(socket_addr);
        let backend_id = crate::types::BackendId::from_index(1);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2);

        let result =
            drain_until_terminator(&mut reader, b"", client_addr, backend_id, &buffer_pool).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_stream_multiline_response_simple() {
        use crate::types::BufferSize;
        // Simple multiline response
        let response = b"220 Article follows\r\nLine 1\r\nLine 2\r\n.\r\n";
        let mut reader = Cursor::new(response);
        let mut writer = Vec::new();
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let client_addr = crate::types::ClientAddress::from(socket_addr);
        let backend_id = crate::types::BackendId::from_index(1);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2);

        let result = stream_multiline_response(
            &mut reader,
            &mut writer,
            response,
            response.len(),
            client_addr,
            backend_id,
            &buffer_pool,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), response.len() as u64);
        assert_eq!(&writer[..], response);
    }

    #[tokio::test]
    async fn test_stream_multiline_response_terminator_in_middle() {
        use crate::types::BufferSize;
        // Response with terminator not at end of chunk
        let response = b"220 Article\r\nData\r\n.\r\nExtra";
        let mut reader = Cursor::new(&response[22..]); // Everything after terminator
        let mut writer = Vec::new();
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let client_addr = crate::types::ClientAddress::from(socket_addr);
        let backend_id = crate::types::BackendId::from_index(1);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2);

        // First chunk includes terminator and extra data
        let first_chunk = &response[..27]; // All data including extra
        let result = stream_multiline_response(
            &mut reader,
            &mut writer,
            first_chunk,
            first_chunk.len(),
            client_addr,
            backend_id,
            &buffer_pool,
        )
        .await;

        assert!(result.is_ok());
        // Should only write up to and including terminator (position 22)
        assert_eq!(result.unwrap(), 22);
        assert_eq!(&writer[..], b"220 Article\r\nData\r\n.\r\n");
    }

    #[tokio::test]
    async fn test_stream_multiline_response_empty_body() {
        use crate::types::BufferSize;
        // Response with just status and terminator
        let response = b"220 0 Article follows\r\n.\r\n";
        let mut reader = Cursor::new(b"");
        let mut writer = Vec::new();
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let client_addr = crate::types::ClientAddress::from(socket_addr);
        let backend_id = crate::types::BackendId::from_index(1);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2);

        let result = stream_multiline_response(
            &mut reader,
            &mut writer,
            response,
            response.len(),
            client_addr,
            backend_id,
            &buffer_pool,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), response.len() as u64);
        assert_eq!(&writer[..], response);
    }

    #[tokio::test]
    async fn test_stream_multiline_response_large_article() {
        use crate::types::BufferSize;
        // Simulate a large article that spans multiple chunks
        let header = b"220 Article follows\r\n";
        let mut body = Vec::new();
        for i in 0..1000 {
            body.extend_from_slice(format!("Line {}\r\n", i).as_bytes());
        }
        let terminator = b".\r\n";

        let mut full_response = Vec::new();
        full_response.extend_from_slice(header);
        full_response.extend_from_slice(&body);
        full_response.extend_from_slice(terminator);

        let mut reader = Cursor::new(&full_response[header.len()..]);
        let mut writer = Vec::new();
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let client_addr = crate::types::ClientAddress::from(socket_addr);
        let backend_id = crate::types::BackendId::from_index(1);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2);

        let result = stream_multiline_response(
            &mut reader,
            &mut writer,
            header,
            header.len(),
            client_addr,
            backend_id,
            &buffer_pool,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), full_response.len() as u64);
        assert_eq!(&writer[..], &full_response[..]);
    }

    #[tokio::test]
    async fn test_handle_client_write_error_complete_chunk() {
        use crate::types::BufferSize;
        // Test that client disconnect after complete chunk logs at debug level
        let mut backend = Cursor::new(b"");
        let error = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let client_addr = crate::types::ClientAddress::from(socket_addr);
        let backend_id = crate::types::BackendId::from_index(1);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2);

        let ctx = ClientWriteErrorContext {
            write_len: 100,
            current_n: 100, // Same as write_len = complete chunk
            total_bytes: 100,
            partial_data: b"test",
            terminator_found: true,
            client_addr,
            backend_id,
            buffer_pool: &buffer_pool,
        };

        let result = handle_client_write_error(error, &mut backend, ctx).await;
        // Should return the error
        assert!(result.to_string().contains("broken pipe"));
    }

    #[tokio::test]
    async fn test_handle_client_write_error_incomplete_chunk() {
        use crate::types::BufferSize;
        // Test that client disconnect mid-chunk logs at warn level
        let mut backend = Cursor::new(b"remaining data\r\n.\r\n");
        let error = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let client_addr = crate::types::ClientAddress::from(socket_addr);
        let backend_id = crate::types::BackendId::from_index(1);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2);

        let ctx = ClientWriteErrorContext {
            write_len: 50,
            current_n: 100, // Different from write_len = incomplete
            total_bytes: 500,
            partial_data: b"",
            terminator_found: false, // Need to drain
            client_addr,
            backend_id,
            buffer_pool: &buffer_pool,
        };

        let result = handle_client_write_error(error, &mut backend, ctx).await;
        assert!(result.to_string().contains("broken pipe"));
    }

    #[tokio::test]
    async fn test_handle_client_write_error_with_draining() {
        use crate::types::BufferSize;
        // Test that backend is drained when terminator not found
        let remaining_data = b"more data\r\neven more\r\n.\r\n";
        let mut backend = Cursor::new(remaining_data);
        let error = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "broken pipe");
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let client_addr = crate::types::ClientAddress::from(socket_addr);
        let backend_id = crate::types::BackendId::from_index(1);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2);

        let ctx = ClientWriteErrorContext {
            write_len: 10,
            current_n: 10,
            total_bytes: 10,
            partial_data: b"",
            terminator_found: false,
            client_addr,
            backend_id,
            buffer_pool: &buffer_pool,
        };

        let result = handle_client_write_error(error, &mut backend, ctx).await;
        assert!(result.to_string().contains("broken pipe"));

        // Verify backend was drained (cursor should be at or near end)
        let pos = backend.position();
        assert!(pos >= remaining_data.len() as u64 - 5); // Near end after draining
    }

    // =========================================================================
    // Pipelined streaming leftover tests
    // =========================================================================

    #[tokio::test]
    async fn test_stream_pipelined_leftover_in_first_chunk() {
        use crate::types::BufferSize;
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
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let client_addr = crate::types::ClientAddress::from(socket_addr);
        let backend_id = crate::types::BackendId::from_index(1);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2);

        let result = stream_multiline_response_impl(
            &mut reader,
            &mut writer,
            &combined,
            combined.len(),
            client_addr,
            backend_id,
            &buffer_pool,
            None,
            Some(&mut leftover),
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
        use crate::types::BufferSize;
        // First chunk is just the header (no terminator).
        // Backend read returns terminator + start of next response.
        let first_chunk = b"220 Article follows\r\nLong body content here";
        let second_read = b" more body\r\n.\r\n430 No such article\r\n";

        let mut reader = Cursor::new(second_read.as_slice());
        let mut writer = Vec::new();
        let mut leftover = bytes::BytesMut::new();
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let client_addr = crate::types::ClientAddress::from(socket_addr);
        let backend_id = crate::types::BackendId::from_index(1);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2);

        let result = stream_multiline_response_impl(
            &mut reader,
            &mut writer,
            first_chunk,
            first_chunk.len(),
            client_addr,
            backend_id,
            &buffer_pool,
            None,
            Some(&mut leftover),
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
        use crate::types::BufferSize;
        // Terminator at exact end of chunk — no leftover bytes
        let response = b"220 Article follows\r\nBody\r\n.\r\n";
        let mut reader = Cursor::new(b"" as &[u8]);
        let mut writer = Vec::new();
        let mut leftover = bytes::BytesMut::new();
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let client_addr = crate::types::ClientAddress::from(socket_addr);
        let backend_id = crate::types::BackendId::from_index(1);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2);

        let result = stream_multiline_response_impl(
            &mut reader,
            &mut writer,
            response,
            response.len(),
            client_addr,
            backend_id,
            &buffer_pool,
            None,
            Some(&mut leftover),
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
        use crate::types::BufferSize;

        // Create a response where leftover after terminator exceeds MAX_LEFTOVER_BYTES
        let response = b"220 Article follows\r\nBody\r\n.\r\n";
        let mut combined = Vec::new();
        combined.extend_from_slice(response);
        // Add more than MAX_LEFTOVER_BYTES of extra data after terminator
        combined.extend_from_slice(&vec![b'X'; MAX_LEFTOVER_BYTES + 1]);

        let mut reader = Cursor::new(b"" as &[u8]);
        let mut writer = Vec::new();
        let mut leftover = bytes::BytesMut::new();
        let socket_addr: std::net::SocketAddr = "127.0.0.1:8000".parse().unwrap();
        let client_addr = crate::types::ClientAddress::from(socket_addr);
        let backend_id = crate::types::BackendId::from_index(1);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2);

        let result = stream_multiline_response_impl(
            &mut reader,
            &mut writer,
            &combined,
            combined.len(),
            client_addr,
            backend_id,
            &buffer_pool,
            None,
            Some(&mut leftover),
        )
        .await;

        // Should fail with bounds check error
        assert!(
            result.is_err(),
            "Should error when leftover exceeds max size"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Leftover exceeds") || err.contains("maximum"),
            "Error should mention leftover size limit: {err}"
        );
    }
}
