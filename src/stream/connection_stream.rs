//! Stream abstraction for supporting multiple connection types
//!
//! This module provides abstractions for handling different stream types (TCP, TLS, etc.)
//! in a unified way. This is preparation for adding SSL/TLS support to backend connections.

use bytes::BytesMut;

use crate::compression::DecompressStream;
use crate::constants::buffer::MAX_LEFTOVER_BYTES;
use crate::tls::TlsStream;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;

/// Trait for async streams that can be used for NNTP connections
///
/// This trait is automatically implemented for any type that implements
/// `AsyncRead` + `AsyncWrite` + Unpin + Send, making it easy to support
/// different connection types (TCP, TLS, etc.).
pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}

// Blanket implementation for all types that meet the requirements
impl<T> AsyncStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

#[derive(Debug)]
enum ConnectionTransport {
    /// Plain TCP connection
    Plain(TcpStream),
    /// TLS-encrypted connection
    Tls(Box<TlsStream<TcpStream>>),
    /// Compressed plain TCP connection (RFC 8054 / XFEATURE COMPRESS GZIP)
    CompressedPlain(Box<DecompressStream<TcpStream>>),
    /// Compressed TLS connection (RFC 8054 / XFEATURE COMPRESS GZIP)
    CompressedTls(Box<DecompressStream<TlsStream<TcpStream>>>),
}

/// Unified stream type that can represent different connection types.
///
/// The stream also owns a small read-ahead buffer used to preserve bytes that
/// were already consumed from the socket but belong to the next NNTP response.
#[derive(Debug)]
pub struct ConnectionStream {
    transport: ConnectionTransport,
    leftover: BytesMut,
}

impl ConnectionStream {
    /// Create a new plain TCP connection stream
    pub fn plain(stream: TcpStream) -> Self {
        Self::new(ConnectionTransport::Plain(stream))
    }

    /// Create a new TLS-encrypted connection stream
    pub fn tls(stream: TlsStream<TcpStream>) -> Self {
        Self::new(ConnectionTransport::Tls(Box::new(stream)))
    }

    /// Create a compressed plain TCP connection stream
    pub fn compressed_plain(stream: TcpStream) -> Self {
        Self::new(ConnectionTransport::CompressedPlain(Box::new(
            DecompressStream::new(stream),
        )))
    }

    /// Create a compressed TLS connection stream
    pub fn compressed_tls(stream: TlsStream<TcpStream>) -> Self {
        Self::new(ConnectionTransport::CompressedTls(Box::new(
            DecompressStream::new(stream),
        )))
    }

    fn new(transport: ConnectionTransport) -> Self {
        Self {
            transport,
            leftover: BytesMut::new(),
        }
    }

