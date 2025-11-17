//! Backend communication module
//!
//! Handles sending commands to backend and reading initial response.
//! Does NOT buffer entire responses - caller handles streaming.

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::warn;

use crate::pool::PooledBuffer;
use crate::protocol::{MIN_RESPONSE_LENGTH, Response};
use crate::types::BackendId;

/// Send command to backend and read first chunk
///
/// Returns (bytes_read, response_code, is_multiline, ttfb_micros, send_micros, recv_micros)
/// The first chunk is written into the provided buffer.
pub async fn send_command_and_read_first_chunk<T>(
    backend_conn: &mut T,
    command: &str,
    backend_id: BackendId,
    client_addr: std::net::SocketAddr,
    chunk: &mut PooledBuffer,
) -> Result<(usize, Response, bool, u64, u64, u64)>
where
    T: AsyncReadExt + AsyncWriteExt + Unpin,
{
    use std::time::Instant;

    let start = Instant::now();

    // Write command to backend
    let send_start = Instant::now();
    backend_conn.write_all(command.as_bytes()).await?;
    let send_elapsed = send_start.elapsed();

    // Read first chunk to determine response type
    let recv_start = Instant::now();
    let n = chunk.read_from(backend_conn).await?;
    let recv_elapsed = recv_start.elapsed();

    if n == 0 {
        return Err(anyhow::anyhow!("Backend connection closed unexpectedly"));
    }

    // Warn if response is too short to be valid
    if n < MIN_RESPONSE_LENGTH {
        warn!(
            "Client {} got short response from backend {:?} ({} bytes < {} min): {:02x?}",
            client_addr,
            backend_id,
            n,
            MIN_RESPONSE_LENGTH,
            &chunk[..n]
        );
    }

    // Parse response code and check if multiline
    let response_code = Response::parse(&chunk[..n]);
    let is_multiline = response_code.is_multiline();

    // Validate response code
    if response_code == Response::Invalid {
        warn!(
            "Client {} got invalid response from backend {:?} ({} bytes): {:?}",
            client_addr,
            backend_id,
            n,
            String::from_utf8_lossy(&chunk[..n.min(50)])
        );
    } else if let Some(code) = response_code.status_code() {
        // Warn on unusual status codes
        let raw_code = code.as_u16();
        if raw_code == 0 || raw_code >= 600 {
            warn!(
                "Client {} got unusual status code {} from backend {:?}: {:?}",
                client_addr,
                code,
                backend_id,
                String::from_utf8_lossy(&chunk[..n.min(50)])
            );
        }
    }

    let elapsed = start.elapsed();

    Ok((
        n,
        response_code,
        is_multiline,
        elapsed.as_micros() as u64,
        send_elapsed.as_micros() as u64,
        recv_elapsed.as_micros() as u64,
    ))
}
