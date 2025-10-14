//! Client streaming module
//!
//! Handles streaming response data from backend to client with pipelined I/O.
//! Uses double-buffering for optimal performance on large transfers.

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

use crate::constants::buffer::STREAMING_CHUNK_SIZE;
use crate::protocol::{NntpResponse, TERMINATOR_TAIL_SIZE};

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
) -> Result<u64>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut total_bytes = first_n as u64;

    // Write first chunk to client
    if let Err(e) = client_write.write_all(&first_chunk[..first_n]).await {
        // Client disconnected on first chunk - need to drain if multiline and no terminator yet
        let has_terminator = NntpResponse::has_terminator_at_end(&first_chunk[..first_n]);
        if !has_terminator {
            use tracing::debug;
            debug!(
                "Client {} disconnected on first chunk, draining remaining response from backend",
                client_addr
            );
            
            // Drain the rest of the response
            let mut drain_chunk = vec![0u8; STREAMING_CHUNK_SIZE];
            let mut tail: [u8; TERMINATOR_TAIL_SIZE] = [0; TERMINATOR_TAIL_SIZE];
            let mut tail_len: usize = 0;
            
            // Initialize tail from first chunk
            if first_n >= TERMINATOR_TAIL_SIZE {
                tail.copy_from_slice(&first_chunk[first_n - TERMINATOR_TAIL_SIZE..first_n]);
                tail_len = TERMINATOR_TAIL_SIZE;
            } else if first_n > 0 {
                tail[..first_n].copy_from_slice(&first_chunk[..first_n]);
                tail_len = first_n;
            }
            
            loop {
                let n = backend_read.read(&mut drain_chunk).await?;
                if n == 0 {
                    break; // EOF
                }
                
                if NntpResponse::has_terminator_at_end(&drain_chunk[..n]) {
                    break;
                }
                
                if NntpResponse::has_spanning_terminator(&tail, tail_len, &drain_chunk, n) {
                    break;
                }
                
                // Update tail
                if n >= TERMINATOR_TAIL_SIZE {
                    tail.copy_from_slice(&drain_chunk[n - TERMINATOR_TAIL_SIZE..n]);
                    tail_len = TERMINATOR_TAIL_SIZE;
                } else if n > 0 {
                    tail[..n].copy_from_slice(&drain_chunk[..n]);
                    tail_len = n;
                }
            }
            
            debug!(
                "Client {} drained response from backend after first chunk disconnect",
                client_addr
            );
        }
        return Err(e.into());
    }

    // Check if terminator is in first chunk
    let has_terminator = NntpResponse::has_terminator_at_end(&first_chunk[..first_n]);
    if has_terminator {
        debug!(
            "Client {} multiline response complete in first chunk ({} bytes)",
            client_addr, total_bytes
        );
        return Ok(total_bytes);
    }

    // Prepare double buffering for pipelined streaming
    let mut chunk1 = vec![0u8; STREAMING_CHUNK_SIZE];
    let mut chunk2 = vec![0u8; STREAMING_CHUNK_SIZE];

    let mut tail: [u8; TERMINATOR_TAIL_SIZE] = [0; TERMINATOR_TAIL_SIZE];
    let mut tail_len: usize = 0;

    // Initialize tail with last bytes of first chunk
    if first_n >= TERMINATOR_TAIL_SIZE {
        tail.copy_from_slice(&first_chunk[first_n - TERMINATOR_TAIL_SIZE..first_n]);
        tail_len = TERMINATOR_TAIL_SIZE;
    } else if first_n > 0 {
        tail[..first_n].copy_from_slice(&first_chunk[..first_n]);
        tail_len = first_n;
    }

    let mut current_chunk = &mut chunk1;
    let mut next_chunk = &mut chunk2;

    // Read next chunk and start loop
    let mut current_n = backend_read.read(next_chunk).await?;
    if current_n > 0 {
        std::mem::swap(&mut current_chunk, &mut next_chunk);

        loop {
            // Write current chunk to client
            if let Err(e) = client_write.write_all(&current_chunk[..current_n]).await {
                // Client disconnected - drain the rest from backend to keep connection clean
                use tracing::debug;
                debug!(
                    "Client {} disconnected while streaming, draining remaining response from backend",
                    client_addr
                );
                
                // Check if we've already seen the terminator in current chunk
                let has_term = NntpResponse::has_terminator_at_end(&current_chunk[..current_n]);
                if !has_term {
                    let has_spanning_term =
                        NntpResponse::has_spanning_terminator(&tail, tail_len, current_chunk, current_n);
                    
                    if !has_spanning_term {
                        // Need to keep reading until we find the terminator
                        let mut drain_chunk = vec![0u8; STREAMING_CHUNK_SIZE];
                        let mut drain_tail = tail;
                        let mut drain_tail_len = tail_len;
                        
                        loop {
                            let n = backend_read.read(&mut drain_chunk).await?;
                            if n == 0 {
                                break; // EOF
                            }
                            
                            if NntpResponse::has_terminator_at_end(&drain_chunk[..n]) {
                                break; // Found terminator
                            }
                            
                            if NntpResponse::has_spanning_terminator(&drain_tail, drain_tail_len, &drain_chunk, n) {
                                break; // Found spanning terminator
                            }
                            
                            // Update tail for next iteration
                            if n >= TERMINATOR_TAIL_SIZE {
                                drain_tail.copy_from_slice(&drain_chunk[n - TERMINATOR_TAIL_SIZE..n]);
                                drain_tail_len = TERMINATOR_TAIL_SIZE;
                            } else if n > 0 {
                                drain_tail[..n].copy_from_slice(&drain_chunk[..n]);
                                drain_tail_len = n;
                            }
                        }
                        
                        debug!(
                            "Client {} drained remaining response from backend, connection is clean",
                            client_addr
                        );
                    }
                }
                
                return Err(e.into());
            }
            total_bytes += current_n as u64;

            // Check terminator in chunk we just wrote
            let has_term = NntpResponse::has_terminator_at_end(&current_chunk[..current_n]);
            if has_term {
                break; // Done!
            }

            // Check boundary spanning terminator
            let has_spanning_term =
                NntpResponse::has_spanning_terminator(&tail, tail_len, current_chunk, current_n);
            if has_spanning_term {
                break; // Done!
            }

            // Update tail for next iteration
            if current_n >= TERMINATOR_TAIL_SIZE {
                tail.copy_from_slice(&current_chunk[current_n - TERMINATOR_TAIL_SIZE..current_n]);
                tail_len = TERMINATOR_TAIL_SIZE;
            } else if current_n > 0 {
                tail[..current_n].copy_from_slice(&current_chunk[..current_n]);
                tail_len = current_n;
            }

            // Read next chunk
            let next_n = backend_read.read(next_chunk).await?;
            if next_n == 0 {
                break; // EOF
            }

            // Swap buffers for next iteration
            std::mem::swap(&mut current_chunk, &mut next_chunk);
            current_n = next_n;
        }
    }

    debug!(
        "Client {} multiline streaming complete ({} bytes)",
        client_addr, total_bytes
    );

    Ok(total_bytes)
}
