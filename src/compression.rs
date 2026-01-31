//! Compression support for wire compression (RFC 8054)
//!
//! This module provides the `DecompressStream` wrapper type used by
//! `ConnectionStream::CompressedPlain` and `ConnectionStream::CompressedTls`.
//!
//! Implements bidirectional deflate compression using raw DEFLATE (no zlib header)
//! as specified in RFC 8054 §2.2.2.

use std::fmt;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

const COMPRESSED_BUF_SIZE: usize = 8192;
const WRITE_BUF_SIZE: usize = 16384; // Pre-allocated for poll_write compressed output
const DEFAULT_COMPRESS_LEVEL: u32 = 1; // Fast compression (latency > ratio for a proxy)

/// Tracks whether a pre-allocated output buffer has pending data to drain.
#[derive(Debug, Clone, Copy)]
enum DrainState {
    Idle,
    Draining { pos: usize, len: usize },
}

/// Try to write pending bytes from `buf` to inner based on `state`.
/// Returns Ready(Ok(())) when fully drained or idle, Pending if inner isn't ready.
fn poll_drain_buf<S: AsyncWrite>(
    mut inner: Pin<&mut S>,
    cx: &mut Context<'_>,
    buf: &[u8],
    state: &mut DrainState,
) -> Poll<io::Result<()>> {
    let DrainState::Draining { pos, len } = state else {
        return Poll::Ready(Ok(()));
    };
    while *pos < *len {
        let n = ready!(inner.as_mut().poll_write(cx, &buf[*pos..*len]))?;
        if n == 0 {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "inner stream write returned 0",
            )));
        }
        *pos += n;
    }
    *state = DrainState::Idle;
    Poll::Ready(Ok(()))
}

/// Drain the compressor with the given flush mode, writing all output to the inner stream.
fn poll_compress_drain<S: AsyncWrite>(
    compressor: &mut flate2::Compress,
    mut inner: Pin<&mut S>,
    cx: &mut Context<'_>,
    buf: &mut [u8],
    state: &mut DrainState,
    flush: flate2::FlushCompress,
) -> Poll<io::Result<()>> {
    loop {
        ready!(poll_drain_buf(inner.as_mut(), cx, buf, state))?;

        let before_out = compressor.total_out();
        compressor
            .compress(&[], buf, flush)
            .map_err(io::Error::other)?;
        let produced = (compressor.total_out() - before_out) as usize;

        if produced > 0 {
            *state = DrainState::Draining {
                pos: 0,
                len: produced,
            };
        }

        if produced < buf.len() {
            ready!(poll_drain_buf(inner.as_mut(), cx, buf, state))?;
            return Poll::Ready(Ok(()));
        }
    }
}

/// Decompress `input` into `buf`, returning `(consumed, done)`.
/// `done` is true if output was produced or the stream ended.
fn try_decompress(
    decompressor: &mut flate2::Decompress,
    input: &[u8],
    buf: &mut ReadBuf<'_>,
    stats: &mut u64,
) -> io::Result<(usize, bool)> {
    let out_slice = buf.initialize_unfilled();
    if out_slice.is_empty() {
        return Ok((0, true));
    }

    let before_in = decompressor.total_in();
    let before_out = decompressor.total_out();

    let status = decompressor
        .decompress(input, out_slice, flate2::FlushDecompress::None)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    let consumed = (decompressor.total_in() - before_in) as usize;
    let produced = (decompressor.total_out() - before_out) as usize;

    if produced > 0 {
        *stats += produced as u64;
        buf.advance(produced);
    }

    Ok((
        consumed,
        produced > 0 || matches!(status, flate2::Status::StreamEnd),
    ))
}

pin_project_lite::pin_project! {
    /// Bidirectional deflate stream wrapper for RFC 8054 COMPRESS DEFLATE.
    ///
    /// Reads: decompresses data from the inner stream (network → decompress → caller).
    /// Writes: compresses data to the inner stream (caller → compress → network).
    pub struct DecompressStream<S> {
        #[pin]
        inner: S,
        // Read side: network → decompress → caller
        decompressor: flate2::Decompress,
        compressed_buf: Box<[u8]>,
        compressed_pos: usize,
        compressed_len: usize,
        // Write side: caller → compress → network
        compressor: flate2::Compress,
        write_buf: Box<[u8]>,
        flush_buf: Box<[u8]>,
        write_drain: DrainState,
        flush_drain: DrainState,
        // Stats
        bytes_compressed_in: u64,
        bytes_decompressed_out: u64,
    }
}

