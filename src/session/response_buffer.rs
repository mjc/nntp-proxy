//! Buffered NNTP response readers for pooled backend connections.
//!
//! This module owns the request-aware response buffering logic used by
//! per-command and pipeline paths. It intentionally excludes the old
//! direct-to-client multiline streaming machinery.

use anyhow::Result;
use bytes::Bytes;
use std::ops::Range;
#[cfg(test)]
use tracing::warn;

use crate::session::multiline_framing::MultilineFramer;

/// Outcome of buffering a backend response.
#[derive(Debug)]
pub enum StreamingError {
    /// Client disconnected after the proxy had already buffered a complete
    /// backend response and started writing it locally.
    ClientDisconnect(std::io::Error),

    /// Backend closed connection before sending the multiline terminator.
    BackendEof {
        backend_id: crate::types::BackendId,
        bytes_received: u64,
    },

    /// Other I/O / protocol error.
    Io(anyhow::Error),
}

impl StreamingError {
    pub(crate) const fn must_remove_connection(&self) -> bool {
        !matches!(self, Self::ClientDisconnect(_))
    }

    pub(crate) fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::ClientDisconnect(io_err) => anyhow::Error::from(io_err),
            Self::Io(e) => e,
            Self::BackendEof {
                backend_id,
                bytes_received,
            } => anyhow::anyhow!(
                "Backend {backend_id:?} closed connection before multiline terminator \
                 ({bytes_received} bytes received)"
            ),
        }
    }
}

impl std::fmt::Display for StreamingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClientDisconnect(e) => write!(f, "client disconnected: {e}"),
            Self::BackendEof {
                backend_id,
                bytes_received,
            } => write!(
                f,
                "backend {backend_id:?} closed connection before multiline terminator \
                 ({bytes_received} bytes received)"
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StreamingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ClientDisconnect(e) => Some(e),
            Self::BackendEof { .. } => None,
            Self::Io(e) => e.source(),
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
enum PrefetchedResponse {
    Buffer {
        status_code: crate::protocol::StatusCode,
        len: usize,
    },
    Shared {
        status_code: crate::protocol::StatusCode,
        bytes: Bytes,
        range: Range<usize>,
    },
}

#[cfg_attr(not(test), allow(dead_code))]
impl PrefetchedResponse {
    const fn status_code(&self) -> crate::protocol::StatusCode {
        match self {
            Self::Buffer { status_code, .. } | Self::Shared { status_code, .. } => *status_code,
        }
    }
}

pub(crate) struct PrefetchedResponseContext<'a> {
    pub request: &'a crate::protocol::RequestContext,
    pub status_code: crate::protocol::StatusCode,
    pub initial_len: usize,
    pub pool: &'a crate::pool::BufferPool,
    pub backend_id: crate::types::BackendId,
    pub allow_packed_suffix: bool,
}

#[cfg(test)]
fn validate_response_prefix(
    response: &[u8],
    source: &'static str,
) -> Result<crate::protocol::StatusCode, StreamingError> {
    let validated = crate::session::backend::parse_backend_status(
        response,
        response.len(),
        crate::protocol::MIN_RESPONSE_LENGTH,
    );
    validated.status_code.ok_or_else(|| {
        warn!(
            bytes_read = response.len(),
            first_bytes_hex = %crate::session::backend::format_hex_preview(response, 256),
            first_bytes_utf8 = %String::from_utf8_lossy(&response[..response.len().min(256)]),
            source = source,
            "Invalid status code in response"
        );
        StreamingError::Io(anyhow::anyhow!("Invalid status code in response"))
    })
}

