//! Stream abstraction for supporting multiple connection types
//!
//! This module provides abstractions for handling different stream types (TCP, TLS, etc.)
//! in a unified way. This is preparation for adding SSL/TLS support to backend connections.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

/// Trait for async streams that can be used for NNTP connections
///
/// This trait is automatically implemented for any type that implements
/// AsyncRead + AsyncWrite + Unpin + Send, making it easy to support
/// different connection types (TCP, TLS, etc.).
pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}

// Blanket implementation for all types that meet the requirements
impl<T> AsyncStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

/// Unified stream type that can represent different connection types
///
/// This enum allows the proxy to handle both plain TCP and future TLS connections
/// through a single type, avoiding the need for trait objects and their associated
/// heap allocation overhead.
///
/// # Future SSL Support
///
/// When adding SSL/TLS support, add a new variant:
/// ```ignore
/// Tls(tokio_rustls::TlsStream<TcpStream>),
/// ```
/// TODO(SSL): Add Tls variant here - see SSL_IMPLEMENTATION.md for details
#[derive(Debug)]
pub enum ConnectionStream {
    /// Plain TCP connection
    Plain(TcpStream),
    // TODO(SSL): Uncomment when implementing TLS:
    // Tls(tokio_rustls::TlsStream<TcpStream>),
}

impl ConnectionStream {
    /// Create a new plain TCP connection stream
    pub fn plain(stream: TcpStream) -> Self {
        Self::Plain(stream)
    }

    /// Get a reference to the underlying TCP stream (if plain TCP)
    ///
    /// Returns None for TLS streams, as the TCP stream is wrapped.
    /// Useful for socket optimization that requires direct TCP access.
    pub fn as_tcp_stream(&self) -> Option<&TcpStream> {
        match self {
            Self::Plain(tcp) => Some(tcp),
            // Future TLS variant would return None or use get_ref()
        }
    }

    /// Get a mutable reference to the underlying TCP stream (if plain TCP)
    pub fn as_tcp_stream_mut(&mut self) -> Option<&mut TcpStream> {
        match self {
            Self::Plain(tcp) => Some(tcp),
        }
    }

    /// Returns true if this is a plain TCP connection
    pub fn is_plain_tcp(&self) -> bool {
        matches!(self, Self::Plain(_))
    }

    /// Returns true if this is a TLS connection (currently always false)
    pub fn is_tls(&self) -> bool {
        false // Will be updated when TLS variant is added
    }
}

impl AsyncRead for ConnectionStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            // Future: Self::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for ConnectionStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            // Future: Self::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            // Future: Self::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            // Future: Self::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn test_connection_stream_plain_tcp() {
        // Create a listener and client
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client_handle = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });

        let (server_stream, _) = listener.accept().await.unwrap();
        let client_stream = client_handle.await.unwrap();

        // Wrap in ConnectionStream
        let mut server_conn = ConnectionStream::plain(server_stream);
        let mut client_conn = ConnectionStream::plain(client_stream);

        // Test writing and reading
        client_conn.write_all(b"Hello").await.unwrap();
        client_conn.flush().await.unwrap();

        let mut buf = [0u8; 5];
        server_conn.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"Hello");

        // Test stream type checking
        assert!(client_conn.is_plain_tcp());
        assert!(!client_conn.is_tls());
        assert!(client_conn.as_tcp_stream().is_some());
    }

    #[test]
    fn test_async_stream_trait() {
        // Verify TcpStream implements AsyncStream
        fn assert_async_stream<T: AsyncStream>() {}
        assert_async_stream::<TcpStream>();
        assert_async_stream::<ConnectionStream>();
    }

    #[tokio::test]
    async fn test_connection_stream_tcp_access() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let tcp_stream = std::net::TcpStream::connect(addr).unwrap();
        tcp_stream.set_nonblocking(true).unwrap();
        let tokio_stream = TcpStream::from_std(tcp_stream).unwrap();

        let mut conn_stream = ConnectionStream::plain(tokio_stream);

        // Should be able to access underlying TCP stream
        assert!(conn_stream.as_tcp_stream().is_some());
        assert!(conn_stream.as_tcp_stream_mut().is_some());
    }
}
