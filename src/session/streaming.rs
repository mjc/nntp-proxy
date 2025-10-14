//! Client streaming module
//!
//! This module handles streaming data to NNTP clients.
//! It is responsible for taking response data and writing it to the client connection.
//!
//! Key principle: This module does NOT interact with backends. All errors
//! returned from this module indicate client failures.

use anyhow::Result;
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tracing::debug;

use super::backend::BackendResponse;

/// Errors that can occur when streaming to client
#[derive(Debug, Error)]
pub enum ClientError {
    /// Client disconnected during streaming
    #[error("Client disconnected")]
    Disconnected,

    /// Client I/O error
    #[error("Client I/O error: {0}")]
    IoError(#[from] std::io::Error),
}

/// Stream a complete backend response to the client
///
/// This function takes a BackendResponse (with all chunks already fetched from backend)
/// and writes them sequentially to the client.
///
/// # Arguments
///
/// * `client_write` - Write half of the client TCP connection
/// * `response` - Complete backend response with all chunks
/// * `client_addr` - Client address for logging
///
/// # Returns
///
/// Returns the total number of bytes written to the client
///
/// # Errors
///
/// Returns `ClientError` if:
/// - Writing to client fails (connection broken, client disconnected, etc.)
///
/// All errors indicate client-side failure. The backend data is already complete.
pub async fn stream_to_client<W>(
    client_write: &mut W,
    response: BackendResponse,
    client_addr: std::net::SocketAddr,
) -> Result<u64, ClientError>
where
    W: AsyncWriteExt + Unpin,
{
    let mut total_bytes = 0u64;

    debug!(
        "Client {} streaming {} chunks to client ({} total bytes)",
        client_addr,
        response.chunk_count(),
        response.total_bytes
    );

    // Stream all chunks to client
    for (i, chunk) in response.chunks().enumerate() {
        debug!(
            "Client {} writing chunk {}/{} ({} bytes)",
            client_addr,
            i + 1,
            response.chunk_count(),
            chunk.len()
        );

        // Write chunk to client
        client_write.write_all(chunk).await?;
        total_bytes += chunk.len() as u64;
    }

    debug!(
        "Client {} successfully streamed {} bytes to client",
        client_addr, total_bytes
    );

    Ok(total_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::ResponseCode;
    use bytes::Bytes;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[tokio::test]
    async fn test_stream_single_chunk_to_client() {
        let chunks = vec![Bytes::from_static(b"200 server ready\r\n")];
        let response = BackendResponse::new(
            ResponseCode::parse(b"200 server ready\r\n"),
            false,
            chunks,
            18,
        );

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let (mut client, mut server) = tokio::io::duplex(1024);

        // Spawn task to read from client side to prevent blocking
        let reader = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = [0u8; 1024];
            client.read(&mut buf).await.unwrap()
        });

        let result = stream_to_client(&mut server, response, addr).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 18);

        // Verify data was received
        let bytes_read = reader.await.unwrap();
        assert_eq!(bytes_read, 18);
    }

    #[tokio::test]
    async fn test_stream_multiple_chunks_to_client() {
        let chunks = vec![
            Bytes::from_static(b"215 list follows\r\n"),
            Bytes::from_static(b"group1 0 0 y\r\n"),
            Bytes::from_static(b"group2 0 0 y\r\n"),
            Bytes::from_static(b".\r\n"),
        ];
        let total = chunks.iter().map(|c| c.len()).sum::<usize>() as u64;
        let response = BackendResponse::new(
            ResponseCode::parse(b"215 list follows\r\n"),
            true,
            chunks,
            total,
        );

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let (mut client, mut server) = tokio::io::duplex(1024);

        // Spawn task to read from client side
        let reader = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut buf = vec![0u8; 1024];
            let n = client.read(&mut buf).await.unwrap();
            buf.truncate(n);
            buf
        });

        let result = stream_to_client(&mut server, response, addr).await;

        assert!(result.is_ok());
        let bytes_written = result.unwrap();
        assert_eq!(bytes_written, total);

        // Verify data received by client
        let received = reader.await.unwrap();
        assert_eq!(
            received,
            b"215 list follows\r\ngroup1 0 0 y\r\ngroup2 0 0 y\r\n.\r\n"
        );
    }

    #[tokio::test]
    async fn test_stream_to_closed_client() {
        let chunks = vec![Bytes::from_static(b"200 server ready\r\n")];
        let response = BackendResponse::new(
            ResponseCode::parse(b"200 server ready\r\n"),
            false,
            chunks,
            18,
        );

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let (client, mut server) = tokio::io::duplex(1024);

        // Close client side immediately
        drop(client);

        // Give it a moment to propagate
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let result = stream_to_client(&mut server, response, addr).await;

        // Should fail with client error
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_stream_empty_chunks() {
        // Edge case: response with no chunks (shouldn't happen in practice, but let's handle it)
        let chunks = vec![];
        let response = BackendResponse::new(ResponseCode::parse(b"200 OK\r\n"), false, chunks, 0);

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let (_, mut server) = tokio::io::duplex(1024);

        let result = stream_to_client(&mut server, response, addr).await;

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }
}