#[cfg(test)]
async fn read_prefetched_response(
    io_buffer: &mut crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
) -> Result<PrefetchedResponse, StreamingError> {
    if let Some((bytes, range)) = conn.pop_leftover_shared() {
        let response = &bytes[range.clone()];
        if crate::session::backend::status_line_end(response).is_some() {
            let status_code = validate_response_prefix(response, "leftover")?;
            return Ok(PrefetchedResponse::Shared {
                status_code,
                bytes,
                range,
            });
        }

        if response.len() > io_buffer.capacity() {
            return Err(StreamingError::Io(anyhow::anyhow!(
                "Backend leftover exceeded response buffer before complete status line ({} bytes)",
                response.len()
            )));
        }

        io_buffer.copy_from_slice(response);

        while crate::session::backend::status_line_end(io_buffer).is_none() {
            let more = io_buffer.read_more(conn).await.map_err(|e| {
                StreamingError::Io(
                    anyhow::Error::from(e).context("Failed to read partial status line"),
                )
            })?;
            if more == 0 {
                return Err(StreamingError::Io(anyhow::anyhow!(
                    "Backend EOF before complete status line ({} bytes)",
                    io_buffer.initialized()
                )));
            }
        }

        let initial_len = io_buffer.initialized();
        let response = &io_buffer[..initial_len];
        let status_code = validate_response_prefix(response, "leftover")?;
        return Ok(PrefetchedResponse::Buffer {
            status_code,
            len: initial_len,
        });
    }

    let source = if conn.has_leftover() {
        "leftover"
    } else {
        "fresh_read"
    };
    let n = io_buffer.read_from(conn).await.map_err(|e| {
        StreamingError::Io(anyhow::Error::from(e).context("Failed to read response from backend"))
    })?;
    if n == 0 {
        return Err(StreamingError::Io(anyhow::anyhow!(
            "Backend connection closed unexpectedly"
        )));
    }
    while crate::session::backend::status_line_end(io_buffer).is_none() {
        let more = io_buffer.read_more(conn).await.map_err(|e| {
            StreamingError::Io(anyhow::Error::from(e).context("Failed to read partial status line"))
        })?;
        if more == 0 {
            return Err(StreamingError::Io(anyhow::anyhow!(
                "Backend EOF before complete status line ({} bytes)",
                io_buffer.initialized()
            )));
        }
    }

    let initial_len = io_buffer.initialized();
    let response = &io_buffer[..initial_len];
    let status_code = validate_response_prefix(response, source)?;
    Ok(PrefetchedResponse::Buffer {
        status_code,
        len: initial_len,
    })
}

fn buffer_single_line_response(
    prefetched: PrefetchedResponse,
    io_buffer: &mut crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    result_buf: &mut crate::pool::ChunkedResponse,
    pool: &crate::pool::BufferPool,
) -> Result<(), StreamingError> {
    result_buf.clear();
    match prefetched {
        PrefetchedResponse::Buffer { len, .. } => {
            let response = &io_buffer[..len];
            let end = crate::session::backend::status_line_end(response).ok_or_else(|| {
                StreamingError::Io(anyhow::anyhow!(
                    "Prefetched single-line response missing status line terminator"
                ))
            })?;
            push_buffer_prefix_to_response(io_buffer, conn, result_buf, pool, end, len)?;
        }
        PrefetchedResponse::Shared { bytes, range, .. } => {
            let response = &bytes[range.clone()];
            let end = crate::session::backend::status_line_end(response).ok_or_else(|| {
                StreamingError::Io(anyhow::anyhow!(
                    "Prefetched single-line response missing status line terminator"
                ))
            })?;
            push_shared_prefix_to_response(bytes, range, conn, result_buf, end)?;
        }
    }
    Ok(())
}

fn push_shared_prefix_to_response(
    bytes: Bytes,
    range: Range<usize>,
    conn: &mut crate::stream::ConnectionStream,
    response: &mut crate::pool::ChunkedResponse,
    prefix_len: usize,
) -> Result<(), StreamingError> {
    let prefix_range = range.start..(range.start + prefix_len);
    let suffix_range = prefix_range.end..range.end;

    response.push_shared_range(bytes.clone(), prefix_range);
    if !suffix_range.is_empty() {
        conn.push_front_leftover_shared(bytes, suffix_range)
            .map_err(StreamingError::Io)?;
    }
    Ok(())
}

