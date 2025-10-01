//! Data transfer and streaming utilities
//!
//! This module handles bidirectional data transfer between client and backend
//! connections, with support for both standard and high-throughput modes.

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{ReadHalf, WriteHalf};
use tracing::{debug, warn};

use crate::protocol::HIGH_THROUGHPUT_BUFFER_SIZE;

/// Streaming handler for bidirectional data transfer
pub struct StreamHandler;

impl StreamHandler {
    /// Handle high-throughput data transfer after initial command
    /// This is optimized for large article transfers
    pub async fn high_throughput_transfer(
        mut client_reader: BufReader<ReadHalf<'_>>,
        mut client_write: WriteHalf<'_>,
        mut backend_read: ReadHalf<'_>,
        mut backend_write: WriteHalf<'_>,
        mut client_to_backend_bytes: u64,
        mut backend_to_client_bytes: u64,
    ) -> Result<(u64, u64)> {
        debug!("Starting high-throughput data transfer");

        // Use direct buffer allocation for high-throughput to avoid pool overhead
        let mut direct_buffer = vec![0u8; HIGH_THROUGHPUT_BUFFER_SIZE];

        // Continue handling commands and large data responses
        loop {
            let mut line = String::new();

            tokio::select! {
                // Continue reading client commands
                result = client_reader.read_line(&mut line) => {
                    match result {
                        Ok(0) => break,
                        Ok(_) => {
                            // Forward all commands including QUIT to backend
                            // This maintains proper NNTP protocol flow
                            backend_write.write_all(line.as_bytes()).await?;
                            client_to_backend_bytes += line.len() as u64;
                        }
                        Err(e) => {
                            warn!("Error reading client command: {}", e);
                            break;
                        }
                    }
                }

                // Read large responses from backend with direct buffer (no pool overhead)
                result = backend_read.read(&mut direct_buffer) => {
                    match result {
                        Ok(0) => {
                            break;
                        }
                        Ok(n) => {
                            client_write.write_all(&direct_buffer[..n]).await?;
                            backend_to_client_bytes += n as u64;

                            // For very large transfers, ensure we keep reading efficiently
                            if n == direct_buffer.len() {
                                // Buffer was full, likely more data coming
                                // Continue optimized reading...
                            }
                        }
                        Err(e) => {
                            warn!("Error reading backend response: {}", e);
                            break;
                        }
                    }
                }
            }
        }

        Ok((client_to_backend_bytes, backend_to_client_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_handler_exists() {
        // Basic compile-time test
        let _ = StreamHandler;
    }
}
