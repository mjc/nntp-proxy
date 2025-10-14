//! Client streaming module
//!
//! Handles streaming response data from backend to client with pipelined I/O.
//! Uses double-buffering for optimal performance on large transfers.

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

use crate::constants::buffer::STREAMING_CHUNK_SIZE;

mod tail_buffer;
use tail_buffer::TailBuffer;

/// Swap between two buffer indices (0 <-> 1)
#[inline]
const fn alternate_buffer_index(current: usize) -> usize {
    1 - current
}

/// Context for handling client write errors during streaming
struct ClientWriteErrorContext<'a> {
    write_len: usize,
    current_n: usize,
    total_bytes: u64,
    partial_data: &'a [u8],
    terminator_found: bool,
    client_addr: std::net::SocketAddr,
    backend_id: crate::types::BackendId,
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
    warn!(
        "Client {} disconnected while streaming ({} of {}, total {} so far) → backend {:?}",
        ctx.client_addr,
        crate::formatting::format_bytes(ctx.write_len as u64),
        crate::formatting::format_bytes(ctx.current_n as u64),
        crate::formatting::format_bytes(ctx.total_bytes),
        ctx.backend_id
    );
    if !ctx.terminator_found {
        // Need to drain the rest to keep connection clean
        if let Err(drain_err) = drain_until_terminator(
            backend_read,
            ctx.partial_data,
            ctx.client_addr,
            ctx.backend_id,
        )
        .await
        {
            warn!(
                "Client {} failed to drain backend {:?} after disconnect: {}",
                ctx.client_addr, ctx.backend_id, drain_err
            );
        }
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
    client_addr: std::net::SocketAddr,
    backend_id: crate::types::BackendId,
) -> Result<()>
where
    R: AsyncReadExt + Unpin,
{
    let mut chunk = vec![0u8; STREAMING_CHUNK_SIZE].into_boxed_slice();
    let mut tail = TailBuffer::new();
    tail.update(initial_tail);
    loop {
        let n = backend_read
            .read(&mut chunk)
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
pub async fn stream_multiline_response<R, W>(
    backend_read: &mut R,
    client_write: &mut W,
    first_chunk: &[u8],
    first_n: usize,
    client_addr: std::net::SocketAddr,
    backend_id: crate::types::BackendId,
) -> Result<u64>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut total_bytes = 0u64;
    // Prepare double buffering for pipelined streaming
    let mut buffers = [
        vec![0u8; STREAMING_CHUNK_SIZE].into_boxed_slice(),
        vec![0u8; STREAMING_CHUNK_SIZE].into_boxed_slice(),
    ];
    let mut current_idx = 0;
    // Copy first chunk into buffer
    buffers[0][..first_n].copy_from_slice(&first_chunk[..first_n]);
    let mut current_n = first_n;
    // Track tail for spanning terminator detection
    let mut tail = TailBuffer::new();
    // Main streaming loop - processes first chunk and all subsequent chunks uniformly
    loop {
        let data = &buffers[current_idx][..current_n];

        // Detect terminator location: within chunk or spanning boundary
        let status = tail.detect_terminator(data);
        let write_len = status.write_len(current_n);

        // Write current chunk (or portion up to terminator) to client
        if let Err(e) = client_write.write_all(&data[..write_len]).await {
            return Err(handle_client_write_error(
                e,
                backend_read,
                ClientWriteErrorContext {
                    write_len,
                    current_n,
                    total_bytes,
                    partial_data: &data[..write_len],
                    terminator_found: status.is_found(),
                    client_addr,
                    backend_id,
                },
            )
            .await);
        }
        total_bytes += write_len as u64;

        // If terminator found, we're done
        if status.is_found() {
            debug!(
                "Client {} multiline response complete ({})",
                client_addr,
                crate::formatting::format_bytes(total_bytes)
            );
            break;
        }

        // Update tail for next iteration
        tail.update(&data[..write_len]);

        // Read next chunk into alternate buffer
        let next_idx = alternate_buffer_index(current_idx);
        let next_n = backend_read
            .read(&mut buffers[next_idx])
            .await
            .context("Failed to read next chunk from backend")?;
        if next_n == 0 {
            debug!(
                "Client {} multiline streaming complete ({}, EOF)",
                client_addr,
                crate::formatting::format_bytes(total_bytes)
            );
            break; // EOF
        }

        // Swap buffers for next iteration
        current_idx = next_idx;
        current_n = next_n;
    }

    Ok(total_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_alternate_buffer_index() {
        assert_eq!(alternate_buffer_index(0), 1);
        assert_eq!(alternate_buffer_index(1), 0);
    }

    #[tokio::test]
    async fn test_drain_until_terminator_immediate() {
        // Response with terminator already present
        let data = b"220 Article follows\r\nLine 1\r\nLine 2\r\n.\r\n";
        let mut reader = Cursor::new(data);
        let client_addr = "127.0.0.1:8000".parse().unwrap();
        let backend_id = crate::types::BackendId::from_index(1);

        let result = drain_until_terminator(&mut reader, b"", client_addr, backend_id).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_drain_until_terminator_spanning() {
        // Response where terminator spans chunks
        let data = b"220 Article follows\r\nLine 1\r\n.\r\n";
        let mut reader = Cursor::new(data);
        let client_addr = "127.0.0.1:8000".parse().unwrap();
        let backend_id = crate::types::BackendId::from_index(1);

        // Start with tail that could span
        let result = drain_until_terminator(&mut reader, b"\r\n", client_addr, backend_id).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_drain_until_terminator_eof() {
        // Response without terminator (EOF)
        let data = b"220 Article follows\r\nLine 1\r\nLine 2\r\n";
        let mut reader = Cursor::new(data);
        let client_addr = "127.0.0.1:8000".parse().unwrap();
        let backend_id = crate::types::BackendId::from_index(1);

        let result = drain_until_terminator(&mut reader, b"", client_addr, backend_id).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_stream_multiline_response_simple() {
        // Simple multiline response
        let response = b"220 Article follows\r\nLine 1\r\nLine 2\r\n.\r\n";
        let mut reader = Cursor::new(response);
        let mut writer = Vec::new();
        let client_addr = "127.0.0.1:8000".parse().unwrap();
        let backend_id = crate::types::BackendId::from_index(1);

        let result = stream_multiline_response(
            &mut reader,
            &mut writer,
            response,
            response.len(),
            client_addr,
            backend_id,
        )
        .await;

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
        let client_addr = "127.0.0.1:8000".parse().unwrap();
        let backend_id = crate::types::BackendId::from_index(1);

        // First chunk includes terminator and extra data
        let first_chunk = &response[..27]; // All data including extra
        let result = stream_multiline_response(
            &mut reader,
            &mut writer,
            first_chunk,
            first_chunk.len(),
            client_addr,
            backend_id,
        )
        .await;

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
        let client_addr = "127.0.0.1:8000".parse().unwrap();
        let backend_id = crate::types::BackendId::from_index(1);

        let result = stream_multiline_response(
            &mut reader,
            &mut writer,
            response,
            response.len(),
            client_addr,
            backend_id,
        )
        .await;

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
            body.extend_from_slice(format!("Line {}\r\n", i).as_bytes());
        }
        let terminator = b".\r\n";

        let mut full_response = Vec::new();
        full_response.extend_from_slice(header);
        full_response.extend_from_slice(&body);
        full_response.extend_from_slice(terminator);

        let mut reader = Cursor::new(&full_response[header.len()..]);
        let mut writer = Vec::new();
        let client_addr = "127.0.0.1:8000".parse().unwrap();
        let backend_id = crate::types::BackendId::from_index(1);

        let result = stream_multiline_response(
            &mut reader,
            &mut writer,
            header,
            header.len(),
            client_addr,
            backend_id,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), full_response.len() as u64);
        assert_eq!(&writer[..], &full_response[..]);
    }
}
