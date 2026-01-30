//! Network socket optimization utilities
//!
//! This module provides utilities for optimizing TCP socket performance
//! for high-throughput NNTP transfers.
//!
//! Includes trait-based optimization strategies for different connection types
//! (plain TCP, TLS) with configurable buffer sizes.

use crate::stream::ConnectionStream;
use crate::tls::TlsStream;
use anyhow::{Context, Result};
use socket2::SockRef;
use std::time::Duration;
use tokio::net::TcpStream;
use tracing::debug;

/// SO_LINGER timeout - prevents indefinite blocking on socket close
const LINGER_TIMEOUT: Duration = Duration::from_secs(5);

/// TCP_USER_TIMEOUT - faster dead connection detection on Linux
const TCP_USER_TIMEOUT: Duration = Duration::from_secs(30);

/// IP_TOS value for throughput optimization
const TOS_THROUGHPUT: u32 = 0x08;

/// Trait for network optimization strategies
pub trait NetworkOptimizer {
    /// Apply optimizations to improve network performance
    fn optimize(&self) -> Result<()>;

    /// Get a description of the optimization strategy
    fn description(&self) -> &'static str;
}

/// Apply core TCP optimizations to a socket reference
fn apply_core_optimizations(
    sock_ref: &SockRef,
    recv_buffer_size: usize,
    send_buffer_size: usize,
) -> Result<()> {
    sock_ref
        .set_recv_buffer_size(recv_buffer_size)
        .context("Failed to set TCP receive buffer size")?;

    sock_ref
        .set_send_buffer_size(send_buffer_size)
        .context("Failed to set TCP send buffer size")?;

    sock_ref
        .set_linger(Some(LINGER_TIMEOUT))
        .context("Failed to set SO_LINGER timeout")?;

    sock_ref
        .set_tcp_nodelay(true)
        .context("Failed to set TCP_NODELAY")?;

    Ok(())
}

/// Apply Linux-specific TCP optimizations (best-effort)
#[cfg(target_os = "linux")]
fn apply_linux_optimizations(sock_ref: &SockRef, context: &str) {
    [
        (
            "TCP_USER_TIMEOUT",
            sock_ref.set_tcp_user_timeout(Some(TCP_USER_TIMEOUT)),
        ),
        ("IP_TOS", sock_ref.set_tos_v4(TOS_THROUGHPUT)),
    ]
    .into_iter()
    .filter_map(|(name, result)| result.err().map(|e| (name, e)))
    .for_each(|(name, err)| {
        debug!("Failed to set {} on {}: {}", name, context, err);
    });
}

/// Get platform-specific optimization description
const fn platform_optimization_desc() -> &'static str {
    match () {
        #[cfg(target_os = "linux")]
        () => ", tcp_user_timeout=30s, tos=0x08",
        #[cfg(target_os = "windows")]
        () => " (Windows)",
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        () => "",
    }
}

/// TCP-specific optimizations for high-throughput scenarios
pub struct TcpOptimizer<'a> {
    stream: &'a TcpStream,
    recv_buffer_size: usize,
    send_buffer_size: usize,
}

impl<'a> TcpOptimizer<'a> {
    /// Create a new TCP optimizer with default high-throughput settings
    pub fn new(stream: &'a TcpStream) -> Self {
        Self {
            stream,
            recv_buffer_size: crate::constants::socket::HIGH_THROUGHPUT_RECV_BUFFER,
            send_buffer_size: crate::constants::socket::HIGH_THROUGHPUT_SEND_BUFFER,
        }
    }

    /// Create optimizer with custom buffer sizes using builder pattern
    pub const fn with_buffer_sizes(
        stream: &'a TcpStream,
        recv_size: usize,
        send_size: usize,
    ) -> Self {
        Self {
            stream,
            recv_buffer_size: recv_size,
            send_buffer_size: send_size,
        }
    }
}

