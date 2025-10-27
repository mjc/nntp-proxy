//! Network optimization traits and implementations
//!
//! This module provides a trait-based approach to network optimizations,
//! allowing different optimization strategies for TCP and TLS connections.

use crate::stream::ConnectionStream;
use std::io;
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use tracing::debug;

/// Trait for network optimization strategies
pub trait NetworkOptimizer {
    /// Apply optimizations to improve network performance
    fn optimize(&self) -> Result<(), io::Error>;

    /// Get a description of the optimization strategy
    fn description(&self) -> &'static str;
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

    /// Create a TCP optimizer with custom buffer sizes
    pub fn with_buffer_sizes(stream: &'a TcpStream, recv_size: usize, send_size: usize) -> Self {
        Self {
            stream,
            recv_buffer_size: recv_size,
            send_buffer_size: send_size,
        }
    }
}

impl<'a> NetworkOptimizer for TcpOptimizer<'a> {
    fn optimize(&self) -> Result<(), io::Error> {
        use socket2::SockRef;
        use std::time::Duration;

        let sock_ref = SockRef::from(self.stream);

        // Set larger buffer sizes for high throughput
        sock_ref.set_recv_buffer_size(self.recv_buffer_size)?;
        sock_ref.set_send_buffer_size(self.send_buffer_size)?;

        // Prevent indefinite blocking on close with pending data
        // Works on: Linux, macOS, Windows
        sock_ref.set_linger(Some(Duration::from_secs(5)))?;

        // Linux-specific optimizations
        #[cfg(target_os = "linux")]
        {
            // Faster dead connection detection - abort if no ACK for 30 seconds
            // instead of default ~15 minutes
            if let Err(e) = sock_ref.set_tcp_user_timeout(Some(Duration::from_secs(30))) {
                debug!("Failed to set TCP_USER_TIMEOUT: {}", e);
            }

            // QoS marking - optimize for throughput
            // Most networks ignore this, but no harm in setting
            // Note: IPv4 only, IPv6 uses set_tos_v6
            if let Err(e) = sock_ref.set_tos_v4(0x08) {
                debug!("Failed to set IP_TOS: {}", e);
            }
        }

        // Windows-specific optimizations
        #[cfg(target_os = "windows")]
        {
            // On Windows, we might want to disable SIO_LOOPBACK_FAST_PATH for localhost
            // but socket2 doesn't expose this, so we'll skip it for now
        }

        debug!(
            "Applied TCP optimizations: recv_buffer={}, send_buffer={}, linger=5s{}{}",
            self.recv_buffer_size,
            self.send_buffer_size,
            if cfg!(target_os = "linux") {
                ", tcp_user_timeout=30s, tos=0x08"
            } else {
                ""
            },
            if cfg!(target_os = "windows") {
                " (Windows)"
            } else {
                ""
            }
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

    /// Create a TLS optimizer with custom buffer sizes
    pub fn with_buffer_sizes(
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
    fn optimize(&self) -> Result<(), io::Error> {
        use socket2::SockRef;
        use std::time::Duration;

        // Get the underlying TCP stream for optimization
        let tcp_stream = self.stream.get_ref().0;
        let sock_ref = SockRef::from(tcp_stream);

        // Apply TCP-level optimizations to the underlying stream
        sock_ref.set_recv_buffer_size(self.recv_buffer_size)?;
        sock_ref.set_send_buffer_size(self.send_buffer_size)?;

        // Prevent indefinite blocking on close with pending data
        // Works on: Linux, macOS, Windows
        sock_ref.set_linger(Some(Duration::from_secs(5)))?;

        // Linux-specific optimizations
        #[cfg(target_os = "linux")]
        {
            // Faster dead connection detection
            if let Err(e) = sock_ref.set_tcp_user_timeout(Some(Duration::from_secs(30))) {
                debug!("Failed to set TCP_USER_TIMEOUT on TLS stream: {}", e);
            }

            // QoS marking (IPv4 only)
            if let Err(e) = sock_ref.set_tos_v4(0x08) {
                debug!("Failed to set IP_TOS on TLS stream: {}", e);
            }
        }

        debug!(
            "Applied TLS optimizations to underlying TCP stream: recv_buffer={}, send_buffer={}, linger=5s{}",
            self.recv_buffer_size,
            self.send_buffer_size,
            if cfg!(target_os = "linux") {
                ", tcp_user_timeout=30s, tos=0x08"
            } else {
                ""
            }
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
    fn optimize(&self) -> Result<(), io::Error> {
        match (self.recv_buffer_size, self.send_buffer_size) {
            (Some(recv), Some(send)) => {
                // Use custom buffer sizes
                match self.stream {
                    ConnectionStream::Plain(tcp) => {
                        let optimizer = TcpOptimizer::with_buffer_sizes(tcp, recv, send);
                        debug!("Using {} with custom buffers", optimizer.description());
                        optimizer.optimize()
                    }
                    ConnectionStream::Tls(tls) => {
                        let optimizer = TlsOptimizer::with_buffer_sizes(tls.as_ref(), recv, send);
                        debug!("Using {} with custom buffers", optimizer.description());
                        optimizer.optimize()
                    }
                }
            }
            _ => {
                // Use default buffer sizes
                match self.stream {
                    ConnectionStream::Plain(tcp) => {
                        let optimizer = TcpOptimizer::new(tcp);
                        debug!("Using {}", optimizer.description());
                        optimizer.optimize()
                    }
                    ConnectionStream::Tls(tls) => {
                        let optimizer = TlsOptimizer::new(tls.as_ref());
                        debug!("Using {}", optimizer.description());
                        optimizer.optimize()
                    }
                }
            }
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
    use tokio::net::TcpListener;

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
}