fn push_buffer_prefix_to_response(
    io_buffer: &mut crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    response: &mut crate::pool::ChunkedResponse,
    pool: &crate::pool::BufferPool,
    prefix_len: usize,
    total_len: usize,
) -> Result<(), StreamingError> {
    let old = std::mem::replace(io_buffer, pool.acquire());
    if prefix_len < total_len {
        let bytes = old.freeze();
        response.push_shared_range(bytes.clone(), 0..prefix_len);
        conn.stash_leftover_shared(bytes, prefix_len..total_len)
            .map_err(StreamingError::Io)?;
        return Ok(());
    }

    response.push_buffer_range(old, 0..prefix_len);
    Ok(())
}

fn validate_multiline_suffix(
    chunk_len: usize,
    terminator_end: usize,
    allow_packed_suffix: bool,
) -> Result<(), StreamingError> {
    if allow_packed_suffix || terminator_end == chunk_len {
        return Ok(());
    }

    Err(StreamingError::Io(anyhow::anyhow!(
        "Backend sent {} unexpected trailing bytes after multiline terminator",
        chunk_len - terminator_end
    )))
}

fn process_shared_multiline_chunk(
    bytes: Bytes,
    range: Range<usize>,
    conn: &mut crate::stream::ConnectionStream,
    response: &mut crate::pool::ChunkedResponse,
    framer: &mut MultilineFramer,
    allow_packed_suffix: bool,
) -> Result<bool, StreamingError> {
    let chunk = &bytes[range.clone()];
    let Some(write_len) = framer.advance_to_next_terminator_end(chunk) else {
        response.push_shared_range(bytes, range);
        return Ok(false);
    };
    validate_multiline_suffix(chunk.len(), write_len, allow_packed_suffix)?;
    push_shared_prefix_to_response(bytes, range, conn, response, write_len)?;
    Ok(true)
}

async fn fill_multiline_response(
    io_buffer: &mut crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    response: &mut crate::pool::ChunkedResponse,
    pool: &crate::pool::BufferPool,
    prefetched: PrefetchedResponse,
    backend_id: crate::types::BackendId,
    allow_packed_suffix: bool,
) -> Result<(), StreamingError> {
    response.clear();
    let mut framer = MultilineFramer::default();

    match prefetched {
        PrefetchedResponse::Buffer {
            len: initial_chunk_len,
            ..
        } => match framer.advance_to_next_terminator_end(&io_buffer[..initial_chunk_len]) {
            Some(pos) => {
                validate_multiline_suffix(initial_chunk_len, pos, allow_packed_suffix)?;
                push_buffer_prefix_to_response(
                    io_buffer,
                    conn,
                    response,
                    pool,
                    pos,
                    initial_chunk_len,
                )?;
                return Ok(());
            }
            None => {
                let old = std::mem::replace(io_buffer, pool.acquire());
                response.push_buffer_range(old, 0..initial_chunk_len);
            }
        },
        PrefetchedResponse::Shared { bytes, range, .. } => {
            if process_shared_multiline_chunk(
                bytes,
                range,
                conn,
                response,
                &mut framer,
                allow_packed_suffix,
            )? {
                return Ok(());
            }
        }
    }

    loop {
        if let Some((bytes, range)) = conn.pop_leftover_shared() {
            if process_shared_multiline_chunk(
                bytes,
                range,
                conn,
                response,
                &mut framer,
                allow_packed_suffix,
            )? {
                return Ok(());
            }
            continue;
        }

        let n = io_buffer.read_from(conn).await.map_err(|e| {
            StreamingError::Io(
                anyhow::Error::from(e).context("Failed to read remaining response body"),
            )
        })?;
        if n == 0 {
            return Err(StreamingError::BackendEof {
                backend_id,
                bytes_received: response.len() as u64,
            });
        }

        if let Some(write_len) = framer.advance_to_next_terminator_end(&io_buffer[..n]) {
            validate_multiline_suffix(n, write_len, allow_packed_suffix)?;
            push_buffer_prefix_to_response(io_buffer, conn, response, pool, write_len, n)?;
            return Ok(());
        }

        let old = std::mem::replace(io_buffer, pool.acquire());
        response.push_buffer_range(old, 0..n);
    }
}