impl<'a> NetworkOptimizer for TcpOptimizer<'a> {
    fn optimize(&self) -> Result<()> {
        let sock_ref = SockRef::from(self.stream);

        // Core optimizations (required)
        apply_core_optimizations(&sock_ref, self.recv_buffer_size, self.send_buffer_size)
            .context("Failed to apply core TCP optimizations")?;

        // Platform-specific optimizations (best-effort)
        #[cfg(target_os = "linux")]
        apply_linux_optimizations(&sock_ref, "TCP stream");

        debug!(
            "Applied TCP optimizations: recv_buffer={}, send_buffer={}, linger={}s, nodelay=true{}",
            self.recv_buffer_size,
            self.send_buffer_size,
            LINGER_TIMEOUT.as_secs(),
            platform_optimization_desc()
        );

        Ok(())
    }

    fn description(&self) -> &'static str {
        "TCP high-throughput optimization"
    }
}

/// TLS-specific optimizations that work on the underlying TCP stream
pub struct TlsOptimizer<'a> {
    stream: &'a TlsStream<TcpStream>,
    recv_buffer_size: usize,
    send_buffer_size: usize,
}

impl<'a> TlsOptimizer<'a> {
    /// Create a new TLS optimizer with default settings
    pub fn new(stream: &'a TlsStream<TcpStream>) -> Self {
        Self {
            stream,
            recv_buffer_size: crate::constants::socket::HIGH_THROUGHPUT_RECV_BUFFER,
            send_buffer_size: crate::constants::socket::HIGH_THROUGHPUT_SEND_BUFFER,
        }
    }

    /// Create optimizer with custom buffer sizes using builder pattern
    pub const fn with_buffer_sizes(
        stream: &'a TlsStream<TcpStream>,
        recv_size: usize,
        send_size: usize,
    ) -> Self {
        Self {
            stream,
            recv_buffer_size: recv_size,
            send_buffer_size: send_size,
        }
    }
}

impl<'a> NetworkOptimizer for TlsOptimizer<'a> {
    fn optimize(&self) -> Result<()> {
        // Get the underlying TCP stream for optimization
        let tcp_stream = self.stream.get_ref().0;
        let sock_ref = SockRef::from(tcp_stream);

        // Core optimizations (required)
        apply_core_optimizations(&sock_ref, self.recv_buffer_size, self.send_buffer_size)
            .context("Failed to apply core TCP optimizations to TLS stream")?;

        // Platform-specific optimizations (best-effort)
        #[cfg(target_os = "linux")]
        apply_linux_optimizations(&sock_ref, "TLS stream");

        debug!(
            "Applied TLS optimizations to underlying TCP stream: recv_buffer={}, send_buffer={}, linger={}s, nodelay=true{}",
            self.recv_buffer_size,
            self.send_buffer_size,
            LINGER_TIMEOUT.as_secs(),
            platform_optimization_desc()
        );

        Ok(())
    }

    fn description(&self) -> &'static str {
        "TLS optimization via underlying TCP stream"
    }
}

/// High-level optimizer that works with ConnectionStream
pub struct ConnectionOptimizer<'a> {
    stream: &'a ConnectionStream,
    recv_buffer_size: Option<usize>,
    send_buffer_size: Option<usize>,
}

impl<'a> ConnectionOptimizer<'a> {
    /// Create a new connection optimizer with default buffer sizes
    pub fn new(stream: &'a ConnectionStream) -> Self {
        Self {
            stream,
            recv_buffer_size: None,
            send_buffer_size: None,
        }
    }

    /// Create a connection optimizer with custom buffer sizes
    pub fn with_buffer_sizes(
        stream: &'a ConnectionStream,
        recv_size: usize,
        send_size: usize,
    ) -> Self {
        Self {
            stream,
            recv_buffer_size: Some(recv_size),
            send_buffer_size: Some(send_size),
        }
    }
}

impl<'a> NetworkOptimizer for ConnectionOptimizer<'a> {
    fn optimize(&self) -> Result<()> {
        // Use functional pattern matching to create and optimize in one step
        let optimize_fn = |desc: &str, result: Result<()>| {
            debug!("Using {}", desc);
            result
        };

        match (self.recv_buffer_size, self.send_buffer_size, self.stream) {
            // Custom buffer sizes
            (Some(recv), Some(send), ConnectionStream::Plain(tcp)) => optimize_fn(
                "TCP high-throughput optimization with custom buffers",
                TcpOptimizer::with_buffer_sizes(tcp, recv, send).optimize(),
            ),
            (Some(recv), Some(send), ConnectionStream::Tls(tls)) => optimize_fn(
                "TLS optimization via underlying TCP stream with custom buffers",
                TlsOptimizer::with_buffer_sizes(tls.as_ref(), recv, send).optimize(),
            ),
            // Default buffer sizes
            (_, _, ConnectionStream::Plain(tcp)) => optimize_fn(
                "TCP high-throughput optimization",
                TcpOptimizer::new(tcp).optimize(),
            ),
            (_, _, ConnectionStream::Tls(tls)) => optimize_fn(
                "TLS optimization via underlying TCP stream",
                TlsOptimizer::new(tls.as_ref()).optimize(),
            ),
        }
    }

