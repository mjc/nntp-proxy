//! Backend communication module
//!
//! This module handles all communication with NNTP backend servers.
//! It is responsible for sending commands and receiving complete responses.
//!
//! # Important Performance Note
//!
//! This module **buffers entire responses** before returning them. This is suitable for:
//! - Single-line responses (200 OK, 430 No such article, etc.)
//! - Small multiline responses (LIST, GROUP commands)
//! - Testing and mocking
//! - Future caching layer integration
//!
//! **NOT suitable for the hot path** with large article downloads because:
//! - Buffers entire 50MB+ articles in memory before streaming
//! - No pipelined I/O (can't read next chunk while writing current chunk)
//! - Kills throughput from 100+ MB/s down to < 1 MB/s
//!
//! For high-performance article streaming, use the direct pipelined approach
//! in `execute_command_on_backend()` in legacy.rs.
//!
//! Key principle: This module does NOT interact with clients. All errors
//! returned from this module indicate backend failures.

use anyhow::Result;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::debug;

use crate::constants::buffer::STREAMING_CHUNK_SIZE;
use crate::protocol::{NntpResponse, ResponseCode, TERMINATOR_TAIL_SIZE};
use crate::types::BackendId;

/// Complete response from a backend server
#[derive(Debug, Clone)]
pub struct BackendResponse {
    /// Response code (e.g., 200, 220, 430)
    pub code: ResponseCode,
    /// Is this a multiline response?
    pub is_multiline: bool,
    /// All response data as individual chunks (cheap to clone due to Bytes)
    pub chunks: Vec<Bytes>,
    /// Total bytes read from backend
    pub total_bytes: u64,
}

impl BackendResponse {
    /// Create a new backend response
    pub fn new(
        code: ResponseCode,
        is_multiline: bool,
        chunks: Vec<Bytes>,
        total_bytes: u64,
    ) -> Self {
        Self {
            code,
            is_multiline,
            chunks,
            total_bytes,
        }
    }

    /// Iterate over all chunks in order
    pub fn chunks(&self) -> impl Iterator<Item = &Bytes> + '_ {
        self.chunks.iter()
    }

    /// Get total number of chunks
    pub fn chunk_count(&self) -> usize {
        self.chunks.len()
    }

    /// Get the first chunk (always present for valid responses)
    pub fn first_chunk(&self) -> Option<&Bytes> {
        self.chunks.first()
    }
}

