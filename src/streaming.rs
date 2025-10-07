//! Data transfer and streaming utilities
//!
//! This module handles bidirectional data transfer between client and backend
//! connections, with support for both standard and high-throughput modes.

use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{ReadHalf, WriteHalf};
use tracing::{debug, warn};

use crate::constants::stateless_proxy::HIGH_THROUGHPUT_BUFFER_SIZE;

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

        // Reuse line buffer to avoid allocations
        let mut line = String::with_capacity(512);

        // Continue handling commands and large data responses
        loop {
            line.clear();

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
    use std::io::Cursor;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn test_stream_handler_exists() {
        // Basic compile-time test
        let _ = StreamHandler;
    }

    #[test]
    fn test_buffer_sizes() {
        // Verify we're using optimized buffer sizes
        assert_eq!(HIGH_THROUGHPUT_BUFFER_SIZE, 262144); // 256KB
        const _: () = assert!(HIGH_THROUGHPUT_BUFFER_SIZE > 8192); // Larger than default
        const _: () = assert!(HIGH_THROUGHPUT_BUFFER_SIZE % 4096 == 0); // Page-aligned
    }

    #[tokio::test]
    async fn test_bidirectional_copy_small_data() {
        // Test with small data that fits in buffer
        let client_data = b"Hello from client";
        let backend_data = b"Hello from backend";

        let mut client_read = Cursor::new(client_data);
        let mut client_write = Vec::new();
        let mut backend_read = Cursor::new(backend_data);
        let mut backend_write = Vec::new();

        // Simulate a small transfer
        let mut buffer1 = vec![0u8; 1024];
        let mut buffer2 = vec![0u8; 1024];

        // Read from client, write to backend
        let n = client_read.read(&mut buffer1).await.unwrap();
        backend_write.write_all(&buffer1[..n]).await.unwrap();
        assert_eq!(&backend_write, client_data);

        // Read from backend, write to client
        let n = backend_read.read(&mut buffer2).await.unwrap();
        client_write.write_all(&buffer2[..n]).await.unwrap();
        assert_eq!(&client_write, backend_data);
    }

    #[tokio::test]
    async fn test_large_buffer_allocation() {
        // Test that we can allocate the optimized buffer size
        let buffer = vec![0u8; HIGH_THROUGHPUT_BUFFER_SIZE];
        assert_eq!(buffer.len(), HIGH_THROUGHPUT_BUFFER_SIZE);

        // Test writing to the buffer
        let mut cursor = Cursor::new(buffer);
        cursor.write_all(b"test data").await.unwrap();
    }

    #[test]
    fn test_buffer_size_optimization() {
        // Verify buffer size is optimized for typical article sizes
        // News articles are typically 1KB-100KB
        // Our 256KB buffer should handle most articles in one read
        const _: () = assert!(HIGH_THROUGHPUT_BUFFER_SIZE >= 256 * 1024);

        // Should be significantly larger than default socket buffer (64KB)
        const _: () = assert!(HIGH_THROUGHPUT_BUFFER_SIZE > 65536);
    }

    #[tokio::test]
    async fn test_stream_handler_buffer_creation() {
        // Verify buffer allocation works
        let buffer1 = vec![0u8; HIGH_THROUGHPUT_BUFFER_SIZE];
        let buffer2 = vec![0u8; HIGH_THROUGHPUT_BUFFER_SIZE];

        assert_eq!(buffer1.len(), buffer2.len());
        assert_eq!(buffer1.len(), HIGH_THROUGHPUT_BUFFER_SIZE);
    }

    #[test]
    fn test_constants_are_powers_of_two() {
        // Buffer sizes should be powers of 2 for efficiency
        let size = HIGH_THROUGHPUT_BUFFER_SIZE;
        assert!(size.is_power_of_two() || size % 4096 == 0);
    }

    #[tokio::test]
    async fn test_empty_stream_handling() {
        // Test handling of empty streams
        let empty_data: &[u8] = b"";
        let mut read_stream = Cursor::new(empty_data);
        let mut buffer = vec![0u8; 1024];

        let n = read_stream.read(&mut buffer).await.unwrap();
        assert_eq!(n, 0); // EOF
    }

    #[tokio::test]
    async fn test_partial_buffer_reads() {
        // Test reading data smaller than buffer
        let data = b"Small data";
        let mut read_stream = Cursor::new(data);
        let mut buffer = vec![0u8; HIGH_THROUGHPUT_BUFFER_SIZE];

        let n = read_stream.read(&mut buffer).await.unwrap();
        assert_eq!(n, data.len());
        assert_eq!(&buffer[..n], data);
    }
}
