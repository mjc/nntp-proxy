//! Network socket optimization utilities
//!
//! This module provides utilities for optimizing TCP socket performance
//! for high-throughput NNTP transfers.

use std::io;
use tokio::net::TcpStream;
use tracing::debug;
use crate::constants::socket::{HIGH_THROUGHPUT_RECV_BUFFER, HIGH_THROUGHPUT_SEND_BUFFER};

/// Socket optimizer for high-throughput scenarios
pub struct SocketOptimizer;

impl SocketOptimizer {
    /// Set socket optimizations for high-throughput transfers using socket2
    pub fn optimize_for_throughput(stream: &TcpStream) -> Result<(), io::Error> {
        use socket2::SockRef;

        let sock_ref = SockRef::from(stream);

        // Set larger buffer sizes for high throughput
        sock_ref.set_recv_buffer_size(HIGH_THROUGHPUT_RECV_BUFFER)?;
        sock_ref.set_send_buffer_size(HIGH_THROUGHPUT_SEND_BUFFER)?;

        // Keep Nagle's algorithm enabled for large transfers to reduce packet overhead
        // (socket2 doesn't expose some advanced TCP options like TCP_QUICKACK, TCP_CORK)
        // but the basic optimizations are sufficient for most use cases

        Ok(())
    }

    /// Apply aggressive socket optimizations for 1GB+ transfers
    pub fn apply_to_streams(
        client_stream: &TcpStream,
        backend_stream: &TcpStream,
    ) -> Result<(), io::Error> {
        debug!("Applying high-throughput socket optimizations");

        if let Err(e) = Self::optimize_for_throughput(client_stream) {
            debug!("Failed to set client socket optimizations: {}", e);
        }
        if let Err(e) = Self::optimize_for_throughput(backend_stream) {
            debug!("Failed to set backend socket optimizations: {}", e);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_constants() {
        assert_eq!(HIGH_THROUGHPUT_RECV_BUFFER, 16 * 1024 * 1024);
        assert_eq!(HIGH_THROUGHPUT_SEND_BUFFER, 16 * 1024 * 1024);
    }

    #[test]
    fn test_buffer_size_is_reasonable() {
        // Buffer sizes should be large but not excessive
        // Compile-time assertions
        const _: () = assert!(HIGH_THROUGHPUT_RECV_BUFFER >= 1024 * 1024); // At least 1MB
        const _: () = assert!(HIGH_THROUGHPUT_RECV_BUFFER <= 128 * 1024 * 1024); // At most 128MB

        const _: () = assert!(HIGH_THROUGHPUT_SEND_BUFFER >= 1024 * 1024);
        const _: () = assert!(HIGH_THROUGHPUT_SEND_BUFFER <= 128 * 1024 * 1024);
    }

    #[test]
    fn test_buffer_sizes_are_equal() {
        // Send and receive buffers should be the same for bidirectional transfers
        assert_eq!(HIGH_THROUGHPUT_RECV_BUFFER, HIGH_THROUGHPUT_SEND_BUFFER);
    }

    #[test]
    fn test_buffer_sizes_are_power_of_two_or_multiple() {
        // Should be aligned to reasonable boundaries
        let size = HIGH_THROUGHPUT_RECV_BUFFER;

        // Should be a multiple of 1MB for efficient allocation
        assert_eq!(size % (1024 * 1024), 0);
    }

    #[test]
    fn test_socket_optimizer_exists() {
        // Verify SocketOptimizer can be instantiated
        let _ = SocketOptimizer;
    }

    #[tokio::test]
    async fn test_optimize_for_throughput_with_real_socket() {
        // Create a real TCP listener and connection
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Connect to it
        let client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        // Try to optimize (might fail on some systems, but shouldn't panic)
        let result = SocketOptimizer::optimize_for_throughput(&client_stream);

        // On most systems this should succeed, but some might not support large buffers
        // The important thing is it doesn't panic
        match result {
            Ok(()) => {
                // Success - verify we can still use the socket
                assert!(client_stream.peer_addr().is_ok());
            }
            Err(e) => {
                // Some systems might not support these buffer sizes
                println!(
                    "Buffer size not supported (expected on some systems): {}",
                    e
                );
            }
        }
    }

    #[tokio::test]
    async fn test_apply_to_streams() {
        // Create two TCP connections
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server_stream, _) = listener.accept().await.unwrap();

        // Apply optimizations to both streams
        let result = SocketOptimizer::apply_to_streams(&client_stream, &server_stream);

        // Should always succeed (errors are logged but not propagated)
        assert!(result.is_ok());

        // Streams should still be usable
        assert!(client_stream.peer_addr().is_ok());
        assert!(server_stream.peer_addr().is_ok());
    }

    #[test]
    fn test_buffer_size_calculation() {
        // Verify buffer sizes are calculated correctly
        assert_eq!(HIGH_THROUGHPUT_RECV_BUFFER, 16 * 1024 * 1024);

        // Verify it's 16MB in bytes
        assert_eq!(HIGH_THROUGHPUT_RECV_BUFFER, 16_777_216);

        // Verify relationship to KB/MB
        assert_eq!(HIGH_THROUGHPUT_RECV_BUFFER / 1024, 16384); // KB
        assert_eq!(HIGH_THROUGHPUT_RECV_BUFFER / (1024 * 1024), 16); // MB
    }

    #[test]
    fn test_buffer_size_for_large_articles() {
        // Typical large Usenet article is 1-100MB
        // Our 16MB buffer should handle most efficiently
        let typical_large_article = 10 * 1024 * 1024; // 10MB
        let very_large_article = 100 * 1024 * 1024; // 100MB

        // Buffer should be larger than typical article
        assert!(HIGH_THROUGHPUT_RECV_BUFFER > typical_large_article);

        // But we accept that very large articles will require multiple buffers
        assert!(HIGH_THROUGHPUT_RECV_BUFFER < very_large_article);
    }
}