    /// Wrap the current transport in a decompressor, preserving any read-ahead bytes.
    ///
    /// Returns an error if compression is requested for a stream that is already compressed.
    /// This keeps the state transition explicit instead of panicking on an invalid call.
    pub(crate) fn into_compressed(self, level: u32) -> io::Result<Self> {
        let transport = match self.transport {
            ConnectionTransport::Plain(tcp) => ConnectionTransport::CompressedPlain(Box::new(
                DecompressStream::with_level(tcp, level),
            )),
            ConnectionTransport::Tls(tls) => ConnectionTransport::CompressedTls(Box::new(
                DecompressStream::with_level(*tls, level),
            )),
            ConnectionTransport::CompressedPlain(_) | ConnectionTransport::CompressedTls(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "cannot enable compression on an already-compressed connection",
                ));
            }
        };

        Ok(Self {
            transport,
            leftover: self.leftover,
        })
    }

    /// Returns the connection type as a string for logging/debugging
    #[must_use]
    pub const fn connection_type(&self) -> &'static str {
        match &self.transport {
            ConnectionTransport::Plain(_) => "TCP",
            ConnectionTransport::Tls(_) => "TLS",
            ConnectionTransport::CompressedPlain(_) => "TCP+COMPRESS",
            ConnectionTransport::CompressedTls(_) => "TLS+COMPRESS",
        }
    }

    /// Returns true if this connection uses encryption (TLS/SSL)
    #[inline]
    #[must_use]
    pub const fn is_encrypted(&self) -> bool {
        matches!(
            &self.transport,
            ConnectionTransport::Tls(_) | ConnectionTransport::CompressedTls(_)
        )
    }

    /// Returns true if this connection is unencrypted (plain TCP)
    #[inline]
    #[must_use]
    pub const fn is_unencrypted(&self) -> bool {
        matches!(
            &self.transport,
            ConnectionTransport::Plain(_) | ConnectionTransport::CompressedPlain(_)
        )
    }

    /// Returns true if this connection uses wire compression
    #[inline]
    #[must_use]
    pub const fn is_compressed(&self) -> bool {
        matches!(
            &self.transport,
            ConnectionTransport::CompressedPlain(_) | ConnectionTransport::CompressedTls(_)
        )
    }

    /// Get a reference to the underlying TCP stream (if plain, uncompressed TCP)
    ///
    /// Returns None for TLS or compressed streams.
    /// Useful for socket optimization that requires direct TCP access.
    #[must_use]
    pub const fn as_tcp_stream(&self) -> Option<&TcpStream> {
        match &self.transport {
            ConnectionTransport::Plain(tcp) => Some(tcp),
            _ => None,
        }
    }

    /// Get a mutable reference to the underlying TCP stream (if plain, uncompressed TCP)
    pub const fn as_tcp_stream_mut(&mut self) -> Option<&mut TcpStream> {
        match &mut self.transport {
            ConnectionTransport::Plain(tcp) => Some(tcp),
            _ => None,
        }
    }

    /// Get a reference to the TLS stream (if uncompressed TLS connection)
    #[must_use]
    pub fn as_tls_stream(&self) -> Option<&TlsStream<TcpStream>> {
        match &self.transport {
            ConnectionTransport::Tls(tls) => Some(tls.as_ref()),
            _ => None,
        }
    }

    /// Get a mutable reference to the TLS stream (if uncompressed TLS connection)
    pub fn as_tls_stream_mut(&mut self) -> Option<&mut TlsStream<TcpStream>> {
        match &mut self.transport {
            ConnectionTransport::Tls(tls) => Some(tls.as_mut()),
            _ => None,
        }
    }

    /// Get the underlying TCP stream reference regardless of connection type
    ///
    /// For plain TCP, returns the stream directly.
    /// For TLS, returns the underlying TCP stream within the TLS wrapper.
    /// For compressed streams, returns the TCP stream from within the wrapper.
    #[must_use]
    pub fn underlying_tcp_stream(&self) -> &TcpStream {
        match &self.transport {
            ConnectionTransport::Plain(tcp) => tcp,
            ConnectionTransport::Tls(tls) => tls.get_ref().0,
            ConnectionTransport::CompressedPlain(cs) => cs.get_ref(),
            ConnectionTransport::CompressedTls(cs) => cs.get_ref().get_ref().0,
        }
    }

    /// Stash bytes that were already read from the backend but belong to the next response.
    pub fn stash_leftover(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.leftover.len() + bytes.len() <= MAX_LEFTOVER_BYTES,
            "Leftover exceeds {} bytes ({} bytes) — probable protocol desync",
            MAX_LEFTOVER_BYTES,
            self.leftover.len() + bytes.len()
        );
        self.leftover.extend_from_slice(bytes);
        Ok(())
    }

    #[must_use]
    pub fn has_leftover(&self) -> bool {
        !self.leftover.is_empty()
    }

    #[must_use]
    pub fn leftover_len(&self) -> usize {
        self.leftover.len()
    }

    #[cfg(test)]
    pub fn clear_leftover(&mut self) {
        self.leftover.clear();
    }
}

impl AsyncRead for ConnectionStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.leftover.is_empty() && buf.remaining() > 0 {
            let n = self.leftover.len().min(buf.remaining());
            let data = self.leftover.split_to(n);
            buf.put_slice(&data);
            return Poll::Ready(Ok(()));
        }

        match &mut self.transport {
            ConnectionTransport::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            ConnectionTransport::Tls(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
            ConnectionTransport::CompressedPlain(stream) => {
                Pin::new(stream.as_mut()).poll_read(cx, buf)
            }
            ConnectionTransport::CompressedTls(stream) => {
                Pin::new(stream.as_mut()).poll_read(cx, buf)
            }
        }
    }
}

impl AsyncWrite for ConnectionStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match &mut self.transport {
            ConnectionTransport::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            ConnectionTransport::Tls(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
            ConnectionTransport::CompressedPlain(stream) => {
                Pin::new(stream.as_mut()).poll_write(cx, buf)
            }
            ConnectionTransport::CompressedTls(stream) => {
                Pin::new(stream.as_mut()).poll_write(cx, buf)
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.transport {
            ConnectionTransport::Plain(stream) => Pin::new(stream).poll_flush(cx),
            ConnectionTransport::Tls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
            ConnectionTransport::CompressedPlain(stream) => {
                Pin::new(stream.as_mut()).poll_flush(cx)
            }
            ConnectionTransport::CompressedTls(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match &mut self.transport {
            ConnectionTransport::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            ConnectionTransport::Tls(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
            ConnectionTransport::CompressedPlain(stream) => {
                Pin::new(stream.as_mut()).poll_shutdown(cx)
            }
            ConnectionTransport::CompressedTls(stream) => {
                Pin::new(stream.as_mut()).poll_shutdown(cx)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn test_connection_stream_plain_tcp() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client_handle = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });

        let (server_stream, _) = listener.accept().await.unwrap();
        let client_stream = client_handle.await.unwrap();

        let mut server_conn = ConnectionStream::plain(server_stream);
        let mut client_conn = ConnectionStream::plain(client_stream);

        client_conn.write_all(b"Hello").await.unwrap();

        let mut buf = [0u8; 5];
        server_conn.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"Hello");

        assert!(client_conn.is_unencrypted());
        assert!(!client_conn.is_encrypted());
        assert_eq!(client_conn.connection_type(), "TCP");
        assert!(client_conn.as_tcp_stream().is_some());
    }

    #[test]
    fn test_async_stream_trait() {
        fn assert_async_stream<T: AsyncStream>() {}
        assert_async_stream::<TcpStream>();
        assert_async_stream::<ConnectionStream>();
    }

    #[tokio::test]
    async fn test_connection_stream_tcp_access() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client_handle = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server_stream, _) = listener.accept().await.unwrap();
        let _client_stream = client_handle.await.unwrap();

        let mut conn_stream = ConnectionStream::plain(server_stream);

        assert!(conn_stream.is_unencrypted());
        assert!(conn_stream.as_tcp_stream().is_some());
        assert!(conn_stream.as_tls_stream().is_none());

        let _underlying = conn_stream.underlying_tcp_stream();

        let tcp_mut = conn_stream.as_tcp_stream_mut().unwrap();
        tcp_mut.set_nodelay(true).unwrap();
    }

    #[tokio::test]
    async fn test_plain_connection_type_checks() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client_handle = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server_stream, _) = listener.accept().await.unwrap();
        let _client = client_handle.await.unwrap();