    fn description(&self) -> &'static str {
        match self.stream {
            ConnectionStream::Plain(_) => "Connection-level TCP optimization",
            ConnectionStream::Tls(_) => "Connection-level TLS optimization",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::socket::{HIGH_THROUGHPUT_RECV_BUFFER, HIGH_THROUGHPUT_SEND_BUFFER};
    use tokio::net::TcpListener;

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

    #[tokio::test]
    async fn test_tcp_optimizer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let optimizer = TcpOptimizer::new(&stream);

        assert_eq!(optimizer.description(), "TCP high-throughput optimization");

        // Should not panic - actual socket optimization might fail in test environment
        let _ = optimizer.optimize();
    }

    #[tokio::test]
    async fn test_connection_optimizer_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let tcp_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let connection_stream = ConnectionStream::Plain(tcp_stream);
        let optimizer = ConnectionOptimizer::new(&connection_stream);

        assert_eq!(optimizer.description(), "Connection-level TCP optimization");

        // Should not panic - actual socket optimization might fail in test environment
        let _ = optimizer.optimize();
    }

    #[tokio::test]
    async fn test_connection_optimizer_trait_usage() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let tcp_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let connection_stream = ConnectionStream::Plain(tcp_stream);

        // Test that ConnectionOptimizer implements NetworkOptimizer trait
        let optimizer: Box<dyn NetworkOptimizer> =
            Box::new(ConnectionOptimizer::new(&connection_stream));

        assert_eq!(optimizer.description(), "Connection-level TCP optimization");
        let _ = optimizer.optimize();
    }

    #[tokio::test]
    async fn test_connection_optimizer_with_custom_buffers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let tcp_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let connection_stream = ConnectionStream::Plain(tcp_stream);
        let optimizer = ConnectionOptimizer::with_buffer_sizes(&connection_stream, 4096, 8192);

        assert_eq!(optimizer.description(), "Connection-level TCP optimization");
        let _ = optimizer.optimize();
    }

    #[tokio::test]
    async fn test_optimizer_creation() {
        // Test that we can create optimizers with tokio streams
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let tokio_stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        let optimizer = TcpOptimizer::new(&tokio_stream);
        assert_eq!(
            optimizer.recv_buffer_size,
            crate::constants::socket::HIGH_THROUGHPUT_RECV_BUFFER
        );
        assert_eq!(
            optimizer.send_buffer_size,
            crate::constants::socket::HIGH_THROUGHPUT_SEND_BUFFER
        );
    }

    #[tokio::test]
    async fn test_custom_buffer_sizes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let tokio_stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        let optimizer = TcpOptimizer::with_buffer_sizes(&tokio_stream, 1024, 2048);
        assert_eq!(optimizer.recv_buffer_size, 1024);
        assert_eq!(optimizer.send_buffer_size, 2048);
    }

    // Platform-specific description tests

    #[test]
    fn test_platform_optimization_desc() {
        let desc = platform_optimization_desc();
        // Check that it returns a valid string (platform-specific content)
        #[cfg(target_os = "linux")]
        assert!(desc.contains("tcp_user_timeout"));
        #[cfg(target_os = "windows")]
        assert!(desc.contains("Windows"));
        // Just verify it doesn't panic on other platforms
        #[cfg(not(any(target_os = "linux", target_os = "windows")))]
        let _ = desc;
    }

    // Constructor tests

    #[tokio::test]
    async fn test_tcp_optimizer_new_uses_defaults() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        let optimizer = TcpOptimizer::new(&stream);

        assert_eq!(
            optimizer.recv_buffer_size,
            crate::constants::socket::HIGH_THROUGHPUT_RECV_BUFFER
        );
        assert_eq!(
            optimizer.send_buffer_size,
            crate::constants::socket::HIGH_THROUGHPUT_SEND_BUFFER
        );
    }

    #[tokio::test]
    async fn test_tcp_optimizer_with_custom_buffers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        let recv_size = 512 * 1024;
        let send_size = 1024 * 1024;
        let optimizer = TcpOptimizer::with_buffer_sizes(&stream, recv_size, send_size);

        assert_eq!(optimizer.recv_buffer_size, recv_size);
        assert_eq!(optimizer.send_buffer_size, send_size);
    }

    // Description tests

    #[tokio::test]
    async fn test_tcp_optimizer_description() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        let optimizer = TcpOptimizer::new(&stream);
        assert_eq!(optimizer.description(), "TCP high-throughput optimization");
    }

    #[tokio::test]
    async fn test_connection_optimizer_description_tcp() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let stream = ConnectionStream::Plain(tcp);

        let optimizer = ConnectionOptimizer::new(&stream);
        assert_eq!(optimizer.description(), "Connection-level TCP optimization");
    }

    // ConnectionOptimizer buffer configuration tests

    #[tokio::test]
    async fn test_connection_optimizer_new_no_custom_buffers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let stream = ConnectionStream::Plain(tcp);

        let optimizer = ConnectionOptimizer::new(&stream);
        assert!(optimizer.recv_buffer_size.is_none());
        assert!(optimizer.send_buffer_size.is_none());
    }

    #[tokio::test]
    async fn test_connection_optimizer_with_custom_buffers_sets_sizes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let stream = ConnectionStream::Plain(tcp);

        let recv = 16384;
        let send = 32768;
        let optimizer = ConnectionOptimizer::with_buffer_sizes(&stream, recv, send);
        assert_eq!(optimizer.recv_buffer_size, Some(recv));
        assert_eq!(optimizer.send_buffer_size, Some(send));
    }

    // Edge case tests

    #[tokio::test]
    async fn test_tcp_optimizer_with_zero_buffers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        // Zero buffer sizes are technically allowed, though not recommended
        let optimizer = TcpOptimizer::with_buffer_sizes(&stream, 0, 0);
        assert_eq!(optimizer.recv_buffer_size, 0);
        assert_eq!(optimizer.send_buffer_size, 0);
    }

    #[tokio::test]
    async fn test_tcp_optimizer_with_very_large_buffers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        // Very large buffer sizes (system may reject these, but we test the constructor)
        let large_size = 128 * 1024 * 1024; // 128MB
        let optimizer = TcpOptimizer::with_buffer_sizes(&stream, large_size, large_size);
        assert_eq!(optimizer.recv_buffer_size, large_size);
        assert_eq!(optimizer.send_buffer_size, large_size);
    }

    #[tokio::test]
    async fn test_tcp_optimizer_with_asymmetric_buffers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        // Asymmetric buffers (common for download-heavy workloads)
        let recv_size = 32 * 1024 * 1024; // 32MB
        let send_size = 1024 * 1024; // 1MB
        let optimizer = TcpOptimizer::with_buffer_sizes(&stream, recv_size, send_size);
        assert_eq!(optimizer.recv_buffer_size, recv_size);
        assert_eq!(optimizer.send_buffer_size, send_size);
    }

    // NetworkOptimizer trait tests

    #[tokio::test]
    async fn test_tcp_optimizer_implements_network_optimizer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        let optimizer = TcpOptimizer::new(&stream);
        // Can be used as a trait object
        let _trait_obj: &dyn NetworkOptimizer = &optimizer;
    }

    #[tokio::test]
    async fn test_connection_optimizer_implements_network_optimizer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let stream = ConnectionStream::Plain(tcp);

        let optimizer = ConnectionOptimizer::new(&stream);
        // Can be used as a trait object
        let _trait_obj: &dyn NetworkOptimizer = &optimizer;
    }

    // Constants tests

    #[test]
    fn test_linger_timeout_constant() {
        assert_eq!(LINGER_TIMEOUT, Duration::from_secs(5));
    }

    #[test]
    fn test_tcp_user_timeout_constant() {
        assert_eq!(TCP_USER_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn test_tos_throughput_constant() {
        assert_eq!(TOS_THROUGHPUT, 0x08);
    }
}
