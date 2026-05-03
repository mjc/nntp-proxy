//! Backend pipeline worker for request multiplexing
//!
//! Each backend has one long-running worker task that:
//! 1. Dequeues batches of requests from the backend's queue
//! 2. Writes all commands to a single backend connection (pipelining)
//! 3. Reads responses in order and routes them back to client sessions
//!
//! This enables N client sessions to share M backend connections (N >> M).

#[cfg(test)]
use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use crate::metrics::MetricsCollector;
use crate::pool::{BufferPool, DeadpoolConnectionProvider};
use crate::protocol::ResponsePayloadLen;
use crate::router::backend_queue::{BackendQueue, PipelineError, QueuedContext};
#[cfg(test)]
use crate::session::backend::parse_backend_status;
use crate::types::BackendId;

/// Configuration for the pipeline worker
#[derive(Debug, Clone)]
pub(crate) struct PipelineWorkerConfig {
    /// Maximum number of commands in a single pipeline batch
    pub(crate) batch_size: usize,
    /// Backend identifier for logging/metrics
    pub(crate) backend_id: BackendId,
}

/// Run the pipeline worker loop for a single backend.
///
/// This function runs forever (until the task is cancelled). It:
/// 1. Waits for requests in the backend's queue
/// 2. Dequeues a batch of up to `config.batch_size` requests
/// 3. Acquires a pooled backend connection
/// 4. Writes all commands, then reads all responses in order
/// 5. Sends each response back to the waiting client via oneshot channel
///
/// On connection errors, the entire remaining batch is failed and the worker
/// retries with a fresh connection on the next iteration.
pub(crate) async fn backend_pipeline_worker(
    config: PipelineWorkerConfig,
    queue: Arc<BackendQueue>,
    provider: Arc<DeadpoolConnectionProvider>,
    metrics: MetricsCollector,
    buffer_pool: BufferPool,
) {
    let backend_id = config.backend_id;
    info!(
        "Pipeline worker started for backend {:?} (batch_size={})",
        backend_id, config.batch_size
    );

    // Hoist buffers to worker lifetime (reused across batches)
    let mut result_buf = crate::pool::ChunkedResponse::default();
    let mut batch = Vec::with_capacity(config.batch_size);

    loop {
        // Wait for at least one request, then grab up to batch_size
        batch = queue.dequeue_batch(config.batch_size, batch).await;
        let batch_len = batch.len();

        debug!(
            "Pipeline worker backend {:?}: dequeued batch of {} requests",
            backend_id, batch_len
        );

        // Clear buffers for this batch
        result_buf.clear();

        // Acquire a connection from the pool
        let conn_raw = match provider.get_pooled_connection().await {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "Pipeline worker backend {:?}: connection acquisition failed: {}",
                    backend_id, e
                );
                metrics.record_connection_failure(backend_id);
                batch = fail_batch(batch, PipelineError::ConnectionAcquire);
                continue;
            }
        };
        let mut conn = crate::pool::ConnectionGuard::new(conn_raw, (*provider).clone());

        // Execute the batch: write all commands, then read all responses
        let success;
        (success, batch) = execute_pipeline_batch(
            backend_id,
            &mut conn,
            batch,
            &metrics,
            &buffer_pool,
            &mut result_buf,
        )
        .await;

        if success {
            let _ = conn.release(); // batch healthy; return connection to pool
        }
        // !success: conn drops here → ConnectionGuard::remove_with_cooldown
        // Unconditional: !success means a backend write, flush, or read failed.
        // Client disconnects only close the oneshot response channel; they do
        // not affect the backend connection and cannot cause !success here.

        // Record pipeline batch metrics
        if batch_len > 1 {
            metrics.record_pipeline_batch(batch_len as u64);
        }
    }
}

