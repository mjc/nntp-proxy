//! Stream abstraction for supporting multiple connection types
//!
//! This module provides abstractions for handling different stream types (TCP, TLS, etc.)
//! in a unified way. This is preparation for adding SSL/TLS support to backend connections.

use crate::tls::TlsStream;
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
/// This enum allows the proxy to handle both plain TCP and TLS connections
/// through a single type, avoiding the need for trait objects and their associated
/// heap allocation overhead.
#[derive(Debug)]
pub enum ConnectionStream {
    /// Plain TCP connection
    Plain(TcpStream),
    /// TLS-encrypted connection
    Tls(Box<TlsStream<TcpStream>>),
}

impl ConnectionStream {
    /// Create a new plain TCP connection stream
    pub fn plain(stream: TcpStream) -> Self {
        Self::Plain(stream)
    }

    /// Create a new TLS-encrypted connection stream
    pub fn tls(stream: TlsStream<TcpStream>) -> Self {
        Self::Tls(Box::new(stream))
    }

    /// Returns the connection type as a string for logging/debugging
    #[must_use]
    pub fn connection_type(&self) -> &'static str {
        match self {
            Self::Plain(_) => "TCP",
            Self::Tls(_) => "TLS",
        }
    }

    /// Returns true if this connection uses encryption (TLS/SSL)
    #[inline]
    #[must_use]
    pub const fn is_encrypted(&self) -> bool {
        matches!(self, Self::Tls(_))
    }

    /// Returns true if this connection is unencrypted (plain TCP)
    #[inline]
    #[must_use]
    pub const fn is_unencrypted(&self) -> bool {
        matches!(self, Self::Plain(_))
    }

    /// Get a reference to the underlying TCP stream (if plain TCP)
    ///
    /// Returns None for TLS streams, as the TCP stream is wrapped.
    /// Useful for socket optimization that requires direct TCP access.
    #[must_use]
    pub fn as_tcp_stream(&self) -> Option<&TcpStream> {
        match self {
            Self::Plain(tcp) => Some(tcp),
            Self::Tls(_) => None,
        }
    }

    /// Get a mutable reference to the underlying TCP stream (if plain TCP)
    pub fn as_tcp_stream_mut(&mut self) -> Option<&mut TcpStream> {
        match self {
            Self::Plain(tcp) => Some(tcp),
            Self::Tls(_) => None,
        }
    }

    /// Get a reference to the TLS stream (if TLS connection)
    #[must_use]
    pub fn as_tls_stream(&self) -> Option<&TlsStream<TcpStream>> {
        match self {
            Self::Tls(tls) => Some(tls.as_ref()),
            Self::Plain(_) => None,
        }
    }

    /// Get a mutable reference to the TLS stream (if TLS connection)
    pub fn as_tls_stream_mut(&mut self) -> Option<&mut TlsStream<TcpStream>> {
        match self {
            Self::Tls(tls) => Some(tls.as_mut()),
            Self::Plain(_) => None,
        }
    }

    /// Get the underlying TCP stream reference regardless of connection type
    ///
    /// For plain TCP, returns the stream directly.
    /// For TLS, returns the underlying TCP stream within the TLS wrapper.
    #[must_use]
    pub fn underlying_tcp_stream(&self) -> &TcpStream {
        match self {
            Self::Plain(tcp) => tcp,
            Self::Tls(tls) => tls.get_ref().0,
        }
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
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
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
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut *self {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
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

        let mut buf = [0u8; 5];
        server_conn.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"Hello");

        // Test stream type checking
        assert!(client_conn.is_unencrypted());
        assert!(!client_conn.is_encrypted());
        assert_eq!(client_conn.connection_type(), "TCP");
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

        // Test new API methods
        assert!(conn_stream.is_unencrypted());
        assert!(!conn_stream.is_encrypted());
        assert_eq!(conn_stream.connection_type(), "TCP");

        // Test underlying TCP access
        let _underlying = conn_stream.underlying_tcp_stream();
    }

    #[tokio::test]
    async fn test_connection_type_methods() {
        // Test that the new API names are more explicit and clear
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let tcp = std::net::TcpStream::connect(addr).unwrap();
        tcp.set_nonblocking(true).unwrap();
        let stream = TcpStream::from_std(tcp).unwrap();

        let conn = ConnectionStream::plain(stream);

        // New explicit method names
        assert!(conn.is_unencrypted(), "Plain TCP should be unencrypted");
        assert!(!conn.is_encrypted(), "Plain TCP should not be encrypted");
        assert_eq!(conn.connection_type(), "TCP");
    }
}
