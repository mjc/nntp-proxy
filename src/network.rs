//! Network socket optimization utilities
//!
//! This module provides utilities for optimizing TCP socket performance
//! for high-throughput NNTP transfers.
//!
//! The module is organized into:
//! - `optimizers`: Trait-based optimization strategies for different connection types

pub mod optimizers;

// Re-export the optimizers for easier access
pub use optimizers::{ConnectionOptimizer, NetworkOptimizer, TcpOptimizer, TlsOptimizer};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::socket::{HIGH_THROUGHPUT_RECV_BUFFER, HIGH_THROUGHPUT_SEND_BUFFER};
    use tokio::net::TcpStream;

    #[test]
    fn test_constants() {
        assert_eq!(HIGH_THROUGHPUT_RECV_BUFFER, 16 * 1024 * 1024);
        assert_eq!(HIGH_THROUGHPUT_SEND_BUFFER, 16 * 1024 * 1024);
    }

    #[test]
    fn test_buffer_size_is_reasonable() {
        // Buffer sizes should be large but not excessive
        const _: () = assert!(HIGH_THROUGHPUT_RECV_BUFFER >= 1024 * 1024); // At least 1MB
        const _: () = assert!(HIGH_THROUGHPUT_RECV_BUFFER <= 128 * 1024 * 1024); // At most 128MB
        const _: () = assert!(HIGH_THROUGHPUT_SEND_BUFFER >= 1024 * 1024);
        const _: () = assert!(HIGH_THROUGHPUT_SEND_BUFFER <= 128 * 1024 * 1024);
    }

    #[test]
    fn test_buffer_sizes_are_equal() {
        assert_eq!(HIGH_THROUGHPUT_RECV_BUFFER, HIGH_THROUGHPUT_SEND_BUFFER);
    }

    #[test]
    fn test_buffer_sizes_are_power_of_two_or_multiple() {
        let size = HIGH_THROUGHPUT_RECV_BUFFER;
        assert_eq!(size % (1024 * 1024), 0);
    }

    #[tokio::test]
    async fn test_connection_optimizer() {
        use crate::stream::ConnectionStream;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let tcp_stream = std::net::TcpStream::connect(addr).unwrap();
        tcp_stream.set_nonblocking(true).unwrap();
        let tokio_stream = TcpStream::from_std(tcp_stream).unwrap();

        let conn_stream = ConnectionStream::plain(tokio_stream);

        let optimizer = ConnectionOptimizer::new(&conn_stream);
        let result = optimizer.optimize();
        assert!(result.is_ok());
    }

    #[test]
    fn test_buffer_size_calculation() {
        assert_eq!(HIGH_THROUGHPUT_RECV_BUFFER, 16 * 1024 * 1024);
        assert_eq!(HIGH_THROUGHPUT_RECV_BUFFER, 16_777_216);
        assert_eq!(HIGH_THROUGHPUT_RECV_BUFFER / 1024, 16384); // KB
        assert_eq!(HIGH_THROUGHPUT_RECV_BUFFER / (1024 * 1024), 16); // MB
    }

    #[test]
    fn test_buffer_size_for_large_articles() {
        let typical_large_article = 10 * 1024 * 1024; // 10MB
        let very_large_article = 100 * 1024 * 1024; // 100MB
        assert!(HIGH_THROUGHPUT_RECV_BUFFER > typical_large_article);
        assert!(HIGH_THROUGHPUT_RECV_BUFFER < very_large_article);
    }
}
