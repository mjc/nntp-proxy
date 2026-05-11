//! Buffered NNTP response readers for pooled backend connections.
//!
//! This module owns the request-aware response buffering logic used by
//! per-command and pipeline paths. It intentionally excludes the old
//! direct-to-client multiline streaming machinery.

use anyhow::Result;
use tracing::warn;

use crate::session::tail_buffer::{TailBuffer, TerminatorStatus};

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

/// Immutable context required to buffer multiline replies.
pub(crate) struct BufferContext<'a> {
    pub backend_id: crate::types::BackendId,
    pub buffer_pool: &'a crate::pool::BufferPool,
}

fn stash_leftover(
    conn: &mut crate::stream::ConnectionStream,
    remainder: &[u8],
) -> Result<(), StreamingError> {
    if remainder.len() > crate::constants::buffer::MAX_LEFTOVER_BYTES {
        return Err(StreamingError::Io(anyhow::anyhow!(
            "Leftover exceeds {} bytes ({} bytes) — probable protocol desync",
            crate::constants::buffer::MAX_LEFTOVER_BYTES,
            remainder.len()
        )));
    }
    conn.stash_leftover(remainder).map_err(StreamingError::Io)?;
    Ok(())
}

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

pub(crate) async fn read_prefetched_response_status(
    io_buffer: &mut crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
) -> Result<(crate::protocol::StatusCode, usize), StreamingError> {
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
    Ok((status_code, initial_len))
}

pub(crate) fn buffer_prefetched_single_line_response(
    io_buffer: &crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    result_buf: &mut crate::pool::ChunkedResponse,
    pool: &crate::pool::BufferPool,
    initial_len: usize,
) -> Result<(), StreamingError> {
    result_buf.clear();
    let response = &io_buffer[..initial_len];
    result_buf.extend_from_slice(pool, response);
    if let Some(pos) = memchr::memchr(b'\n', response) {
        let end = pos + 1;
        if end < response.len() {
            stash_leftover(conn, &response[end..])?;
            result_buf.truncate(end);
        }
    }
    Ok(())
}

async fn fill_multiline_response(
    io_buffer: &mut crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    response: &mut crate::pool::ChunkedResponse,
    pool: &crate::pool::BufferPool,
    initial_chunk_len: usize,
    backend_id: crate::types::BackendId,
) -> Result<(), StreamingError> {
    response.clear();
    let mut tail = TailBuffer::default();

    match tail.detect_terminator(&io_buffer[..initial_chunk_len]) {
        TerminatorStatus::FoundAt(pos) => {
            response.extend_from_slice(pool, &io_buffer[..pos]);
            if pos < initial_chunk_len {
                stash_leftover(conn, &io_buffer[pos..initial_chunk_len])?;
            }
            return Ok(());
        }
        TerminatorStatus::NotFound => {
            tail.update(&io_buffer[..initial_chunk_len]);
            response.extend_from_slice(pool, &io_buffer[..initial_chunk_len]);
        }
    }

    loop {
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

        let status = tail.detect_terminator(&io_buffer[..n]);
        let write_len = status.write_len(n);

        if status.is_found() {
            response.extend_from_slice(pool, &io_buffer[..write_len]);
            if write_len < n {
                stash_leftover(conn, &io_buffer[write_len..n])?;
            }
            return Ok(());
        }

        debug_assert_eq!(
            write_len, n,
            "non-terminal multiline chunk should consume full read"
        );
        tail.update(&io_buffer[..write_len]);
        response.extend_from_slice(pool, &io_buffer[..write_len]);
    }
}

/// Read a complete NNTP response using request-aware framing.
pub(crate) async fn buffer_response_for_request(
    request: &crate::protocol::RequestContext,
    io_buffer: &mut crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    result_buf: &mut crate::pool::ChunkedResponse,
    pool: &crate::pool::BufferPool,
    backend_id: crate::types::BackendId,
) -> Result<crate::protocol::StatusCode, StreamingError> {
    let (status_code, initial_len) = read_prefetched_response_status(io_buffer, conn).await?;

    if !request.expects_multiline_response(status_code) {
        buffer_prefetched_single_line_response(io_buffer, conn, result_buf, pool, initial_len)?;
        return Ok(status_code);
    }

    fill_multiline_response(io_buffer, conn, result_buf, pool, initial_len, backend_id).await?;
    Ok(status_code)
}

/// Read a complete NNTP response and attach it to the matching request context.
pub(crate) async fn read_response_into_context(
    request: &mut crate::protocol::RequestContext,
    io_buffer: &mut crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    result_buf: &mut crate::pool::ChunkedResponse,
    pool: &crate::pool::BufferPool,
    backend_id: crate::types::BackendId,
) -> Result<(), StreamingError> {
    let status_code =
        buffer_response_for_request(request, io_buffer, conn, result_buf, pool, backend_id).await?;
    let response = std::mem::take(result_buf);
    request.complete_backend_response(backend_id, status_code, response);
    Ok(())
}