/// Read a complete NNTP response using request-aware framing.
#[cfg(test)]
pub(crate) async fn buffer_response_for_request(
    request: &crate::protocol::RequestContext,
    io_buffer: &mut crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    result_buf: &mut crate::pool::ChunkedResponse,
    pool: &crate::pool::BufferPool,
    backend_id: crate::types::BackendId,
    allow_packed_suffix: bool,
) -> Result<crate::protocol::StatusCode, StreamingError> {
    let prefetched = read_prefetched_response(io_buffer, conn).await?;
    let status_code = prefetched.status_code();

    if !request.expects_multiline_response(status_code) {
        buffer_single_line_response(prefetched, io_buffer, conn, result_buf, pool)?;
        return Ok(status_code);
    }

    fill_multiline_response(
        io_buffer,
        conn,
        result_buf,
        pool,
        prefetched,
        backend_id,
        allow_packed_suffix,
    )
    .await?;
    Ok(status_code)
}

/// Buffer a complete response when the first backend read has already happened.
///
/// This keeps direct backend delivery on the same request-aware buffering path as
/// pipelined delivery, while preserving the owned pooled read buffer instead of
/// copying the first chunk into capture storage.
pub(crate) async fn buffer_prefetched_response_for_request(
    ctx: PrefetchedResponseContext<'_>,
    io_buffer: &mut crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    result_buf: &mut crate::pool::ChunkedResponse,
) -> Result<(), StreamingError> {
    let prefetched = PrefetchedResponse::Buffer {
        status_code: ctx.status_code,
        len: ctx.initial_len,
    };

    if !ctx.request.expects_multiline_response(ctx.status_code) {
        buffer_single_line_response(prefetched, io_buffer, conn, result_buf, ctx.pool)?;
        return Ok(());
    }

    fill_multiline_response(
        io_buffer,
        conn,
        result_buf,
        ctx.pool,
        prefetched,
        ctx.backend_id,
        ctx.allow_packed_suffix,
    )
    .await
}