/// Execute a batch of commands on a single connection using write-write-read-read pipelining.
///
/// Returns `(healthy, batch)` — whether the connection is still healthy, plus the
/// (now-drained) batch Vec for allocation reuse.
#[allow(clippy::iter_with_drain)] // batch returned to caller for reuse; drain preserves allocation
async fn execute_pipeline_batch(
    backend_id: BackendId,
    conn: &mut crate::stream::ConnectionStream,
    mut batch: Vec<QueuedContext>,
    metrics: &MetricsCollector,
    buffer_pool: &BufferPool,
    result_buf: &mut crate::pool::ChunkedResponse,
) -> (bool, Vec<QueuedContext>) {
    let batch_len = batch.len();

    // Phase 1: Write all commands
    for (i, req) in batch.iter().enumerate() {
        if let Err(e) = crate::session::backend::write_request(conn, &req.context).await {
            warn!(
                "Pipeline worker backend {:?}: write failed at command {}/{}: {}",
                backend_id,
                i + 1,
                batch_len,
                e
            );
            // We wrote commands 0..i successfully but can't read responses
            // because the connection is broken. Fail everything and return early.
            let batch = fail_batch(
                batch,
                PipelineError::WriteFailed {
                    index: i + 1,
                    batch_len,
                },
            );
            return (false, batch);
        }
    }
    // All writes succeeded, continue to flush

    // Flush after writing all commands
    if let Err(e) = conn.flush().await {
        warn!(
            "Pipeline worker backend {:?}: flush failed: {}",
            backend_id, e
        );
        let batch = fail_batch(batch, PipelineError::FlushFailed);
        return (false, batch);
    }

    // Phase 2: Read responses in order (with shared buffer + connection-stashed leftovers)
    let mut buffer = buffer_pool.acquire().await;
    // result_buf is passed in as a parameter (hoisted to worker loop)

    let mut batch_iter = batch.drain(..).enumerate();
    while let Some((i, mut req)) = batch_iter.next() {
        // Read the response for this command
        match crate::session::streaming::read_response_into_context(
            &mut req.context,
            &mut buffer,
            conn,
            result_buf,
            buffer_pool,
            backend_id,
        )
        .await
        .map_err(crate::session::streaming::StreamingError::into_anyhow)
        {
            Ok(()) => {
                metrics.record_command(backend_id);
                let data_len = req
                    .context
                    .response_payload_len()
                    .map_or(0, ResponsePayloadLen::get);
                metrics.record_backend_to_client_bytes_for(backend_id, data_len as u64);
                metrics.record_client_to_backend_bytes_for(
                    backend_id,
                    req.context.request_wire_len().as_u64(),
                );

                req.complete_context();
            }
            Err(e) => {
                warn!(
                    backend = ?backend_id,
                    response_index = i + 1,
                    batch_size = batch_len,
                    command_verb = ?req.context.verb(),
                    error = %e,
                    leftover_bytes = conn.leftover_len(),
                    "Pipeline worker read failed"
                );

                // Fail this request
                req.complete_error(PipelineError::ReadFailed);

                // Fail remaining requests (no Vec allocation - iterate directly)
                let error_msg = PipelineError::ConnectionLost {
                    completed: i + 1,
                    batch_len,
                };
                for (_, remaining_req) in batch_iter {
                    remaining_req.complete_error(error_msg);
                }

                // Connection broken — return false immediately
                return (false, batch);
            }
        }
    }
    // Release the drain iterator's mutable borrow before returning batch
    drop(batch_iter);

    // All responses processed successfully. Leftover is valid only while there
    // are more already-sent responses remaining in this borrow. If bytes remain
    // after the final expected response, returning the connection to the pool
    // would leak stale backend data into the next borrower.
    debug_assert!(
        conn.leftover_len() <= crate::constants::buffer::MAX_LEFTOVER_BYTES,
        "Leftover buffer should never exceed MAX_LEFTOVER_BYTES after successful batch"
    );

    if conn.has_leftover() {
        warn!(
            backend = ?backend_id,
            leftover_bytes = conn.leftover_len(),
            "Pipeline batch ended with buffered bytes; retiring connection to avoid cross-borrow desync"
        );
        return (false, batch);
    }

    (true, batch)
}

/// Buffer the complete response shape for the supplied request context.
///
/// Fills `result_buf` with the response bytes for the supplied request context
/// and returns the parsed status code.
///
/// `result_buf` is cleared and reused for each response to avoid per-response allocations.
#[cfg(test)]
async fn buffer_response_for_request(
    request_line: &[u8],
    buffer: &mut crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    result_buf: &mut crate::pool::ChunkedResponse,
    pool: &BufferPool,
) -> Result<crate::protocol::StatusCode> {
    let request = crate::protocol::RequestContext::parse(request_line).expect("valid request line");
    crate::session::streaming::buffer_response_for_request(
        &request,
        buffer,
        conn,
        result_buf,
        pool,
        BackendId::from_index(0),
    )
    .await
    .map_err(crate::session::streaming::StreamingError::into_anyhow)
}