/// Buffer the current request's complete backend response after the first chunk was prefetched.
pub(crate) async fn buffer_multiline_response(
    conn: &mut crate::stream::ConnectionStream,
    first_chunk: &[u8],
    ctx: &BufferContext<'_>,
) -> Result<crate::pool::ChunkedResponse, StreamingError> {
    let mut io_buffer = ctx.buffer_pool.acquire();
    let mut captured = crate::pool::ChunkedResponse::default();
    validate_response_prefix(first_chunk, "prefetched")?;

    let mut tail = TailBuffer::default();
    match tail.detect_terminator(first_chunk) {
        TerminatorStatus::FoundAt(pos) => {
            captured.extend_from_slice(ctx.buffer_pool, &first_chunk[..pos]);
            if pos < first_chunk.len() {
                stash_leftover(conn, &first_chunk[pos..])?;
            }
        }
        TerminatorStatus::NotFound => {
            captured.extend_from_slice(ctx.buffer_pool, first_chunk);
            tail.update(first_chunk);

            loop {
                let n = io_buffer.read_from(conn).await.map_err(|e| {
                    StreamingError::Io(
                        anyhow::Error::from(e).context("Failed to read remaining response body"),
                    )
                })?;
                if n == 0 {
                    return Err(StreamingError::BackendEof {
                        backend_id: ctx.backend_id,
                        bytes_received: captured.len() as u64,
                    });
                }

                let status = tail.detect_terminator(&io_buffer[..n]);
                let write_len = status.write_len(n);

                if status.is_found() {
                    captured.extend_from_slice(ctx.buffer_pool, &io_buffer[..write_len]);
                    if write_len < n {
                        stash_leftover(conn, &io_buffer[write_len..n])?;
                    }
                    break;
                }

                debug_assert_eq!(
                    write_len, n,
                    "non-terminal multiline chunk should consume full read"
                );
                tail.update(&io_buffer[..write_len]);
                captured.extend_from_slice(ctx.buffer_pool, &io_buffer[..write_len]);
            }
        }
    }
    Ok(captured)
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

    fn make_ctx(pool: &crate::pool::BufferPool) -> BufferContext<'_> {
        BufferContext {
            backend_id: crate::types::BackendId::from_index(1),
            buffer_pool: pool,
        }
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
        let ctx = make_ctx(&pool);
        let mut conn = mock_backend_conn(vec![]).await;

        let captured = buffer_multiline_response(&mut conn, response, &ctx)
            .await
            .unwrap();

        assert_eq!(captured.to_vec(), response);
    }

    #[tokio::test]
    async fn buffered_multiline_response_handles_empty_body() {
        let response = b"220 0 Article follows\r\n.\r\n";
        let pool = make_pool();
        let ctx = make_ctx(&pool);
        let mut conn = mock_backend_conn(vec![]).await;

        let captured = buffer_multiline_response(&mut conn, response, &ctx)
            .await
            .unwrap();

        assert_eq!(captured.to_vec(), response);
    }

    #[tokio::test]
    async fn buffered_multiline_response_stashes_packed_next_response_from_first_chunk() {
        let first_response = b"220 Article follows\r\nLine 1\r\n.\r\n";
        let packed_next = b"223 0 <next@example>\r\n";
        let mut packed = Vec::from(first_response.as_slice());
        packed.extend_from_slice(packed_next);

        let pool = make_pool();
        let ctx = make_ctx(&pool);
        let mut conn = mock_backend_conn(vec![]).await;

        let captured = buffer_multiline_response(&mut conn, &packed, &ctx)
            .await
            .unwrap();

        assert_eq!(captured.to_vec(), first_response);
        assert!(conn.has_leftover());
        assert_eq!(conn.leftover_len(), packed_next.len());

        let mut io_buffer = pool.acquire();
        let (status, initial_len) = read_prefetched_response_status(&mut io_buffer, &mut conn)
            .await
            .expect("packed next status line should be read from leftover");
        assert_eq!(status.as_u16(), 223);

        let mut buffered = crate::pool::ChunkedResponse::default();
        buffer_prefetched_single_line_response(
            &io_buffer,
            &mut conn,
            &mut buffered,
            &pool,
            initial_len,
        )
        .expect("single-line leftover should buffer cleanly");
        assert_eq!(buffered.to_vec(), packed_next);
        assert!(!conn.has_leftover());
    }

    #[tokio::test]
    async fn buffered_multiline_response_stashes_packed_next_response_from_later_chunk() {
        let first_chunk = b"220 Article follows\r\nLine 1\r\n";
        let tail_chunk = b".\r\n223 0 <next@example>\r\n".to_vec();

        let pool = make_pool();
        let ctx = make_ctx(&pool);
        let mut conn = mock_backend_conn(vec![tail_chunk]).await;

        let captured = buffer_multiline_response(&mut conn, first_chunk, &ctx)
            .await
            .unwrap();

        assert_eq!(captured.to_vec(), b"220 Article follows\r\nLine 1\r\n.\r\n");
        assert!(conn.has_leftover());
        assert_eq!(conn.leftover_len(), b"223 0 <next@example>\r\n".len());

        let mut io_buffer = pool.acquire();
        let (status, initial_len) = read_prefetched_response_status(&mut io_buffer, &mut conn)
            .await
            .expect("later packed status line should be read from leftover");
        assert_eq!(status.as_u16(), 223);

        let mut buffered = crate::pool::ChunkedResponse::default();
        buffer_prefetched_single_line_response(
            &io_buffer,
            &mut conn,
            &mut buffered,
            &pool,
            initial_len,
        )
        .expect("single-line leftover should buffer cleanly");
        assert_eq!(buffered.to_vec(), b"223 0 <next@example>\r\n");
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
        let ctx = make_ctx(&pool);
        let mut conn = mock_backend_conn(vec![expected_response[header.len()..].to_vec()]).await;

        let captured = buffer_multiline_response(&mut conn, header, &ctx)
            .await
            .expect("large multiline response should buffer successfully");

        assert_eq!(captured.to_vec(), expected_response);
    }

    #[tokio::test]
    async fn buffered_multiline_response_handles_full_buffer_mid_chunk_terminator() {
        let first_chunk = b"220 Article follows\r\nLong body content here";
        let expected_tail = b" more body\r\n.\r\n";
        let mut second_chunk = expected_tail.to_vec();
        second_chunk.resize(4096, b'X');

        let pool =
            crate::pool::BufferPool::new(crate::types::BufferSize::try_new(4096).unwrap(), 2);
        let ctx = make_ctx(&pool);
        let mut conn = mock_backend_conn(vec![second_chunk]).await;

        let captured = buffer_multiline_response(&mut conn, first_chunk, &ctx)
            .await
            .expect("terminator inside full chunk should buffer response");

        let mut expected_response = Vec::new();
        expected_response.extend_from_slice(first_chunk);
        expected_response.extend_from_slice(expected_tail);
        assert_eq!(captured.to_vec(), expected_response);
        assert!(conn.has_leftover());
        assert_eq!(conn.leftover_len(), 4096 - expected_tail.len());
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
        let ctx = make_ctx(&pool);
        let mut conn = mock_backend_conn(vec![]).await;

        let result = buffer_multiline_response(&mut conn, partial, &ctx).await;
        assert!(
            matches!(result, Err(StreamingError::BackendEof { .. })),
            "EOF before terminator must be BackendEof"
        );
    }

    #[tokio::test]
    async fn prefetched_status_reader_assembles_split_status_line() {
        let pool = make_pool();
        let mut io_buffer = pool.acquire();
        let mut conn = mock_backend_conn(vec![b" 0 <test@example>\r\n".to_vec()]).await;
        conn.stash_leftover(b"223").expect("stash split prefix");

        let (status, initial_len) = read_prefetched_response_status(&mut io_buffer, &mut conn)
            .await
            .expect("split status line should be reassembled");

        assert_eq!(status.as_u16(), 223);
        assert_eq!(&io_buffer[..initial_len], b"223 0 <test@example>\r\n");
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
    async fn buffered_multiline_response_prefers_boundary_terminator_over_later_chunk_terminator() {
        let first_chunk = b"220 Article follows\r\nLine 1\r\n.";
        let second_response = b"220 Next follows\r\nLine 2\r\n.\r\n";
        let mut later_chunk = b"\r\n".to_vec();
        later_chunk.extend_from_slice(second_response);

        let pool = make_pool();
        let ctx = make_ctx(&pool);
        let mut conn = mock_backend_conn(vec![later_chunk]).await;

        let captured = buffer_multiline_response(&mut conn, first_chunk, &ctx)
            .await
            .expect("boundary terminator should finish first response");

        assert_eq!(captured.to_vec(), b"220 Article follows\r\nLine 1\r\n.\r\n");
        assert!(conn.has_leftover());
        assert_eq!(conn.leftover_len(), second_response.len());

        let mut io_buffer = pool.acquire();
        let (status, initial_len) = read_prefetched_response_status(&mut io_buffer, &mut conn)
            .await
            .expect("second response should remain buffered as leftover");
        assert_eq!(status.as_u16(), 220);
        assert_eq!(&io_buffer[..initial_len], second_response);
    }

    #[tokio::test]
    async fn buffered_multiline_response_rejects_invalid_prefetched_status() {
        let pool = make_pool();
        let ctx = make_ctx(&pool);
        let mut conn = mock_backend_conn(vec![]).await;

        let result = buffer_multiline_response(&mut conn, b"oops\r\n", &ctx).await;
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
