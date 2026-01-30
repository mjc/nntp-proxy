//! Compression support stub for wire compression (RFC 8054)
//!
//! This module provides the `DecompressStream` wrapper type used by
//! `ConnectionStream::CompressedPlain` and `ConnectionStream::CompressedTls`.
//!
//! Currently a no-op passthrough — actual decompression logic will be
//! implemented in `feature/wire-compression-rfc8054`.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Wrapper that will apply decompression to an inner stream.
///
/// Currently forwards all reads/writes directly to the inner stream.
/// The compression feature branch will replace this with `flate2`-based
/// zlib decompression (RFC 8054 COMPRESS DEFLATE / XFEATURE COMPRESS GZIP).
#[derive(Debug)]
pub struct DecompressStream<S> {
    inner: S,
}

impl<S> DecompressStream<S> {
    /// Wrap a stream for decompression.
    pub fn new(inner: S) -> Self {
        Self { inner }
    }

    /// Get a reference to the inner stream.
    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    /// Get a mutable reference to the inner stream.
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.inner
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for DecompressStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for DecompressStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
