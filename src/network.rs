//! Network socket optimization utilities
//!
//! This module provides utilities for optimizing TCP socket performance
//! for high-throughput NNTP transfers.

use std::io;
use tokio::net::TcpStream;
use tracing::debug;

/// TCP socket buffer sizes for high-throughput transfers
pub const HIGH_THROUGHPUT_RECV_BUFFER: usize = 16 * 1024 * 1024; // 16MB
pub const HIGH_THROUGHPUT_SEND_BUFFER: usize = 16 * 1024 * 1024; // 16MB

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
}