impl<S> DecompressStream<S> {
    /// Wrap a stream for bidirectional deflate compression with the default (fast) level.
    pub fn new(inner: S) -> Self {
        Self::with_level(inner, DEFAULT_COMPRESS_LEVEL)
    }

    /// Wrap a stream with a specific compression level (0-9).
    pub fn with_level(inner: S, level: u32) -> Self {
        let level = flate2::Compression::new(level.min(9));
        Self {
            inner,
            // Raw deflate, no zlib header (RFC 8054 §2.2.2)
            decompressor: flate2::Decompress::new(false),
            compressed_buf: vec![0u8; COMPRESSED_BUF_SIZE].into_boxed_slice(),
            compressed_pos: 0,
            compressed_len: 0,
            compressor: flate2::Compress::new(level, false),
            write_buf: vec![0u8; WRITE_BUF_SIZE].into_boxed_slice(),
            flush_buf: vec![0u8; WRITE_BUF_SIZE].into_boxed_slice(),
            write_drain: DrainState::Idle,
            flush_drain: DrainState::Idle,
            bytes_compressed_in: 0,
            bytes_decompressed_out: 0,
        }
    }

    /// Consume the wrapper, returning the inner stream.
    pub fn into_inner(self) -> S {
        self.inner
    }

    /// Get a reference to the inner stream.
    #[inline]
    pub fn get_ref(&self) -> &S {
        &self.inner
    }

    /// Get a mutable reference to the inner stream.
    #[inline]
    pub fn get_mut(&mut self) -> &mut S {
        &mut self.inner
    }

    /// Get bandwidth stats: (compressed bytes read from network, decompressed bytes delivered).
    #[inline]
    pub fn bandwidth_stats(&self) -> (u64, u64) {
        (self.bytes_compressed_in, self.bytes_decompressed_out)
    }
}

impl<S: fmt::Debug> fmt::Debug for DecompressStream<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecompressStream")
            .field("inner", &self.inner)
            .field("compressed_pos", &self.compressed_pos)
            .field("compressed_len", &self.compressed_len)
            .field("bytes_compressed_in", &self.bytes_compressed_in)
            .field("bytes_decompressed_out", &self.bytes_decompressed_out)
            .finish()
    }
}

impl<S: AsyncRead> AsyncRead for DecompressStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut this = self.project();
        loop {
            if *this.compressed_pos < *this.compressed_len {
                let input = &this.compressed_buf[*this.compressed_pos..*this.compressed_len];
                let (consumed, done) =
                    try_decompress(this.decompressor, input, buf, this.bytes_decompressed_out)?;
                *this.compressed_pos += consumed;
                if done {
                    return Poll::Ready(Ok(()));
                }
                continue;
            }

            let (_, done) =
                try_decompress(this.decompressor, &[], buf, this.bytes_decompressed_out)?;
            if done {
                return Poll::Ready(Ok(()));
            }

            // Inline poll_fill_buffer (can't call &mut self methods from projection)
            *this.compressed_pos = 0;
            *this.compressed_len = 0;
            let mut read_buf = ReadBuf::new(this.compressed_buf);
            ready!(this.inner.as_mut().poll_read(cx, &mut read_buf))?;
            let n = read_buf.filled().len();
            if n == 0 {
                return Poll::Ready(Ok(()));
            }
            *this.compressed_len = n;
            *this.bytes_compressed_in += n as u64;
        }
    }
}

