//! Network socket optimization utilities
//!
//! This module provides utilities for optimizing TCP socket performance
//! for high-throughput NNTP transfers.
//!
//! The module is organized into:
//! - `optimizers`: Trait-based optimization strategies for different connection types
//! - Legacy `SocketOptimizer`: Maintained for backward compatibility

pub mod optimizers;

use crate::constants::socket::{HIGH_THROUGHPUT_RECV_BUFFER, HIGH_THROUGHPUT_SEND_BUFFER};
use crate::stream::ConnectionStream;
use std::io;
use tokio::net::TcpStream;
use tracing::debug;

// Re-export the new optimizers for easier access
pub use optimizers::{ConnectionOptimizer, NetworkOptimizer, TcpOptimizer, TlsOptimizer};

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

    /// Apply socket optimizations to ConnectionStream (extracts TCP stream if available)
    ///
    /// # Deprecated
    /// This method is maintained for backward compatibility.
    /// New code should use `ConnectionOptimizer::new(stream).optimize()`.
    pub fn optimize_connection_stream(stream: &ConnectionStream) -> Result<(), io::Error> {
        let optimizer = ConnectionOptimizer::new(stream);
        optimizer.optimize()
    }

    /// Apply aggressive socket optimizations for 1GB+ transfers
    /// Works with both TcpStream and ConnectionStream
    ///
    /// # Deprecated
    /// This method is maintained for backward compatibility.
    /// New code should use `ConnectionOptimizer` for better separation of concerns.
    pub fn apply_to_streams(
        client_stream: &TcpStream,
        backend_stream: &crate::stream::ConnectionStream,
    ) -> Result<(), io::Error> {
        debug!("Applying high-throughput socket optimizations (legacy method)");

        // Use the new trait-based approach internally
        let client_optimizer = TcpOptimizer::new(client_stream);
        if let Err(e) = client_optimizer.optimize() {
            debug!("Failed to set client socket optimizations: {}", e);
        }

        let backend_optimizer = ConnectionOptimizer::new(backend_stream);
        if let Err(e) = backend_optimizer.optimize() {
            debug!("Failed to set backend socket optimizations: {}", e);
        }

        Ok(())
    }

    /// Apply optimizations using the new trait-based approach (recommended)
    pub fn apply_to_connection_streams(
        client_stream: &ConnectionStream,
        backend_stream: &ConnectionStream,
    ) -> Result<(), io::Error> {
        debug!("Applying connection optimizations with trait-based approach");

        let client_optimizer = ConnectionOptimizer::new(client_stream);
        let backend_optimizer = ConnectionOptimizer::new(backend_stream);

        client_optimizer.optimize()?;
        backend_optimizer.optimize()?;

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
        use crate::stream::ConnectionStream;

        // Create two TCP connections
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server_tcp, _) = listener.accept().await.unwrap();
        let server_stream = ConnectionStream::plain(server_tcp);

        // Apply optimizations to both streams
        let result = SocketOptimizer::apply_to_streams(&client_stream, &server_stream);

        // Should always succeed (errors are logged but not propagated)
        assert!(result.is_ok());

        // Streams should still be usable
        assert!(client_stream.peer_addr().is_ok());
        assert!(server_stream.as_tcp_stream().unwrap().peer_addr().is_ok());
    }

    #[tokio::test]
    async fn test_optimize_connection_stream() {
        use crate::stream::ConnectionStream;

        // Create a test server and connection
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let tcp_stream = std::net::TcpStream::connect(addr).unwrap();
        tcp_stream.set_nonblocking(true).unwrap();
        let tokio_stream = TcpStream::from_std(tcp_stream).unwrap();

        let conn_stream = ConnectionStream::plain(tokio_stream);

        // Should successfully optimize ConnectionStream
        let result = SocketOptimizer::optimize_connection_stream(&conn_stream);
        assert!(result.is_ok());
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

    #[tokio::test]
    async fn test_new_trait_based_approach() {
        use crate::stream::ConnectionStream;

        // Create two TCP connections
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client_tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let (server_tcp, _) = listener.accept().await.unwrap();

        let client_stream = ConnectionStream::plain(client_tcp);
        let server_stream = ConnectionStream::plain(server_tcp);

        // Use the new trait-based approach
        let result = SocketOptimizer::apply_to_connection_streams(&client_stream, &server_stream);
        assert!(result.is_ok());

        // Test individual optimizers
        let client_optimizer = ConnectionOptimizer::new(&client_stream);
        let server_optimizer = ConnectionOptimizer::new(&server_stream);

        assert!(client_optimizer.optimize().is_ok());
        assert!(server_optimizer.optimize().is_ok());
    }
}
