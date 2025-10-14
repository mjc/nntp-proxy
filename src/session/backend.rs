//! Backend communication module
//!
//! Handles sending commands to backend and reading initial response.
//! Does NOT buffer entire responses - caller handles streaming.

use anyhow::Result;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, warn};

use crate::constants::buffer::STREAMING_CHUNK_SIZE;
use crate::protocol::{MIN_RESPONSE_LENGTH, ResponseCode};
use crate::types::BackendId;

/// Send command to backend and read first chunk
///
/// Returns (first_chunk, bytes_read, response_code, is_multiline)
pub async fn send_command_and_read_first_chunk<T>(
    backend_conn: &mut T,
    command: &str,
    backend_id: BackendId,
    client_addr: std::net::SocketAddr,
) -> Result<(Vec<u8>, usize, ResponseCode, bool)>
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
    let response_code = ResponseCode::parse(&chunk[..n]);
    let is_multiline = response_code.is_multiline();

    // Validate response code
    if response_code == ResponseCode::Invalid {
        warn!(
            "Client {} got invalid response from backend {:?} ({} bytes): {:?}",
            client_addr,
            backend_id,
            n,
            String::from_utf8_lossy(&chunk[..n.min(50)])
        );
    } else if let Some(code) = response_code.status_code() {
        // Warn on unusual status codes
        if code == 0 || code >= 600 {
            warn!(
                "Client {} got unusual status code {} from backend {:?}: {:?}",
                client_addr,
                code,
                backend_id,
                String::from_utf8_lossy(&chunk[..n.min(50)])
            );
        }
    }

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

    Ok((chunk, n, response_code, is_multiline))
}