/// Read a complete NNTP response and attach it to the matching request context.
#[cfg(test)]
pub(crate) async fn read_response_into_context(
    request: &mut crate::protocol::RequestContext,
    io_buffer: &mut crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    result_buf: &mut crate::pool::ChunkedResponse,
    pool: &crate::pool::BufferPool,
    backend_id: crate::types::BackendId,
    allow_packed_suffix: bool,
) -> Result<usize, StreamingError> {
    let status_code = buffer_response_for_request(
        request,
        io_buffer,
        conn,
        result_buf,
        pool,
        backend_id,
        allow_packed_suffix,
    )
    .await?;
    let data_len = result_buf.len();
    let response = std::mem::take(result_buf);
    request.complete_backend_response(backend_id, status_code, response);
    Ok(data_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BufferSize;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    fn make_pool() -> crate::pool::BufferPool {
        crate::pool::BufferPool::new(BufferSize::try_new(65536).unwrap(), 2)
    }

    async fn buffer_prefetched_for_test(
        conn: &mut crate::stream::ConnectionStream,
        first_chunk: &[u8],
        request_line: &[u8],
        status_code: u16,
        pool: &crate::pool::BufferPool,
    ) -> Result<crate::pool::ChunkedResponse, StreamingError> {
        let request = crate::protocol::RequestContext::parse(request_line)
            .expect("test request should parse");
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(first_chunk);
        let mut captured = crate::pool::ChunkedResponse::default();
        buffer_prefetched_response_for_request(
            PrefetchedResponseContext {
                request: &request,
                status_code: crate::protocol::StatusCode::new(status_code),
                initial_len: first_chunk.len(),
                pool,
                backend_id: crate::types::BackendId::from_index(1),
                allow_packed_suffix: false,
            },
            &mut io_buffer,
            conn,
            &mut captured,
        )
        .await?;
        Ok(captured)
    }

    async fn mock_backend_conn(chunks: Vec<Vec<u8>>) -> crate::stream::ConnectionStream {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for chunk in chunks {
                stream.write_all(&chunk).await.unwrap();
            }
            stream.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        crate::stream::ConnectionStream::plain(stream)
    }

    #[tokio::test]
    async fn buffered_multiline_response_returns_complete_response() {
        let response = b"220 Article follows\r\nLine 1\r\nLine 2\r\n.\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;

        let captured = buffer_prefetched_for_test(
            &mut conn,
            response,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await
        .unwrap();

        assert_eq!(captured.to_vec(), response);
    }

    #[tokio::test]
    async fn buffered_multiline_response_handles_empty_body() {
        let response = b"220 0 Article follows\r\n.\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;

        let captured = buffer_prefetched_for_test(
            &mut conn,
            response,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await
        .unwrap();

        assert_eq!(captured.to_vec(), response);
    }

    #[tokio::test]
    async fn buffered_multiline_response_errors_on_packed_suffix_from_first_chunk() {
        let first_response = b"220 Article follows\r\nLine 1\r\n.\r\n";
        let packed_next = b"223 0 <next@example>\r\n";
        let mut packed = Vec::from(first_response.as_slice());
        packed.extend_from_slice(packed_next);

        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;

        let err = buffer_prefetched_for_test(
            &mut conn,
            &packed,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, StreamingError::Io(_)));
        assert!(!conn.has_leftover());
    }

    #[tokio::test]
    async fn buffered_multiline_response_errors_on_packed_suffix_from_later_chunk() {
        let first_chunk = b"220 Article follows\r\nLine 1\r\n";
        let tail_chunk = b".\r\n223 0 <next@example>\r\n".to_vec();

        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![tail_chunk]).await;

        let err = buffer_prefetched_for_test(
            &mut conn,
            first_chunk,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, StreamingError::Io(_)));
        assert!(!conn.has_leftover());
    }

    #[tokio::test]
    async fn buffered_multiline_response_handles_large_article_across_chunks() {
        let header = b"220 Article follows\r\n";
        let mut body = Vec::new();
        for i in 0..1000 {
            body.extend_from_slice(format!("Line {i}\r\n").as_bytes());
        }
        let terminator = b".\r\n";

        let mut expected_response = Vec::new();
        expected_response.extend_from_slice(header);
        expected_response.extend_from_slice(&body);
        expected_response.extend_from_slice(terminator);

        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![expected_response[header.len()..].to_vec()]).await;

        let captured = buffer_prefetched_for_test(
            &mut conn,
            header,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await
        .expect("large multiline response should buffer successfully");

        assert_eq!(captured.to_vec(), expected_response);
    }

    #[tokio::test]
    async fn buffered_multiline_response_moves_scratch_buffers_across_chunks() {
        let header = b"220 Article follows\r\n";
        let mut body_chunk = vec![b'x'; 94];
        body_chunk.extend_from_slice(b"\r\n");
        let terminator = b".\r\n";
        let mut expected_response = Vec::new();
        expected_response.extend_from_slice(header);
        expected_response.extend_from_slice(&body_chunk);
        expected_response.extend_from_slice(terminator);

        let pool = crate::pool::BufferPool::new(BufferSize::try_new(64).unwrap(), 2)
            .with_capture_pool(64, 4);
        let mut io_buffer = pool.acquire();
        io_buffer.copy_from_slice(header);
        let mut captured = crate::pool::ChunkedResponse::default();
        let mut conn = mock_backend_conn(vec![body_chunk.clone(), terminator.to_vec()]).await;

        buffer_prefetched_response_for_request(
            PrefetchedResponseContext {
                request: &crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
                    .expect("valid request"),
                status_code: crate::protocol::StatusCode::new(220),
                initial_len: header.len(),
                pool: &pool,
                backend_id: crate::types::BackendId::from_index(1),
                allow_packed_suffix: false,
            },
            &mut io_buffer,
            &mut conn,
            &mut captured,
        )
        .await
        .expect("multiline buffering should succeed");

        assert_eq!(captured.to_vec(), expected_response);
        assert_eq!(
            pool.available_buffers(),
            0,
            "buffered multiline response should hold pooled read buffers instead of copying into capture buffers"
        );
    }

    #[tokio::test]
    async fn buffered_multiline_response_errors_on_full_buffer_mid_chunk_terminator() {
        let first_chunk = b"220 Article follows\r\nLong body content here";
        let expected_tail = b" more body\r\n.\r\n";
        let mut second_chunk = expected_tail.to_vec();
        second_chunk.resize(4096, b'X');

        let pool =
            crate::pool::BufferPool::new(crate::types::BufferSize::try_new(4096).unwrap(), 2);
        let mut conn = mock_backend_conn(vec![second_chunk]).await;

        let err = buffer_prefetched_for_test(
            &mut conn,
            first_chunk,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, StreamingError::Io(_)));
        assert!(!conn.has_leftover());
    }

    #[tokio::test]
    async fn buffered_response_reader_completes_request_context() {
        let response = b"223 0 <test@example>\r\n";
        let pool = make_pool();
        let mut io_buffer = pool.acquire();
        let mut captured = crate::pool::ChunkedResponse::default();
        let mut conn = mock_backend_conn(vec![response.to_vec()]).await;
        let backend_id = crate::types::BackendId::from_index(1);
        let mut request = crate::protocol::RequestContext::parse(b"STAT <test@example>\r\n")
            .expect("valid request line");

        read_response_into_context(
            &mut request,
            &mut io_buffer,
            &mut conn,
            &mut captured,
            &pool,
            backend_id,
            false,
        )
        .await
        .unwrap();

        assert_eq!(
            request.response_status(),
            Some(crate::protocol::StatusCode::new(223))
        );
        assert_eq!(request.backend_id(), Some(backend_id));
        assert_eq!(request.response_payload_eq(response), Some(true));
    }

    #[tokio::test]
    async fn buffered_multiline_response_errors_on_truncated_backend() {
        let partial = b"220 Article follows\r\nIncomplete body\r\n";
        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![]).await;

        let result = buffer_prefetched_for_test(
            &mut conn,
            partial,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await;
        assert!(
            matches!(result, Err(StreamingError::BackendEof { .. })),
            "EOF before terminator must be BackendEof"
        );
    }

    #[tokio::test]
    async fn response_reader_assembles_split_status_line() {
        let pool = make_pool();
        let request = crate::protocol::RequestContext::parse(b"STAT <test@example>\r\n")
            .expect("valid request");
        let mut io_buffer = pool.acquire();
        let mut buffered = crate::pool::ChunkedResponse::default();
        let mut conn = mock_backend_conn(vec![b" 0 <test@example>\r\n".to_vec()]).await;
        conn.stash_leftover(b"223").unwrap();

        let status = buffer_response_for_request(
            &request,
            &mut io_buffer,
            &mut conn,
            &mut buffered,
            &pool,
            crate::types::BackendId::from_index(1),
            false,
        )
        .await
        .expect("split status line should be reassembled");

        assert_eq!(status.as_u16(), 223);
        assert_eq!(buffered.to_vec(), b"223 0 <test@example>\r\n");
        assert!(!conn.has_leftover());
    }

    #[tokio::test]
    async fn article_430_response_is_buffered_as_single_line() {
        let pool = make_pool();
        let request = crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
            .expect("valid request");
        let mut io_buffer = pool.acquire();
        let mut buffered = crate::pool::ChunkedResponse::default();
        let packed = b"430 No article with that message-id\r\n223 0 <next@example>\r\n";
        let mut conn = mock_backend_conn(vec![packed.to_vec()]).await;

        let status = buffer_response_for_request(
            &request,
            &mut io_buffer,
            &mut conn,
            &mut buffered,
            &pool,
            crate::types::BackendId::from_index(0),
            false,
        )
        .await
        .expect("430 article miss should stay single-line");

        assert_eq!(status.as_u16(), 430);
        assert_eq!(
            buffered.to_vec(),
            b"430 No article with that message-id\r\n"
        );
        assert!(conn.has_leftover());
        assert_eq!(conn.leftover_len(), b"223 0 <next@example>\r\n".len());
    }

    #[tokio::test]
    async fn buffered_response_rejects_shared_leftover_article_with_packed_suffix() {
        let pool = make_pool();
        let request = crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
            .expect("valid request");
        let mut io_buffer = pool.acquire();
        let mut buffered = crate::pool::ChunkedResponse::default();
        let mut conn = mock_backend_conn(vec![]).await;
        let bytes =
            Bytes::from_static(b"xx220 Article follows\r\nbody\r\n.\r\n223 0 <next@example>\r\n");
        let response_start = 2;
        let bytes_len = bytes.len();
        conn.stash_leftover_shared(bytes, response_start..bytes_len)
            .unwrap();

        let result = buffer_response_for_request(
            &request,
            &mut io_buffer,
            &mut conn,
            &mut buffered,
            &pool,
            crate::types::BackendId::from_index(0),
            false,
        )
        .await;

        assert!(matches!(result, Err(StreamingError::Io(_))));
        assert!(!conn.has_leftover());
    }

    #[tokio::test]
    async fn single_line_buffering_stashes_packed_next_response() {
        let pool = make_pool();
        let request = crate::protocol::RequestContext::parse(b"STAT <test@example>\r\n")
            .expect("valid request");
        let mut io_buffer = pool.acquire();
        let mut buffered = crate::pool::ChunkedResponse::default();
        let packed = b"223 0 <test@example>\r\n430 No article with that message-id\r\n";
        let mut conn = mock_backend_conn(vec![packed.to_vec()]).await;

        let status = buffer_response_for_request(
            &request,
            &mut io_buffer,
            &mut conn,
            &mut buffered,
            &pool,
            crate::types::BackendId::from_index(0),
            false,
        )
        .await
        .expect("single-line response should buffer cleanly");

        assert_eq!(status.as_u16(), 223);
        assert_eq!(buffered.to_vec(), b"223 0 <test@example>\r\n");
        assert!(conn.has_leftover());
        assert_eq!(
            conn.leftover_len(),
            b"430 No article with that message-id\r\n".len()
        );
    }

    #[tokio::test]
    async fn single_line_buffering_preserves_packed_leftover_order_from_oversized_leftover_reads() {
        let pool = crate::pool::BufferPool::new(crate::types::BufferSize::try_new(24).unwrap(), 2);
        let first_request = crate::protocol::RequestContext::parse(b"STAT <one@example>\r\n")
            .expect("valid request");
        let second_request = crate::protocol::RequestContext::parse(b"STAT <two@example>\r\n")
            .expect("valid request");
        let mut io_buffer = pool.acquire();
        let mut buffered = crate::pool::ChunkedResponse::default();
        let mut conn = mock_backend_conn(vec![]).await;
        let packed = Bytes::from_static(b"223 0 <one>\r\n223 0 <two>\r\n223 0 <three>\r\n");
        conn.stash_leftover_shared(
            packed,
            0..b"223 0 <one>\r\n223 0 <two>\r\n223 0 <three>\r\n".len(),
        )
        .unwrap();

        let first_status = buffer_response_for_request(
            &first_request,
            &mut io_buffer,
            &mut conn,
            &mut buffered,
            &pool,
            crate::types::BackendId::from_index(0),
            false,
        )
        .await
        .expect("first response should buffer cleanly");
        assert_eq!(first_status.as_u16(), 223);
        assert_eq!(buffered.to_vec(), b"223 0 <one>\r\n");

        buffered.clear();
        let second_status = buffer_response_for_request(
            &second_request,
            &mut io_buffer,
            &mut conn,
            &mut buffered,
            &pool,
            crate::types::BackendId::from_index(0),
            false,
        )
        .await
        .expect("second response must remain ahead of later leftover bytes");

        assert_eq!(second_status.as_u16(), 223);
        assert_eq!(buffered.to_vec(), b"223 0 <two>\r\n");
        assert!(conn.has_leftover());
        assert_eq!(conn.leftover_len(), b"223 0 <three>\r\n".len());
    }

    #[tokio::test]
    async fn single_line_buffering_detaches_packed_suffix_split_from_pool() {
        let pool = crate::pool::BufferPool::new(crate::types::BufferSize::try_new(64).unwrap(), 2);
        assert_eq!(pool.available_buffers(), 2);

        {
            let request = crate::protocol::RequestContext::parse(b"STAT <test@example>\r\n")
                .expect("valid request");
            let mut io_buffer = pool.acquire();
            let mut buffered = crate::pool::ChunkedResponse::default();
            let packed = b"223 0 <test@example>\r\n430 No article with that message-id\r\n";
            let mut conn = mock_backend_conn(vec![packed.to_vec()]).await;

            let status = buffer_response_for_request(
                &request,
                &mut io_buffer,
                &mut conn,
                &mut buffered,
                &pool,
                crate::types::BackendId::from_index(0),
                false,
            )
            .await
            .expect("single-line response should buffer cleanly");

            assert_eq!(status.as_u16(), 223);
            assert_eq!(buffered.to_vec(), b"223 0 <test@example>\r\n");
        }

        assert_eq!(
            pool.available_buffers(),
            1,
            "packed suffix zero-copy splits detach the backing buffer from pool ownership"
        );
    }

    #[tokio::test]
    async fn buffered_multiline_response_rejects_boundary_terminator_with_later_chunk_suffix() {
        let first_chunk = b"220 Article follows\r\nLine 1\r\n.";
        let second_response = b"220 Next follows\r\nLine 2\r\n.\r\n";
        let mut later_chunk = b"\r\n".to_vec();
        later_chunk.extend_from_slice(second_response);

        let pool = make_pool();
        let mut conn = mock_backend_conn(vec![later_chunk]).await;

        let err = buffer_prefetched_for_test(
            &mut conn,
            first_chunk,
            b"ARTICLE <test@example>\r\n",
            220,
            &pool,
        )
        .await
        .unwrap_err();

        assert!(matches!(err, StreamingError::Io(_)));
        assert!(!conn.has_leftover());
    }

    #[tokio::test]
    async fn response_reader_rejects_invalid_status() {
        let pool = make_pool();
        let request = crate::protocol::RequestContext::parse(b"ARTICLE <test@example>\r\n")
            .expect("valid request");
        let mut io_buffer = pool.acquire();
        let mut buffered = crate::pool::ChunkedResponse::default();
        let mut conn = mock_backend_conn(vec![b"oops\r\n".to_vec()]).await;

        let result = buffer_response_for_request(
            &request,
            &mut io_buffer,
            &mut conn,
            &mut buffered,
            &pool,
            crate::types::BackendId::from_index(1),
            false,
        )
        .await;
        assert!(matches!(result, Err(StreamingError::Io(_))));
    }

    #[test]
    fn streaming_error_pool_fate_matches_disconnect_semantics() {
        let disconnect =
            StreamingError::ClientDisconnect(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        let eof = StreamingError::BackendEof {
            backend_id: crate::types::BackendId::from_index(0),
            bytes_received: 12,
        };
        let io = StreamingError::Io(anyhow::anyhow!("backend dirty"));

        assert!(!disconnect.must_remove_connection());
        assert!(eof.must_remove_connection());
        assert!(io.must_remove_connection());
    }
}