impl<S: AsyncWrite> AsyncWrite for DecompressStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut this = self.project();

        ready!(poll_drain_buf(
            this.inner.as_mut(),
            cx,
            this.write_buf,
            this.write_drain,
        ))?;

        let before_in = this.compressor.total_in();
        let before_out = this.compressor.total_out();

        this.compressor
            .compress(buf, this.write_buf, flate2::FlushCompress::None)
            .map_err(io::Error::other)?;

        let consumed = (this.compressor.total_in() - before_in) as usize;
        let produced = (this.compressor.total_out() - before_out) as usize;

        if produced > 0 {
            *this.write_drain = DrainState::Draining {
                pos: 0,
                len: produced,
            };
            match poll_drain_buf(this.inner.as_mut(), cx, this.write_buf, this.write_drain) {
                Poll::Ready(Ok(())) => {}
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => {}
            }
        }

        Poll::Ready(Ok(if consumed > 0 {
            consumed
        } else {
            buf.len().min(1)
        }))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut this = self.project();
        ready!(poll_drain_buf(
            this.inner.as_mut(),
            cx,
            this.write_buf,
            this.write_drain
        ))?;
        ready!(poll_compress_drain(
            this.compressor,
            this.inner.as_mut(),
            cx,
            this.flush_buf,
            this.flush_drain,
            flate2::FlushCompress::Sync,
        ))?;
        this.inner.poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut this = self.project();
        ready!(poll_drain_buf(
            this.inner.as_mut(),
            cx,
            this.write_buf,
            this.write_drain
        ))?;
        ready!(poll_compress_drain(
            this.compressor,
            this.inner.as_mut(),
            cx,
            this.flush_buf,
            this.flush_drain,
            flate2::FlushCompress::Finish,
        ))?;
        this.inner.poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn deflate_compress(data: &[u8]) -> Vec<u8> {
        use flate2::write::DeflateEncoder;
        use std::io::Write;
        let mut enc = DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    async fn feed_compressed(
        data: &[u8],
    ) -> (
        tokio::io::DuplexStream,
        DecompressStream<tokio::io::DuplexStream>,
    ) {
        let compressed = deflate_compress(data);
        let (mut tx, rx) = tokio::io::duplex(4096);
        tx.write_all(&compressed).await.unwrap();
        tx.shutdown().await.unwrap();
        (tx, DecompressStream::new(rx))
    }

    #[tokio::test]
    async fn test_roundtrip_read() {
        let original = b"Hello, NNTP world! COMPRESS DEFLATE test data.\r\n";
        let (_tx, mut stream) = feed_compressed(original).await;

        let mut output = Vec::new();
        stream.read_to_end(&mut output).await.unwrap();

        assert_eq!(output, original);
    }

    #[tokio::test]
    async fn test_roundtrip_write_flush() {
        // Write plaintext to DecompressStream, read compressed output, decompress, verify
        use flate2::read::DeflateDecoder;
        use std::io::Read;

        let original = b"GROUP alt.test\r\n";

        let (reader, writer_side) = tokio::io::duplex(4096);
        let mut stream = DecompressStream::new(writer_side);

        stream.write_all(original).await.unwrap();
        stream.flush().await.unwrap();
        stream.shutdown().await.unwrap();

        drop(stream);

        // Read all compressed output from the other end
        let mut compressed_output = Vec::new();
        let mut reader = reader;
        reader.read_to_end(&mut compressed_output).await.unwrap();

        // Decompress and verify
        let mut decoder = DeflateDecoder::new(&compressed_output[..]);
        let mut decompressed = Vec::new();
        decoder.read_to_end(&mut decompressed).unwrap();

        assert_eq!(decompressed, original);
    }

    #[tokio::test]
    async fn test_large_data_roundtrip() {
        // 1MB of data
        let mut original = Vec::with_capacity(1024 * 1024);
        for i in 0..1024 * 64 {
            original.extend_from_slice(
                format!("Line {}: Some NNTP article content here\r\n", i).as_bytes(),
            );
        }

        let compressed = deflate_compress(&original);

        let (mut writer, reader) = tokio::io::duplex(16384);

        tokio::spawn(async move {
            writer.write_all(&compressed).await.unwrap();
            writer.shutdown().await.unwrap();
        });

        let mut stream = DecompressStream::new(reader);
        let mut output = Vec::new();
        stream.read_to_end(&mut output).await.unwrap();

        assert_eq!(output.len(), original.len());
        assert_eq!(output, original);
    }

    #[tokio::test]
    async fn test_bandwidth_stats() {
        let original = b"Test data for bandwidth tracking\r\n";
        let (_tx, mut stream) = feed_compressed(original).await;

        let mut output = Vec::new();
        stream.read_to_end(&mut output).await.unwrap();

        let (compressed_in, decompressed_out) = stream.bandwidth_stats();
        assert!(compressed_in > 0);
        assert_eq!(decompressed_out, original.len() as u64);
    }

    #[test]
    fn test_debug_impl() {
        let stream = DecompressStream::new(std::io::Cursor::new(Vec::<u8>::new()));
        let debug_str = format!("{:?}", stream);
        assert!(debug_str.contains("DecompressStream"));
        assert!(debug_str.contains("compressed_pos"));
        assert!(debug_str.contains("bytes_compressed_in"));
    }
}