/// Execute a command on backend and fetch the complete response
///
/// This function:
/// 1. Writes the command to the backend
/// 2. Reads the first response chunk and parses the response code
/// 3. If multiline, reads all remaining chunks until terminator
/// 4. Returns a complete BackendResponse
///
/// # Errors
///
/// Returns an error if:
/// - Writing the command fails (connection broken)
/// - Reading the response fails (connection broken)
/// - Backend closes connection unexpectedly (0 bytes read)
///
/// All errors indicate backend failure. There is NO client I/O in this function.
pub async fn fetch_backend_response<T>(
    backend_conn: &mut T,
    command: &str,
    backend_id: BackendId,
    client_addr: std::net::SocketAddr,
) -> Result<BackendResponse>
where
    T: AsyncReadExt + AsyncWriteExt + Unpin,
{
    // Write command to backend
    debug!(
        "Client {} forwarding command to backend {:?} ({} bytes): {}",
        client_addr,
        backend_id,
        command.len(),
        command.trim()
    );

    backend_conn.write_all(command.as_bytes()).await?;

    debug!(
        "Client {} command sent to backend {:?}",
        client_addr, backend_id
    );

    // Read first chunk to determine response type
    debug!(
        "Client {} reading response from backend {:?}",
        client_addr, backend_id
    );

    let mut chunk = vec![0u8; STREAMING_CHUNK_SIZE];
    let n = backend_conn.read(&mut chunk).await?;

    if n == 0 {
        return Err(anyhow::anyhow!("Backend connection closed unexpectedly"));
    }

    debug!(
        "Client {} received backend response chunk ({} bytes): {}",
        client_addr,
        n,
        String::from_utf8_lossy(&chunk[..n.min(100)])
    );

    // Parse response code and check if multiline
    let response_code = ResponseCode::parse(&chunk[..n]);
    let is_multiline = response_code.is_multiline();

    // Log first line (best effort)
    if let Some(newline_pos) = chunk[..n].iter().position(|&b| b == b'\n')
        && let Ok(first_line_str) = std::str::from_utf8(&chunk[..newline_pos])
    {
        debug!(
            "Client {} got first line from backend {:?}: {}",
            client_addr,
            backend_id,
            first_line_str.trim()
        );
    }

    // Pre-allocate chunks vector for typical response size
    let mut chunks = Vec::with_capacity(4);
    let mut total_bytes = n as u64;

    // If multiline, read until terminator
    if is_multiline {
        // Check if terminator is already in first chunk
        let has_terminator = NntpResponse::has_terminator_at_end(&chunk[..n]);

        if !has_terminator {
            // Need to read more chunks - pass slice before consuming chunk
            let additional =
                read_multiline_chunks(backend_conn, &chunk[..n], backend_id, client_addr).await?;

            for chunk in &additional {
                total_bytes += chunk.len() as u64;
            }

            // Store first chunk (convert to Bytes without copying by taking ownership)
            chunk.truncate(n);
            chunks.push(Bytes::from(chunk));
            chunks.extend(additional);
        } else {
            // Terminator in first chunk - store it
            chunk.truncate(n);
            chunks.push(Bytes::from(chunk));
        }
    } else {
        // Single-line response - store first chunk
        chunk.truncate(n);
        chunks.push(Bytes::from(chunk));
    }

    debug!(
        "Client {} received complete response from backend {:?} ({} bytes, {} chunks)",
        client_addr,
        backend_id,
        total_bytes,
        chunks.len()
    );

    Ok(BackendResponse::new(
        response_code,
        is_multiline,
        chunks,
        total_bytes,
    ))
}

