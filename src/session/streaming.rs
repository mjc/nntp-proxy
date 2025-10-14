//! Client streaming module
//!
//! Handles streaming response data from backend to client with pipelined I/O.
//! Uses double-buffering for optimal performance on large transfers.

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

use crate::constants::buffer::STREAMING_CHUNK_SIZE;
use crate::protocol::{NntpResponse, TERMINATOR_TAIL_SIZE};

/// Helper for tracking the last few bytes of streamed data
/// 
/// Used to detect terminators that span across chunk boundaries.
struct TailBuffer {
    data: [u8; TERMINATOR_TAIL_SIZE],
    len: usize,
}

impl TailBuffer {
    fn new() -> Self {
        Self {
            data: [0; TERMINATOR_TAIL_SIZE],
            len: 0,
        }
    }
    
    /// Update tail with the last bytes from a chunk
    fn update(&mut self, chunk: &[u8]) {
        if chunk.len() >= TERMINATOR_TAIL_SIZE {
            self.data.copy_from_slice(&chunk[chunk.len() - TERMINATOR_TAIL_SIZE..]);
            self.len = TERMINATOR_TAIL_SIZE;
        } else if !chunk.is_empty() {
            self.data[..chunk.len()].copy_from_slice(chunk);
            self.len = chunk.len();
        }
    }
    
    /// Get the tail data as a slice
    fn as_slice(&self) -> &[u8] {
        &self.data[..self.len]
    }
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
    let mut chunk = vec![0u8; STREAMING_CHUNK_SIZE];
    let mut tail = TailBuffer::new();
    tail.update(initial_tail);
    
    loop {
        let n = backend_read.read(&mut chunk).await?;
        if n == 0 {
            break; // EOF
        }
        
        let data = &chunk[..n];
        if NntpResponse::has_terminator_at_end(data) 
            || NntpResponse::has_spanning_terminator(tail.as_slice(), tail.len, data, n) 
        {
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
    let mut buffers = [vec![0u8; STREAMING_CHUNK_SIZE], vec![0u8; STREAMING_CHUNK_SIZE]];
    let mut current_idx = 0;
    
    // Copy first chunk into buffer
    buffers[0][..first_n].copy_from_slice(&first_chunk[..first_n]);
    let mut current_n = first_n;
    
    // Track tail for spanning terminator detection
    let mut tail = TailBuffer::new();
    
    // Main streaming loop - processes first chunk and all subsequent chunks uniformly
    loop {
        let current_chunk = &buffers[current_idx];
        let data = &current_chunk[..current_n];
        
        // Detect terminator location: within chunk or spanning boundary
        let (write_len, terminator_found) = 
            if let Some(term_end) = NntpResponse::find_terminator_end(data) {
                // Complete terminator found within chunk (middle or end)
                (term_end, true)
            } else if NntpResponse::has_spanning_terminator(tail.as_slice(), tail.len, data, current_n) {
                // Terminator spans across chunk boundary
                (current_n, true)
            } else {
                // No terminator yet, write entire chunk
                (current_n, false)
            };
        
        // Write current chunk (or portion up to terminator) to client
        if let Err(e) = client_write.write_all(&data[..write_len]).await {
            // Client disconnected - drain remaining response from backend if terminator not yet seen
            warn!(
                "Client {} disconnected while streaming ({} bytes of {}, total {} bytes so far) → backend {:?}",
                client_addr, write_len, current_n, total_bytes, backend_id
            );
            
            if !terminator_found {
                // Need to drain the rest to keep connection clean
                drain_until_terminator(
                    backend_read,
                    &data[..write_len],
                    client_addr,
                    backend_id,
                )
                .await?;
            }
            
            return Err(e.into());
        }
        total_bytes += write_len as u64;
        
        // If terminator found, we're done
        if terminator_found {
            debug!(
                "Client {} multiline response complete ({} bytes)",
                client_addr, total_bytes
            );
            break;
        }
        
        // Update tail for next iteration
        tail.update(&data[..write_len]);
        
        // Read next chunk into alternate buffer
        let next_idx = 1 - current_idx;
        let next_n = backend_read.read(&mut buffers[next_idx]).await?;
        if next_n == 0 {
            debug!(
                "Client {} multiline streaming complete ({} bytes, EOF)",
                client_addr, total_bytes
            );
            break; // EOF
        }
        
        // Swap buffers for next iteration
        current_idx = next_idx;
        current_n = next_n;
    }
    
    Ok(total_bytes)
}