/// Send error responses to all requests in a batch.
///
/// Takes ownership of the Vec, drains it, and returns the empty Vec
/// so the caller can reuse the allocation.
#[allow(clippy::iter_with_drain)] // drain used intentionally; empty Vec returned for allocation reuse
fn fail_batch(mut batch: Vec<QueuedContext>, error: PipelineError) -> Vec<QueuedContext> {
    for req in batch.drain(..) {
        req.complete_error(error);
    }
    batch
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::BufferPool;
    use crate::protocol::RequestContext;
    use crate::router::backend_queue::PipelineResponse;
    use crate::stream::ConnectionStream;
    use proptest::prelude::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const ARTICLE_REQUEST: &[u8] = b"ARTICLE <test@example>\r\n";
    const DATE_REQUEST: &[u8] = b"DATE\r\n";
    const STAT_REQUEST: &[u8] = b"STAT <test@example>\r\n";

    fn queued_context(
        command: &str,
    ) -> (
        QueuedContext,
        tokio::sync::oneshot::Receiver<PipelineResponse>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (
            QueuedContext::new(request_context(command.as_bytes()), tx),
            rx,
        )
    }

    fn request_context(line: &[u8]) -> RequestContext {
        RequestContext::parse(line).expect("valid request line")
    }

    fn expect_success_status(response: PipelineResponse) -> u16 {
        match response {
            Ok(completed) => completed
                .context
                .response_metadata()
                .expect("completed queued request records response status")
                .status()
                .as_u16(),
            other @ Err(_) => panic!("Expected Success, got {other:?}"),
        }
    }

    /// Helper: create a TCP pair where the server writes `data` then optionally closes.
    /// Returns a `ConnectionStream` connected to the mock server.
    async fn mock_backend_conn(data: &[u8]) -> ConnectionStream {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let data = data.to_vec();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream.write_all(&data).await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        ConnectionStream::plain(stream)
    }

    /// Helper: create a TCP pair where the server writes `chunks` with delays.
    /// Returns a `ConnectionStream` connected to the mock server.
    async fn mock_backend_conn_chunked(chunks: Vec<Vec<u8>>) -> ConnectionStream {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            for chunk in chunks {
                stream.write_all(&chunk).await.unwrap();
                // Small delay to ensure separate TCP segments
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
            stream.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        ConnectionStream::plain(stream)
    }

    async fn run_single_line_pipeline_batch(
        responses: &[Vec<u8>],
        extra_tail: &[u8],
    ) -> (bool, Vec<PipelineResponse>, bool, usize) {
        use tokio::io::AsyncReadExt;

        let pool = BufferPool::for_tests();
        let metrics = MetricsCollector::new(responses.len());
        let backend_id = BackendId::from_index(0);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let wire_data = {
            let mut data = Vec::new();
            for response in responses {
                data.extend_from_slice(response);
            }
            data.extend_from_slice(extra_tail);
            data
        };

        let mut batch = Vec::with_capacity(responses.len());
        let mut rxs = Vec::with_capacity(responses.len());
        let mut expected_command_bytes = 0usize;
        for idx in 0..responses.len() {
            let command = format!("STAT <msg{idx}@example.com>\r\n");
            expected_command_bytes += command.len();
            let (request, rx) = queued_context(&command);
            batch.push(request);
            rxs.push(rx);
        }

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let mut received = 0usize;
            while received < expected_command_bytes {
                match stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => received += n,
                    Err(_) => break,
                }
            }
            stream.write_all(&wire_data).await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut conn = ConnectionStream::plain(stream);

        let mut result_buf = crate::pool::ChunkedResponse::default();
        let (success, _batch) = execute_pipeline_batch(
            backend_id,
            &mut conn,
            batch,
            &metrics,
            &pool,
            &mut result_buf,
        )
        .await;

        let mut results = Vec::with_capacity(rxs.len());
        for rx in rxs {
            results.push(rx.await.unwrap());
        }

        (success, results, conn.has_leftover(), conn.leftover_len())
    }

    // ─── buffer_response_for_request unit tests ───────────────────────────────────────

    #[tokio::test]
    async fn test_buffer_response_for_request_single_line() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = crate::pool::ChunkedResponse::default();

        let mut conn = mock_backend_conn(b"430 No such article\r\n").await;

        let status = buffer_response_for_request(
            ARTICLE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await
        .expect("should parse single-line response");

        assert_eq!(status.as_u16(), 430);
        assert_eq!(result_buf.to_vec(), b"430 No such article\r\n");
        assert!(!conn.has_leftover());
    }

    #[tokio::test]
    async fn test_buffer_response_for_request_single_line_split_after_status_code() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = crate::pool::ChunkedResponse::default();

        let mut conn =
            mock_backend_conn_chunked(vec![b"111".to_vec(), b" 20260501173336\r\n".to_vec()]).await;

        let status = buffer_response_for_request(
            DATE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await
        .expect("should wait for complete status line");

        assert_eq!(status.as_u16(), 111);
        assert_eq!(result_buf.to_vec(), b"111 20260501173336\r\n");
        assert!(!conn.has_leftover());
    }

    #[tokio::test]
    async fn test_buffer_response_for_request_multiline() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = crate::pool::ChunkedResponse::default();

        let response = b"220 0 <msg@id> article\r\nSubject: test\r\n\r\nBody line\r\n.\r\n";
        let mut conn = mock_backend_conn(response).await;

        let status = buffer_response_for_request(
            ARTICLE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await
        .expect("should parse multiline response");

        assert_eq!(status.as_u16(), 220);
        assert_eq!(result_buf.to_vec(), response);
        assert!(!conn.has_leftover());
    }

    #[tokio::test]
    async fn test_buffer_response_for_request_multiline_across_chunks() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = crate::pool::ChunkedResponse::default();

        // Split the terminator \r\n.\r\n across two chunks
        let chunk1 = b"220 0 <msg@id> article\r\nBody\r\n.".to_vec();
        let chunk2 = b"\r\n".to_vec();

        let mut conn = mock_backend_conn_chunked(vec![chunk1, chunk2]).await;

        let status = buffer_response_for_request(
            ARTICLE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await
        .expect("should detect terminator across chunks");

        assert_eq!(status.as_u16(), 220);
        assert!(
            result_buf.ends_with(b"\r\n.\r\n"),
            "response should end with terminator"
        );
        assert!(!conn.has_leftover());
    }

    #[tokio::test]
    async fn test_buffer_response_for_request_with_leftover() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = crate::pool::ChunkedResponse::default();

        // Two responses packed together
        let packed = b"220 0 <a@b> article\r\nBody\r\n.\r\n430 No such article\r\n";
        let mut conn = mock_backend_conn(packed).await;

        // First read should get the multiline response and save leftover
        let status1 = buffer_response_for_request(
            ARTICLE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await
        .expect("should parse first response");

        assert_eq!(status1.as_u16(), 220);
        assert!(result_buf.ends_with(b"\r\n.\r\n"));
        assert!(
            conn.has_leftover(),
            "should have leftover from second response"
        );

        // Second read should consume leftover and return the 430
        let status2 = buffer_response_for_request(
            ARTICLE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await
        .expect("should parse second response from leftover");

        assert_eq!(status2.as_u16(), 430);
        assert_eq!(result_buf.to_vec(), b"430 No such article\r\n");
    }

    #[tokio::test]
    async fn test_buffer_response_for_request_uses_request_context_for_same_status_framing() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = crate::pool::ChunkedResponse::default();

        let packed = b"211 3 10 12 group.name\r\n211 3 10 12 group.name\r\n10\r\n11\r\n12\r\n.\r\n";
        let mut conn = mock_backend_conn(packed).await;

        let group_status = buffer_response_for_request(
            b"GROUP group.name\r\n",
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await
        .expect("GROUP 211 is single-line");
        assert_eq!(group_status.as_u16(), 211);
        assert_eq!(result_buf.to_vec(), b"211 3 10 12 group.name\r\n");
        assert!(conn.has_leftover());

        let listgroup_status = buffer_response_for_request(
            b"LISTGROUP group.name\r\n",
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await
        .expect("LISTGROUP 211 is multiline");
        assert_eq!(listgroup_status.as_u16(), 211);
        assert_eq!(
            result_buf.to_vec(),
            b"211 3 10 12 group.name\r\n10\r\n11\r\n12\r\n.\r\n"
        );
    }

    #[tokio::test]
    async fn test_buffer_response_for_request_eof_mid_stream() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = crate::pool::ChunkedResponse::default();

        // Multiline response without terminator — server disconnects
        let mut conn = mock_backend_conn(b"220 0 <a@b> article\r\nBody line\r\n").await;

        let result = buffer_response_for_request(
            ARTICLE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await;
        assert!(result.is_err(), "EOF before terminator should be an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("closed connection before multiline terminator"),
            "Error should describe the premature backend close: {err}"
        );
    }

    #[tokio::test]
    async fn test_buffer_response_for_request_invalid_status() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = crate::pool::ChunkedResponse::default();

        let mut conn = mock_backend_conn(b"garbage data here\r\n").await;

        let result = buffer_response_for_request(
            ARTICLE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await;
        assert!(result.is_err(), "Invalid status code should be an error");
    }

    #[tokio::test]
    async fn test_buffer_response_for_request_short_leftover_triggers_h5() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = crate::pool::ChunkedResponse::default();

        // Backend will provide the rest of the response
        let mut conn = mock_backend_conn(b"0 No such article\r\n").await;
        // Leftover is only 2 bytes — too short to validate, triggers H5 additional read
        conn.stash_leftover(b"43").unwrap();

        let status = buffer_response_for_request(
            ARTICLE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await
        .expect("short leftover + read should produce valid response");

        assert_eq!(status.as_u16(), 430);
        assert!(result_buf.starts_with(b"430"));
    }

    #[tokio::test]
    async fn test_buffer_response_for_request_empty_connection() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = crate::pool::ChunkedResponse::default();

        // Server immediately closes
        let mut conn = mock_backend_conn(b"").await;

        let result = buffer_response_for_request(
            ARTICLE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await;
        assert!(result.is_err(), "Empty connection should be an error");
    }

    // ─── parse_backend_status unit tests ────────────────────────────────

    #[test]
    fn test_validate_empty_response() {
        let validated = parse_backend_status(b"", 0, crate::protocol::MIN_RESPONSE_LENGTH);
        assert_eq!(validated.status_code, None);
        assert!(
            validated
                .warnings
                .contains(&crate::session::backend::ResponseWarning::InvalidResponse)
        );
    }

    #[test]
    fn test_validate_response_with_only_status_code() {
        // "220" with no CRLF — too short
        let data = b"220";
        let validated =
            parse_backend_status(data, data.len(), crate::protocol::MIN_RESPONSE_LENGTH);
        // Should still parse the status code but emit warning for short response
        assert!(!validated.warnings.is_empty());
        // Status code parsing should work even for short responses
        assert_eq!(
            validated.status_code,
            Some(crate::protocol::StatusCode::new(220)),
            "3-digit code should be parseable even if short"
        );
    }

    #[test]
    fn test_validate_response_with_binary_garbage() {
        let data = &[0xFF, 0xFE, 0x00, 0x01, 0x02];
        let validated =
            parse_backend_status(data, data.len(), crate::protocol::MIN_RESPONSE_LENGTH);
        assert_eq!(validated.status_code, None);
        assert!(
            validated
                .warnings
                .contains(&crate::session::backend::ResponseWarning::InvalidResponse)
        );
    }

    #[test]
    fn test_validate_valid_single_line() {
        let data = b"430 No such article\r\n";
        let validated =
            parse_backend_status(data, data.len(), crate::protocol::MIN_RESPONSE_LENGTH);
        assert_eq!(
            validated.status_code,
            Some(crate::protocol::StatusCode::new(430))
        );
    }

    // ─── execute_pipeline_batch integration tests ────────────────────────────

    #[tokio::test]
    async fn test_pipeline_batch_single_430() {
        let pool = BufferPool::for_tests();
        let metrics = MetricsCollector::new(1);
        let backend_id = BackendId::from_index(0);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Read command, then respond with 430
            let mut buf = [0u8; 256];
            let _ = stream.read(&mut buf).await;
            stream.write_all(b"430 No such article\r\n").await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut conn: crate::stream::ConnectionStream = ConnectionStream::plain(stream);

        let (request, rx) = queued_context("STAT <test@msg.id>\r\n");
        let batch = vec![request];

        let mut result_buf = crate::pool::ChunkedResponse::default();

        let (success, _batch) = execute_pipeline_batch(
            backend_id,
            &mut conn,
            batch,
            &metrics,
            &pool,
            &mut result_buf,
        )
        .await;

        assert!(success, "single 430 should not mark connection as broken");
        assert_eq!(expect_success_status(rx.await.unwrap()), 430);
    }

    #[tokio::test]
    async fn test_pipeline_batch_multiple_responses() {
        use tokio::io::AsyncReadExt;

        let pool = BufferPool::for_tests();
        let metrics = MetricsCollector::new(2);
        let backend_id = BackendId::from_index(0);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // Read both commands
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf).await;
            // Respond with two responses packed together
            stream
                .write_all(b"223 0 <a@b> status\r\n430 No such article\r\n")
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut conn = ConnectionStream::plain(stream);

        let (req1, rx1) = queued_context("STAT <a@b>\r\n");
        let (req2, rx2) = queued_context("STAT <c@d>\r\n");
        let batch = vec![req1, req2];

        let mut result_buf = crate::pool::ChunkedResponse::default();

        let (success, _batch) = execute_pipeline_batch(
            backend_id,
            &mut conn,
            batch,
            &metrics,
            &pool,
            &mut result_buf,
        )
        .await;
        assert!(success);

        let first = rx1.await.unwrap().unwrap();
        let second = rx2.await.unwrap().unwrap();

        assert_eq!(
            first
                .context
                .response_metadata()
                .map(|response| response.status().as_u16()),
            Some(223)
        );
        assert_eq!(first.context.message_id(), Some("<a@b>"));
        assert_eq!(first.context.backend_id(), Some(backend_id));
        assert_eq!(
            second
                .context
                .response_metadata()
                .map(|response| response.status().as_u16()),
            Some(430)
        );
        assert_eq!(second.context.message_id(), Some("<c@d>"));
        assert_eq!(second.context.backend_id(), Some(backend_id));
    }

    #[tokio::test]
    async fn test_pipeline_batch_completes_mixed_clients_in_connection_fifo() {
        use tokio::io::AsyncReadExt;

        let pool = BufferPool::for_tests();
        let metrics = MetricsCollector::new(3);
        let backend_id = BackendId::from_index(0);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf).await;
            stream
                .write_all(
                    b"223 0 <c1-r1@example> status\r\n\
                      430 No such article\r\n\
                      223 0 <c3-r1@example> status\r\n",
                )
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut conn = ConnectionStream::plain(stream);

        let (client1_req1, client1_rx1) = queued_context("STAT <c1-r1@example>\r\n");
        let (client2_req1, client2_rx1) = queued_context("STAT <c2-r1@example>\r\n");
        let (client1_req2, client1_rx2) = queued_context("STAT <c1-r2@example>\r\n");
        let batch = vec![client1_req1, client2_req1, client1_req2];

        let mut result_buf = crate::pool::ChunkedResponse::default();

        let (success, _batch) = execute_pipeline_batch(
            backend_id,
            &mut conn,
            batch,
            &metrics,
            &pool,
            &mut result_buf,
        )
        .await;
        assert!(success);

        let client1_first = client1_rx1.await.unwrap().unwrap();
        let client2_first = client2_rx1.await.unwrap().unwrap();
        let client1_second = client1_rx2.await.unwrap().unwrap();

        let completed = [client1_first, client2_first, client1_second].map(|completed| {
            (
                completed.context.message_id().map(str::to_owned),
                completed
                    .context
                    .response_metadata()
                    .map(|response| response.status().as_u16()),
            )
        });

        assert_eq!(
            completed,
            [
                (Some("<c1-r1@example>".to_owned()), Some(223)),
                (Some("<c2-r1@example>".to_owned()), Some(430)),
                (Some("<c1-r2@example>".to_owned()), Some(223)),
            ]
        );
    }

    #[tokio::test]
    async fn test_pipeline_batches_keep_independent_backend_connection_fifo() {
        async fn run_backend_batch(
            backend_id: BackendId,
            responses: &'static [u8],
            commands: [&str; 2],
        ) -> [PipelineResponse; 2] {
            use tokio::io::AsyncReadExt;

            let pool = BufferPool::for_tests();
            let metrics = MetricsCollector::new(2);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();

            tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 512];
                let _ = stream.read(&mut buf).await;
                stream.write_all(responses).await.unwrap();
                stream.shutdown().await.unwrap();
            });

            let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            let mut conn = ConnectionStream::plain(stream);
            let (req1, rx1) = queued_context(commands[0]);
            let (req2, rx2) = queued_context(commands[1]);
            let mut result_buf = crate::pool::ChunkedResponse::default();

            let (success, _batch) = execute_pipeline_batch(
                backend_id,
                &mut conn,
                vec![req1, req2],
                &metrics,
                &pool,
                &mut result_buf,
            )
            .await;
            assert!(success);

            [rx1.await.unwrap(), rx2.await.unwrap()]
        }

        let backend0 = BackendId::from_index(0);
        let backend1 = BackendId::from_index(1);

        let (batch0, batch1) = tokio::join!(
            run_backend_batch(
                backend0,
                b"223 0 <b0-a@example> status\r\n430 No such article\r\n",
                ["STAT <b0-a@example>\r\n", "STAT <b0-b@example>\r\n"],
            ),
            run_backend_batch(
                backend1,
                b"430 No such article\r\n223 0 <b1-b@example> status\r\n",
                ["STAT <b1-a@example>\r\n", "STAT <b1-b@example>\r\n"],
            ),
        );

        let summarize = |response: PipelineResponse| {
            let completed = response.expect("pipeline request should complete");
            (
                completed.context.backend_id(),
                completed.context.message_id().map(str::to_owned),
                completed
                    .context
                    .response_metadata()
                    .map(|metadata| metadata.status().as_u16()),
            )
        };

        assert_eq!(
            batch0.map(summarize),
            [
                (Some(backend0), Some("<b0-a@example>".to_owned()), Some(223)),
                (Some(backend0), Some("<b0-b@example>".to_owned()), Some(430)),
            ]
        );
        assert_eq!(
            batch1.map(summarize),
            [
                (Some(backend1), Some("<b1-a@example>".to_owned()), Some(430)),
                (Some(backend1), Some("<b1-b@example>".to_owned()), Some(223)),
            ]
        );
    }

    #[tokio::test]
    async fn test_pipeline_batch_server_disconnect_mid_batch() {
        use tokio::io::AsyncReadExt;

        let pool = BufferPool::for_tests();
        let metrics = MetricsCollector::new(2);
        let backend_id = BackendId::from_index(0);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf).await;
            // Only send first response, then disconnect
            stream.write_all(b"223 0 <a@b> status\r\n").await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut conn = ConnectionStream::plain(stream);

        let (req1, rx1) = queued_context("STAT <a@b>\r\n");
        let (req2, rx2) = queued_context("STAT <c@d>\r\n");
        let batch = vec![req1, req2];

        let mut result_buf = crate::pool::ChunkedResponse::default();

        let (success, _batch) = execute_pipeline_batch(
            backend_id,
            &mut conn,
            batch,
            &metrics,
            &pool,
            &mut result_buf,
        )
        .await;
        assert!(!success, "server disconnect should mark connection broken");

        // First response should succeed
        assert_eq!(expect_success_status(rx1.await.unwrap()), 223);
        // Second response should be error
        match rx2.await.unwrap() {
            Err(_) => {} // Expected
            other @ Ok(_) => {
                panic!("Expected error for second, got {other:?}")
            }
        }
    }

    #[tokio::test]
    async fn test_pipeline_batch_retires_connection_with_leftover_after_final_response() {
        use tokio::io::AsyncReadExt;

        let pool = BufferPool::for_tests();
        let metrics = MetricsCollector::new(2);
        let backend_id = BackendId::from_index(0);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf).await;
            stream
                .write_all(b"223 0 <a@b> status\r\n430 No such article\r\n111 20260411120000\r\n")
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut conn = ConnectionStream::plain(stream);

        let (req1, rx1) = queued_context("STAT <a@b>\r\n");
        let (req2, rx2) = queued_context("STAT <c@d>\r\n");
        let batch = vec![req1, req2];

        let mut result_buf = crate::pool::ChunkedResponse::default();

        let (success, _batch) = execute_pipeline_batch(
            backend_id,
            &mut conn,
            batch,
            &metrics,
            &pool,
            &mut result_buf,
        )
        .await;

        assert!(
            !success,
            "leftover after the final expected response should retire the connection"
        );
        assert!(
            conn.has_leftover(),
            "unexpected extra response bytes should remain buffered until the connection is retired"
        );

        assert_eq!(expect_success_status(rx1.await.unwrap()), 223);
        assert_eq!(expect_success_status(rx2.await.unwrap()), 430);
    }

    #[test]
    fn test_fail_batch_sends_same_error_to_all_requests() {
        let (req1, rx1) = queued_context("STAT <a@b>\r\n");
        let (req2, rx2) = queued_context("STAT <c@d>\r\n");
        let error = PipelineError::ConnectionLost {
            completed: 1,
            batch_len: 2,
        };

        let reused = fail_batch(vec![req1, req2], error);

        assert!(
            reused.is_empty(),
            "drained batch should be returned empty for reuse"
        );
        [rx1.blocking_recv().unwrap(), rx2.blocking_recv().unwrap()]
            .into_iter()
            .for_each(|response| assert!(matches!(response, Err(e) if e == error)));
    }

    proptest! {
        #[test]
        fn prop_buffer_response_for_request_preserves_intra_batch_single_line_framing(
            // RFC 3977 command/response model:
            // - client may pipeline multiple commands on one TCP stream
            // - server processes them in order and sends responses in that order
            //   (RFC 3977 §3.5)
            //
            // These generated responses are intentionally single-line so the end
            // of each response is the first CRLF, matching the framing rule we
            // use before handing any remainder to the next already-sent command.
            // 223 STAT success and 430/500 errors are all single-line responses
            // under RFC 3977.
            codes in prop::collection::vec(prop_oneof![Just(223u16), Just(430u16), Just(500u16)], 1..6),
            suffix in prop::collection::vec(any::<u8>(), 0..16),
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let pool = BufferPool::for_tests();
                let mut buffer = pool.acquire().await;
                let mut result_buf = crate::pool::ChunkedResponse::default();

                let responses: Vec<Vec<u8>> = codes
                    .iter()
                    .enumerate()
                    .map(|(idx, code)| format!("{code} resp{idx}\r\n").into_bytes())
                    .collect();

                let mut wire = Vec::new();
                for response in &responses {
                    wire.extend_from_slice(response);
                }
                wire.extend_from_slice(&suffix);

                let mut conn = mock_backend_conn(&wire).await;

                for (idx, expected) in responses.iter().enumerate() {
                    let status = buffer_response_for_request(
                        STAT_REQUEST,
                        &mut buffer,
                        &mut conn,
                        &mut result_buf,
                        &pool,
                    )
                        .await
                        .expect("packed single-line response should parse");

                    prop_assert_eq!(result_buf.to_vec(), expected.as_slice());
                    prop_assert_eq!(status.as_u16(), codes[idx]);
                }

                prop_assert_eq!(conn.leftover_len(), suffix.len());
                Ok(())
            })?;
        }

        #[test]
        fn prop_execute_pipeline_batch_reuses_connection_only_when_final_tail_is_empty(
            // RFC 3977 §3.5 allows pipelining, but the server still only sends
            // data in response to client commands and must process them in order.
            // So leftover bytes are valid only while there are more already-sent
            // commands remaining in the current borrow. After the final expected
            // response, any buffered bytes must retire the connection instead of
            // crossing a pool borrow boundary.
            codes in prop::collection::vec(prop_oneof![Just(223u16), Just(430u16), Just(500u16)], 1..5),
            extra_tail in prop::collection::vec(any::<u8>(), 0..24),
        ) {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let responses: Vec<Vec<u8>> = codes
                    .iter()
                    .enumerate()
                    .map(|(idx, code)| format!("{code} batch{idx}\r\n").into_bytes())
                    .collect();

                let (success, results, has_leftover, leftover_len) =
                    run_single_line_pipeline_batch(&responses, &extra_tail).await;

                prop_assert_eq!(results.len(), responses.len());
                for (idx, result) in results.into_iter().enumerate() {
                    match result {
                        Ok(completed) => {
                            prop_assert_eq!(
                                completed
                                    .context
                                    .response_metadata()
                                    .map(|response| response.status().as_u16()),
                                Some(codes[idx])
                            );
                            prop_assert_eq!(
                                completed.context.response_payload_eq(&responses[idx]),
                                Some(true)
                            );
                        }
                        other => prop_assert!(false, "expected success response, got {other:?}"),
                    }
                }

                let expect_reuse = extra_tail.is_empty();
                prop_assert_eq!(success, expect_reuse);
                prop_assert_eq!(has_leftover, !extra_tail.is_empty());
                prop_assert_eq!(leftover_len, extra_tail.len());
                Ok(())
            })?;
        }
    }

    // NOTE: This test is skipped because TCP read sizes are unpredictable in tests.
    // The bounds check in `buffer_response_for_request` prevents leftover from exceeding
    // MAX_LEFTOVER_BYTES (128KB), but triggering this in a test is difficult because:
    // 1. TCP reads are typically 8-16KB, not the full 724KB buffer size
    // 2. We can't control how TCP splits data across reads
    // 3. The check only triggers if a SINGLE read contains > 128KB of leftover data
    //
    // The check still provides defense-in-depth against protocol desync, even though
    // it's hard to test. In practice, normal leftover is < 8KB, so 128KB is generous.
    #[ignore = "flaky: TCP read sizes are unpredictable; leftover check requires single read > 128KB"]
    #[tokio::test]
    async fn test_leftover_exceeds_max_size() {
        use crate::constants::buffer::MAX_LEFTOVER_BYTES;

        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = crate::pool::ChunkedResponse::default();

        // Create a multiline response that accumulates in result_buf across chunks,
        // followed by oversized leftover. When the terminator is found, the remainder
        // in the current chunk should exceed MAX_LEFTOVER_BYTES.
        //
        // NOTE: This test is flaky because TCP read sizes are unpredictable. Even if
        // we send 138KB in one chunk, the read might only return 6-8KB at a time.

        let mut chunks = Vec::new();

        // Part 1: Response header + body start (no terminator yet)
        let mut part1 = Vec::new();
        part1.extend_from_slice(b"220 0 <id> article\r\n");
        part1.extend_from_slice(&vec![b'B'; 50000]); // 50KB of body
        chunks.push(part1);

        // Part 2: More body (no terminator yet)
        let mut part2 = Vec::new();
        part2.extend_from_slice(&vec![b'O'; 50000]); // Another 50KB
        chunks.push(part2);

        // Part 3: Terminator + oversized leftover
        let mut part3 = Vec::new();
        part3.extend_from_slice(&vec![b'D'; 10000]); // 10KB more body
        part3.extend_from_slice(b"\r\n.\r\n"); // Terminator
        part3.extend_from_slice(&vec![b'X'; MAX_LEFTOVER_BYTES + 1000]); // Oversized leftover
        chunks.push(part3);

        let mut conn = mock_backend_conn_chunked(chunks).await;

        let result = buffer_response_for_request(
            ARTICLE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await;

        // Should fail with bounds check error (if TCP delivers enough data in one read)
        assert!(
            result.is_err(),
            "Should error when leftover exceeds max size"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Leftover exceeds") || err.contains("protocol desync"),
            "Error should mention leftover size limit: {err}"
        );
    }

    #[tokio::test]
    async fn test_leftover_within_max_size() {
        use crate::constants::buffer::MAX_LEFTOVER_BYTES;

        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = crate::pool::ChunkedResponse::default();

        // Create a response where leftover is within bounds (< MAX_LEFTOVER_BYTES)
        let mut normal_data = Vec::new();
        normal_data.extend_from_slice(b"220 0 <msg@id> article\r\nBody\r\n.\r\n");
        // Add a reasonable amount of leftover data (well under the limit)
        normal_data.extend_from_slice(b"430 No such article\r\n");

        let mut conn = mock_backend_conn(&normal_data).await;

        let result = buffer_response_for_request(
            ARTICLE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await;

        // Should succeed
        assert!(
            result.is_ok(),
            "Should succeed when leftover is within max size"
        );
        assert!(
            conn.has_leftover(),
            "Should have leftover from second response"
        );
        assert!(
            conn.leftover_len() < MAX_LEFTOVER_BYTES,
            "Leftover should be under limit"
        );
    }
}