/// Read remaining chunks for a multiline response
///
/// Uses double buffering for efficient streaming and terminator detection.
async fn read_multiline_chunks<T>(
    backend_conn: &mut T,
    first_chunk: &[u8],
    backend_id: BackendId,
    client_addr: std::net::SocketAddr,
) -> Result<Vec<Bytes>>
where
    T: AsyncReadExt + Unpin,
{
    let mut chunks = Vec::with_capacity(4); // Pre-allocate for typical multiline response
    let mut buffer_a = vec![0u8; STREAMING_CHUNK_SIZE];
    let mut buffer_b = vec![0u8; STREAMING_CHUNK_SIZE];

    let mut tail: [u8; TERMINATOR_TAIL_SIZE] = [0; TERMINATOR_TAIL_SIZE];
    let mut tail_len: usize = 0;

    // Initialize tail with last bytes of first chunk
    let first_len = first_chunk.len();
    if first_len >= TERMINATOR_TAIL_SIZE {
        tail.copy_from_slice(&first_chunk[first_len - TERMINATOR_TAIL_SIZE..first_len]);
        tail_len = TERMINATOR_TAIL_SIZE;
    } else if first_len > 0 {
        tail[..first_len].copy_from_slice(&first_chunk[..first_len]);
        tail_len = first_len;
    }

    let mut current_chunk = &mut buffer_a;
    let mut next_chunk = &mut buffer_b;

    // Read next chunk
    let mut current_n = backend_conn.read(next_chunk).await?;

    if current_n > 0 {
        std::mem::swap(&mut current_chunk, &mut next_chunk);

        loop {
            debug!(
                "Client {} read multiline chunk from backend {:?} ({} bytes)",
                client_addr, backend_id, current_n
            );

            // Check for terminator
            let has_term = NntpResponse::has_terminator_at_end(&current_chunk[..current_n]);

            if has_term {
                // Found terminator, this is the last chunk
                // Convert to Bytes without copying by taking ownership
                let mut owned = std::mem::replace(current_chunk, vec![0u8; STREAMING_CHUNK_SIZE]);
                owned.truncate(current_n);
                chunks.push(Bytes::from(owned));
                break;
            }

            // Check for spanning terminator
            let has_spanning_term =
                NntpResponse::has_spanning_terminator(&tail, tail_len, current_chunk, current_n);

            if has_spanning_term {
                // Found spanning terminator, this is the last chunk
                let mut owned = std::mem::replace(current_chunk, vec![0u8; STREAMING_CHUNK_SIZE]);
                owned.truncate(current_n);
                chunks.push(Bytes::from(owned));
                break;
            }

            // Save chunk (convert to Bytes without copying by taking ownership)
            let mut owned = std::mem::replace(current_chunk, vec![0u8; STREAMING_CHUNK_SIZE]);
            owned.truncate(current_n);
            chunks.push(Bytes::from(owned));

            // Update tail for next iteration
            if current_n >= TERMINATOR_TAIL_SIZE {
                tail.copy_from_slice(&current_chunk[current_n - TERMINATOR_TAIL_SIZE..current_n]);
                tail_len = TERMINATOR_TAIL_SIZE;
            } else if current_n > 0 {
                tail[..current_n].copy_from_slice(&current_chunk[..current_n]);
                tail_len = current_n;
            }

            // Read next chunk
            let next_n = backend_conn.read(next_chunk).await?;
            if next_n == 0 {
                break; // EOF
            }

            // Swap buffers
            std::mem::swap(&mut current_chunk, &mut next_chunk);
            current_n = next_n;
        }
    }

    Ok(chunks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_response_chunks() {
        let chunks = vec![
            Bytes::from_static(b"200 OK\r\n"),
            Bytes::from_static(b"line 1\r\n"),
            Bytes::from_static(b"line 2\r\n"),
        ];

        let response =
            BackendResponse::new(ResponseCode::parse(b"200 OK\r\n"), true, chunks.clone(), 24);

        assert_eq!(response.chunk_count(), 3);
        assert_eq!(response.total_bytes, 24);
        assert_eq!(response.first_chunk().unwrap(), &chunks[0]);

        let collected: Vec<_> = response.chunks().collect();
        assert_eq!(collected.len(), 3);
    }

    #[tokio::test]
    async fn test_fetch_single_line_response() {
        let (mut client, mut server) = tokio::io::duplex(1024);

        // Spawn task to simulate backend
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            // Read command (we don't care about it for this test)
            let mut buf = [0u8; 128];
            let _ = server.read(&mut buf).await;
            // Send response
            server.write_all(b"200 server ready\r\n").await.unwrap();
        });

        let result = fetch_backend_response(
            &mut client,
            "DATE\r\n",
            crate::types::BackendId::from_index(0),
            "127.0.0.1:9000".parse().unwrap(),
        )
        .await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(!response.is_multiline);
        assert_eq!(response.chunk_count(), 1);
        assert_eq!(response.first_chunk().unwrap().len(), 18); // "200 server ready\r\n"
    }

    #[tokio::test]
    async fn test_fetch_multiline_response_with_terminator_in_first_chunk() {
        let (mut client, mut server) = tokio::io::duplex(1024);

        // Spawn task to simulate backend
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            // Read command
            let mut buf = [0u8; 128];
            let _ = server.read(&mut buf).await;
            // Send multiline response with terminator
            server
                .write_all(b"215 list of newsgroups follows\r\ngroup1 0 0 y\r\n.\r\n")
                .await
                .unwrap();
        });

        let result = fetch_backend_response(
            &mut client,
            "LIST\r\n",
            crate::types::BackendId::from_index(0),
            "127.0.0.1:9000".parse().unwrap(),
        )
        .await;

        assert!(result.is_ok());
        let response = result.unwrap();
        assert!(response.is_multiline);
        // Terminator is in first chunk, so no additional chunks needed
        assert_eq!(response.chunk_count(), 1);
    }

    #[tokio::test]
    async fn test_backend_connection_closed() {
        let (mut client, server) = tokio::io::duplex(1024);

        // Spawn task that immediately closes connection
        tokio::spawn(async move {
            drop(server); // Close immediately
        });

        // Give it a moment to close
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let result = fetch_backend_response(
            &mut client,
            "DATE\r\n",
            crate::types::BackendId::from_index(0),
            "127.0.0.1:9000".parse().unwrap(),
        )
        .await;

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("connection closed") || err_msg.contains("broken pipe"),
            "Expected connection closed error, got: {}",
            err_msg
        );
    }
}