        let conn = ConnectionStream::plain(server_stream);

        assert_eq!(conn.connection_type(), "TCP");
        assert!(conn.is_unencrypted());
        assert!(!conn.is_encrypted());
        assert!(!conn.is_compressed());
    }

    #[tokio::test]
    async fn test_tcp_access_methods_work() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client_handle = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server_stream, _) = listener.accept().await.unwrap();
        let _client = client_handle.await.unwrap();

        let conn = ConnectionStream::plain(server_stream);

        assert!(conn.as_tcp_stream().is_some());
        assert!(conn.as_tls_stream().is_none());
    }

    #[tokio::test]
    async fn test_mutable_tcp_access_methods_work() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client_handle = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server_stream, _) = listener.accept().await.unwrap();
        let _client = client_handle.await.unwrap();

        let mut conn = ConnectionStream::plain(server_stream);

        assert!(conn.as_tcp_stream_mut().is_some());
        assert!(conn.as_tls_stream_mut().is_none());
        assert!(conn.as_tcp_stream().is_some());

        let _underlying = conn.underlying_tcp_stream();
    }

    #[tokio::test]
    async fn test_constructor_creates_plain_variant() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client_handle = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server_stream, _) = listener.accept().await.unwrap();
        let _client = client_handle.await.unwrap();

        let plain_conn = ConnectionStream::plain(server_stream);

        assert_eq!(plain_conn.connection_type(), "TCP");
        assert!(plain_conn.is_unencrypted());
    }

    #[tokio::test]
    async fn test_leftover_is_read_before_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client_handle = tokio::spawn(async move {
            let mut client = TcpStream::connect(addr).await.unwrap();
            client.write_all(b"socket").await.unwrap();
            client
        });

        let (server_stream, _) = listener.accept().await.unwrap();
        let _client = client_handle.await.unwrap();

        let mut conn = ConnectionStream::plain(server_stream);
        conn.stash_leftover(b"left").unwrap();

        let mut buf = [0u8; 4];
        conn.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"left");

        let mut buf = [0u8; 6];
        conn.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"socket");
    }

    #[tokio::test]
    async fn test_stash_leftover_rejects_oversize() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client_handle = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server_stream, _) = listener.accept().await.unwrap();
        let _client = client_handle.await.unwrap();

        let mut conn = ConnectionStream::plain(server_stream);
        let oversized = vec![b'x'; MAX_LEFTOVER_BYTES + 1];
        assert!(conn.stash_leftover(&oversized).is_err());
    }

    #[tokio::test]
    async fn test_into_compressed_preserves_leftover() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client_handle = tokio::spawn(async move {
            let mut client = TcpStream::connect(addr).await.unwrap();
            client.write_all(b"socket").await.unwrap();
            client
        });

        let (server_stream, _) = listener.accept().await.unwrap();
        let _client = client_handle.await.unwrap();

        let mut conn = ConnectionStream::plain(server_stream);
        conn.stash_leftover(b"left").unwrap();

        let mut conn = conn.into_compressed(1).unwrap();
        assert!(conn.is_compressed());
        assert_eq!(conn.leftover_len(), 4);

        let mut buf = [0u8; 4];
        conn.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"left");
    }

    #[tokio::test]
    async fn test_into_compressed_rejects_already_compressed_stream() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client_handle = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server_stream, _) = listener.accept().await.unwrap();
        let _client = client_handle.await.unwrap();

        let conn = ConnectionStream::compressed_plain(server_stream);
        let err = conn.into_compressed(1).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            err.to_string(),
            "cannot enable compression on an already-compressed connection"
        );
    }

    #[tokio::test]
    async fn test_plain_connection_debug_format() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client_handle = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server_stream, _) = listener.accept().await.unwrap();
        let _client = client_handle.await.unwrap();

        let conn = ConnectionStream::plain(server_stream);
        // Test Debug implementation
        let debug_str = format!("{conn:?}");
        assert!(
            debug_str.contains("Plain"),
            "Debug output should indicate Plain TCP"
        );
    }
}
