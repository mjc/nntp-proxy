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
use tracing::{debug, warn};

use crate::metrics::MetricsCollector;
use crate::pool::{BufferPool, DeadpoolConnectionProvider};
use crate::protocol::ResponsePayloadLen;
use crate::router::backend_queue::{BackendQueue, PipelineError, QueuedContext};
#[cfg(test)]
use crate::session::backend::parse_backend_status;
use crate::types::BackendId;

/// Configuration for the pipeline worker
#[derive(Debug, Clone)]
pub struct PipelineWorkerConfig {
    /// Maximum number of commands in a single pipeline batch
    pub(crate) batch_size: usize,
    /// Backend identifier for logging/metrics
    pub(crate) backend_id: BackendId,
}

#[derive(Debug, Clone, Copy)]
struct PipelineReadResult {
    data_len: u64,
}

struct PipelineDepthGuard<'a> {
    queue: &'a BackendQueue,
    outstanding: usize,
}

impl<'a> PipelineDepthGuard<'a> {
    fn new(queue: &'a BackendQueue, outstanding: usize) -> Self {
        queue.mark_pipeline_sent(outstanding);
        Self { queue, outstanding }
    }

    fn complete_one(&mut self) {
        self.outstanding = self.outstanding.saturating_sub(1);
        self.queue.mark_pipeline_resolved(1);
    }
}

impl Drop for PipelineDepthGuard<'_> {
    fn drop(&mut self) {
        if self.outstanding > 0 {
            self.queue.mark_pipeline_resolved(self.outstanding);
        }
    }
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
pub async fn backend_pipeline_worker(
    config: PipelineWorkerConfig,
    queue: Arc<BackendQueue>,
    provider: Arc<DeadpoolConnectionProvider>,
    metrics: MetricsCollector,
    buffer_pool: BufferPool,
) {
    let backend_id = config.backend_id;
    debug!(
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
            queue.as_ref(),
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
        // Unconditional: !success means the current batch can no longer safely
        // reuse the backend connection, including streamed client disconnects.

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
async fn execute_pipeline_batch(
    backend_id: BackendId,
    queue: &BackendQueue,
    conn: &mut crate::stream::ConnectionStream,
    mut batch: Vec<QueuedContext>,
    metrics: &MetricsCollector,
    buffer_pool: &BufferPool,
    result_buf: &mut crate::pool::ChunkedResponse,
) -> (bool, Vec<QueuedContext>) {
    let batch_len = batch.len();

    // Phase 1: Write all commands
    for (i, req) in batch.iter().enumerate() {
        if let Err(e) = req.context.write_wire_to(conn).await {
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

    let mut pipeline_depth = PipelineDepthGuard::new(queue, batch_len);

    // Phase 2: Read responses in order (with shared buffer + connection-stashed leftovers)
    let mut buffer = buffer_pool.acquire();
    // result_buf is passed in as a parameter (hoisted to worker loop)

    batch.reverse();
    let mut response_index = 0usize;
    while let Some(mut req) = batch.pop() {
        req.record_queue_wait();
        let read_start = crate::pipeline_timing::now_if_enabled();
        let read_result = crate::session::response_buffer::read_response_into_context(
            &mut req.context,
            &mut buffer,
            conn,
            result_buf,
            buffer_pool,
            backend_id,
        )
        .await
        .map(|()| PipelineReadResult {
            data_len: req
                .context
                .response_payload_len()
                .map_or(0, ResponsePayloadLen::get) as u64,
        });

        match read_result {
            Ok(read_result) => {
                if let Some(read_start) = read_start {
                    crate::pipeline_timing::record_backend_read(
                        read_start.elapsed(),
                        read_result.data_len,
                    );
                }
                metrics.record_command(backend_id);
                metrics.record_backend_to_client_bytes_for(backend_id, read_result.data_len);
                metrics.record_client_to_backend_bytes_for(
                    backend_id,
                    req.context.request_wire_len().as_u64(),
                );

                req.complete_context();
                pipeline_depth.complete_one();
            }
            Err(crate::session::response_buffer::StreamingError::ClientDisconnect(io_err)) => {
                batch = fail_batch_on_client_disconnect(
                    backend_id,
                    req,
                    batch,
                    response_index,
                    batch_len,
                    conn.leftover_len(),
                    io_err,
                );
                pipeline_depth.complete_one();
                return (false, batch);
            }
            Err(e) => {
                warn!(
                    backend = ?backend_id,
                    response_index = response_index + 1,
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
                    completed: response_index + 1,
                    batch_len,
                };
                while let Some(remaining_req) = batch.pop() {
                    remaining_req.complete_error(error_msg);
                }

                // Connection broken — return false immediately
                return (false, batch);
            }
        }
        response_index += 1;
    }

    // All responses processed successfully. Leftover is valid only while there
    // are more already-sent responses remaining in this borrow. If bytes remain
    // after the final expected response, returning the connection to the pool
    // would leak stale backend data into the next borrower.
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

fn fail_batch_on_client_disconnect(
    backend_id: BackendId,
    current_req: QueuedContext,
    mut remaining_batch: Vec<QueuedContext>,
    response_index: usize,
    batch_len: usize,
    leftover_bytes: usize,
    io_err: std::io::Error,
) -> Vec<QueuedContext> {
    debug!(
        backend = ?backend_id,
        response_index = response_index + 1,
        batch_size = batch_len,
        command_verb = ?current_req.context.verb(),
        leftover_bytes,
        error = %io_err,
        "Pipeline worker client disconnect during pipelined response delivery; retiring backend connection and failing remaining batch"
    );

    current_req.complete_error(PipelineError::ClientDisconnect);
    let error_msg = PipelineError::ConnectionLost {
        completed: response_index + 1,
        batch_len,
    };
    while let Some(remaining_req) = remaining_batch.pop() {
        remaining_req.complete_error(error_msg);
    }
    remaining_batch
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
    crate::session::response_buffer::buffer_response_for_request(
        &request,
        buffer,
        conn,
        result_buf,
        pool,
        BackendId::from_index(0),
    )
    .await
    .map_err(crate::session::response_buffer::StreamingError::into_anyhow)
}

/// Send error responses to all requests in a batch.
///
/// Takes ownership of the Vec, completes each request, and returns the empty Vec
/// so the caller can reuse the allocation.
fn fail_batch(mut batch: Vec<QueuedContext>, error: PipelineError) -> Vec<QueuedContext> {
    while let Some(req) = batch.pop() {
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

    fn client_addr() -> crate::types::ClientAddress {
        crate::types::ClientAddress::new("127.0.0.1:8119".parse().expect("valid client addr"))
    }

    fn queued_context(
        command: &str,
    ) -> (
        QueuedContext,
        tokio::sync::oneshot::Receiver<PipelineResponse>,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (
            QueuedContext::new(request_context(command.as_bytes()), client_addr(), tx, None),
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
        let queue = BackendQueue::new(responses.len().max(1));

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
                    Ok(0) | Err(_) => break,
                    Ok(n) => received += n,
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
            &queue,
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
        let mut buffer = pool.acquire();
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
        let mut buffer = pool.acquire();
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
        let mut buffer = pool.acquire();
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
    async fn test_execute_pipeline_batch_keeps_large_transfer_430_buffered() {
        let pool = BufferPool::for_tests();
        let metrics = MetricsCollector::new(1);
        let backend_id = BackendId::from_index(0);
        let queue = BackendQueue::new(1);
        let response = b"430 No such article\r\n";
        let mut conn = mock_backend_conn(response).await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let batch = vec![QueuedContext::new(
            request_context(ARTICLE_REQUEST),
            client_addr(),
            tx,
            None,
        )];
        let mut result_buf = crate::pool::ChunkedResponse::default();

        let (success, batch) = execute_pipeline_batch(
            backend_id,
            &queue,
            &mut conn,
            batch,
            &metrics,
            &pool,
            &mut result_buf,
        )
        .await;

        assert!(success);
        assert!(batch.is_empty());

        let completed = rx.await.unwrap().unwrap();
        assert_eq!(
            completed
                .context
                .response_metadata()
                .expect("430 is recorded on completed context")
                .status()
                .as_u16(),
            430
        );
        assert_eq!(
            completed
                .context
                .response_payload()
                .expect("430 stays buffered for retry logic")
                .to_vec(),
            response
        );
    }

    #[tokio::test]
    async fn test_execute_pipeline_batch_keeps_buffered_large_transfer_payload() {
        let pool = BufferPool::for_tests();
        let metrics = MetricsCollector::new(1);
        let backend_id = BackendId::from_index(0);
        let queue = BackendQueue::new(1);
        let response = b"220 0 <msg@id> article\r\nSubject: test\r\n\r\nBody line\r\n.\r\n";
        let mut conn = mock_backend_conn(response).await;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let batch = vec![QueuedContext::new(
            request_context(ARTICLE_REQUEST),
            client_addr(),
            tx,
            None,
        )];
        let mut result_buf = crate::pool::ChunkedResponse::default();

        let (success, batch) = execute_pipeline_batch(
            backend_id,
            &queue,
            &mut conn,
            batch,
            &metrics,
            &pool,
            &mut result_buf,
        )
        .await;

        assert!(success);
        assert!(batch.is_empty());

        let completed = rx.await.unwrap().unwrap();
        assert_eq!(
            completed
                .context
                .response_metadata()
                .expect("buffered large transfer records metadata")
                .status()
                .as_u16(),
            220
        );
        assert_eq!(
            completed
                .context
                .response_payload()
                .expect("buffered large transfer keeps payload")
                .to_vec(),
            response
        );
    }

    #[tokio::test]
    async fn test_execute_pipeline_batch_tracks_live_pipeline_depth_until_responses_finish() {
        use tokio::io::AsyncReadExt;

        let pool = BufferPool::for_tests();
        let metrics = MetricsCollector::new(2);
        let backend_id = BackendId::from_index(0);
        let queue = Arc::new(BackendQueue::new(2));

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (commands_read_tx, commands_read_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let mut received = 0usize;
            let expected = b"STAT <a@b>\r\n".len() + b"STAT <c@d>\r\n".len();
            while received < expected {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => received += n,
                }
            }

            let _ = commands_read_tx.send(());
            let _ = release_rx.await;
            stream
                .write_all(b"223 0 <a@b> status\r\n223 0 <c@d> status\r\n")
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
        let queue_for_task = queue.clone();

        let task = tokio::spawn(async move {
            execute_pipeline_batch(
                backend_id,
                queue_for_task.as_ref(),
                &mut conn,
                batch,
                &metrics,
                &pool,
                &mut result_buf,
            )
            .await
        });

        commands_read_rx.await.unwrap();
        assert_eq!(queue.pipeline_depth(), 2);

        release_tx.send(()).unwrap();
        let (success, batch) = task.await.unwrap();
        assert!(success);
        assert!(batch.is_empty());
        assert_eq!(queue.pipeline_depth(), 0);
        assert_eq!(expect_success_status(rx1.await.unwrap()), 223);
        assert_eq!(expect_success_status(rx2.await.unwrap()), 223);
    }

    #[tokio::test]
    async fn test_buffer_response_for_request_multiline_across_chunks() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire();
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
        let mut buffer = pool.acquire();
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
    async fn test_buffer_response_for_request_multiline_terminal_chunk_stashes_leftover() {
        let pool = BufferPool::new(crate::types::BufferSize::try_new(32).unwrap(), 4)
            .with_capture_pool(1024, 1);
        let mut buffer = pool.acquire();
        let mut result_buf = crate::pool::ChunkedResponse::default();

        let first =
            b"220 0 <a@b> article\r\nSubject: split\r\n\r\nBody line one\r\nBody line two\r\n.\r\n";
        let packed = [first.as_slice(), b"430 No such article\r\n"].concat();
        let mut conn = mock_backend_conn(&packed).await;

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
        assert_eq!(result_buf.to_vec(), first);
        assert!(conn.has_leftover(), "next response should remain stashed");

        let status2 = buffer_response_for_request(
            ARTICLE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await
        .expect("should parse leftover response");

        assert_eq!(status2.as_u16(), 430);
        assert_eq!(result_buf.to_vec(), b"430 No such article\r\n");
    }

    #[tokio::test]
    async fn test_buffer_response_for_request_prefers_spanning_terminator_before_later_response() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire();
        let mut result_buf = crate::pool::ChunkedResponse::default();

        let first = b"220 0 <a@b> article\r\nBody one\r\n.\r\n";
        let second = b"220 0 <c@d> article\r\nBody two\r\n.\r\n";
        let chunk1 = b"220 0 <a@b> article\r\nBody one\r\n.".to_vec();
        let chunk2 = [b"\r\n".as_slice(), second.as_slice()].concat();
        let mut conn = mock_backend_conn_chunked(vec![chunk1, chunk2]).await;

        let status1 = buffer_response_for_request(
            ARTICLE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await
        .expect("should stop at the spanning terminator");

        assert_eq!(status1.as_u16(), 220);
        assert_eq!(result_buf.to_vec(), first);
        assert!(conn.has_leftover(), "next response should remain stashed");

        let status2 = buffer_response_for_request(
            ARTICLE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await
        .expect("should consume the stashed second response");

        assert_eq!(status2.as_u16(), 220);
        assert_eq!(result_buf.to_vec(), second);
    }

    #[tokio::test]
    async fn test_buffer_response_for_request_uses_request_context_for_same_status_framing() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire();
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
        let mut buffer = pool.acquire();
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
        let mut buffer = pool.acquire();
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
        let mut buffer = pool.acquire();
        let mut result_buf = crate::pool::ChunkedResponse::default();

        // Backend will provide the rest of the response
        let mut conn = mock_backend_conn(b"0 No such article\r\n").await;
        // Leftover is only 2 bytes — too short to validate, triggers H5 additional read
        conn.stash_leftover(b"43");

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
        let mut buffer = pool.acquire();
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
        let queue = BackendQueue::new(1);

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
            &queue,
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
        let queue = BackendQueue::new(2);

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
            &queue,
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
        let queue = BackendQueue::new(3);

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
            &queue,
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
    async fn test_pipeline_batch_pairs_four_body_responses_to_matching_requests_on_single_connection()
     {
        use tokio::io::AsyncReadExt;

        let pool = BufferPool::for_tests();
        let metrics = MetricsCollector::new(4);
        let backend_id = BackendId::from_index(0);
        let queue = BackendQueue::new(4);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (commands_tx, commands_rx) = tokio::sync::oneshot::channel();

        let expected_wire = concat!(
            "BODY <body-1@example>\r\n",
            "BODY <body-2@example>\r\n",
            "BODY <body-3@example>\r\n",
            "BODY <body-4@example>\r\n"
        )
        .as_bytes()
        .to_vec();
        let responses = concat!(
            "222 1 <body-1@example>\r\nbody-1-line\r\n.\r\n",
            "222 2 <body-2@example>\r\nbody-2-line\r\n.\r\n",
            "222 3 <body-3@example>\r\nbody-3-line\r\n.\r\n",
            "222 4 <body-4@example>\r\nbody-4-line\r\n.\r\n"
        )
        .as_bytes()
        .to_vec();
        let expected_wire_for_backend = expected_wire.clone();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let mut received = Vec::new();
            while received.len() < expected_wire_for_backend.len() {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => received.extend_from_slice(&buf[..n]),
                }
            }
            let _ = commands_tx.send(received);
            stream.write_all(&responses).await.unwrap();
            stream.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut conn = ConnectionStream::plain(stream);

        let (req1, rx1) = queued_context("BODY <body-1@example>\r\n");
        let (req2, rx2) = queued_context("BODY <body-2@example>\r\n");
        let (req3, rx3) = queued_context("BODY <body-3@example>\r\n");
        let (req4, rx4) = queued_context("BODY <body-4@example>\r\n");
        let batch = vec![req1, req2, req3, req4];

        let mut result_buf = crate::pool::ChunkedResponse::default();

        let (success, _batch) = execute_pipeline_batch(
            backend_id,
            &queue,
            &mut conn,
            batch,
            &metrics,
            &pool,
            &mut result_buf,
        )
        .await;
        assert_eq!(commands_rx.await.unwrap(), expected_wire);
        let response1 = rx1.await.unwrap();
        let response2 = rx2.await.unwrap();
        let response3 = rx3.await.unwrap();
        let response4 = rx4.await.unwrap();

        let summarize = |response: &PipelineResponse| match response {
            Ok(completed) => (
                completed.context.message_id().map(str::to_owned),
                completed.context.backend_id(),
                completed
                    .context
                    .response_metadata()
                    .map(|metadata| metadata.status().as_u16()),
                completed
                    .context
                    .response_payload()
                    .map(|payload| payload.to_vec()),
                None,
            ),
            Err(error) => (None, None, None, None, Some(*error)),
        };

        assert!(
            success,
            "expected single-connection 4x BODY pipeline to succeed; leftover={} summaries={:?}",
            conn.leftover_len(),
            [
                summarize(&response1),
                summarize(&response2),
                summarize(&response3),
                summarize(&response4),
            ]
        );
        assert!(!conn.has_leftover());

        assert_eq!(
            [
                response1.expect("BODY request should complete"),
                response2.expect("BODY request should complete"),
                response3.expect("BODY request should complete"),
                response4.expect("BODY request should complete"),
            ]
            .map(|completed| (
                completed.context.message_id().map(str::to_owned),
                completed.context.backend_id(),
                completed
                    .context
                    .response_metadata()
                    .map(|metadata| metadata.status().as_u16()),
                completed
                    .context
                    .response_payload()
                    .expect("buffered BODY response keeps payload")
                    .to_vec(),
            )),
            [
                (
                    Some("<body-1@example>".to_owned()),
                    Some(backend_id),
                    Some(222),
                    b"222 1 <body-1@example>\r\nbody-1-line\r\n.\r\n".to_vec(),
                ),
                (
                    Some("<body-2@example>".to_owned()),
                    Some(backend_id),
                    Some(222),
                    b"222 2 <body-2@example>\r\nbody-2-line\r\n.\r\n".to_vec(),
                ),
                (
                    Some("<body-3@example>".to_owned()),
                    Some(backend_id),
                    Some(222),
                    b"222 3 <body-3@example>\r\nbody-3-line\r\n.\r\n".to_vec(),
                ),
                (
                    Some("<body-4@example>".to_owned()),
                    Some(backend_id),
                    Some(222),
                    b"222 4 <body-4@example>\r\nbody-4-line\r\n.\r\n".to_vec(),
                ),
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
            let queue = BackendQueue::new(2);
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
                &queue,
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
        let queue = BackendQueue::new(2);

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
            &queue,
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
    async fn test_pipeline_batch_pairs_spanning_multiline_terminator_before_next_status() {
        use tokio::io::AsyncReadExt;

        let pool = BufferPool::for_tests();
        let metrics = MetricsCollector::new(2);
        let backend_id = BackendId::from_index(0);
        let queue = BackendQueue::new(2);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let mut received = 0usize;
            let expected = b"BODY <a@b>\r\n".len() + b"STAT <c@d>\r\n".len();
            while received < expected {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => received += n,
                }
            }

            stream
                .write_all(b"222 0 <a@b>\r\nBody line\r\n.")
                .await
                .unwrap();
            stream
                .write_all(b"\r\n223 0 <c@d> status\r\n")
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut conn = ConnectionStream::plain(stream);
        let (req1, rx1) = queued_context("BODY <a@b>\r\n");
        let (req2, rx2) = queued_context("STAT <c@d>\r\n");
        let batch = vec![req1, req2];
        let mut result_buf = crate::pool::ChunkedResponse::default();

        let (success, batch) = execute_pipeline_batch(
            backend_id,
            &queue,
            &mut conn,
            batch,
            &metrics,
            &pool,
            &mut result_buf,
        )
        .await;

        assert!(success);
        assert!(batch.is_empty());
        assert!(!conn.has_leftover());

        let completed_body = rx1.await.unwrap().unwrap();
        assert_eq!(
            completed_body
                .context
                .response_payload()
                .expect("BODY response stays buffered")
                .to_vec(),
            b"222 0 <a@b>\r\nBody line\r\n.\r\n"
        );

        assert_eq!(expect_success_status(rx2.await.unwrap()), 223);
    }

    #[tokio::test]
    async fn test_pipeline_batch_multiline_eof_fails_current_and_remaining_requests() {
        use tokio::io::AsyncReadExt;

        let pool = BufferPool::for_tests();
        let metrics = MetricsCollector::new(2);
        let backend_id = BackendId::from_index(0);
        let queue = BackendQueue::new(2);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let mut received = 0usize;
            let expected = b"BODY <a@b>\r\n".len() + b"STAT <c@d>\r\n".len();
            while received < expected {
                match stream.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => received += n,
                }
            }

            stream
                .write_all(b"222 0 <a@b>\r\nBody line\r\n")
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut conn = ConnectionStream::plain(stream);
        let (req1, rx1) = queued_context("BODY <a@b>\r\n");
        let (req2, rx2) = queued_context("STAT <c@d>\r\n");
        let batch = vec![req1, req2];
        let mut result_buf = crate::pool::ChunkedResponse::default();

        let (success, _batch) = execute_pipeline_batch(
            backend_id,
            &queue,
            &mut conn,
            batch,
            &metrics,
            &pool,
            &mut result_buf,
        )
        .await;

        assert!(!success, "partial multiline EOF must retire the connection");
        assert!(!conn.has_leftover());

        assert!(matches!(rx1.await.unwrap(), Err(PipelineError::ReadFailed)));
        assert!(matches!(
            rx2.await.unwrap(),
            Err(PipelineError::ConnectionLost {
                completed: 1,
                batch_len: 2
            })
        ));
    }

    #[tokio::test]
    async fn test_pipeline_batch_retires_connection_with_leftover_after_final_response() {
        use tokio::io::AsyncReadExt;

        let pool = BufferPool::for_tests();
        let metrics = MetricsCollector::new(2);
        let backend_id = BackendId::from_index(0);
        let queue = BackendQueue::new(2);

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
            &queue,
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

    #[tokio::test]
    async fn test_pipeline_batch_retires_connection_with_garbage_after_final_multiline_response() {
        use tokio::io::AsyncReadExt;

        let pool = BufferPool::for_tests();
        let metrics = MetricsCollector::new(1);
        let backend_id = BackendId::from_index(0);
        let queue = BackendQueue::new(1);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf).await;
            stream
                .write_all(b"222 0 <a@b>\r\nBody line\r\n.\r\n223 0 <c@d> status\r\n")
                .await
                .unwrap();
            stream.shutdown().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut conn = ConnectionStream::plain(stream);

        let (req, rx) = queued_context("BODY <a@b>\r\n");
        let mut result_buf = crate::pool::ChunkedResponse::default();

        let (success, batch) = execute_pipeline_batch(
            backend_id,
            &queue,
            &mut conn,
            vec![req],
            &metrics,
            &pool,
            &mut result_buf,
        )
        .await;

        assert!(
            !success,
            "extra packed bytes after the final multiline response must retire the connection"
        );
        assert!(batch.is_empty());
        assert!(conn.has_leftover());
        assert_eq!(expect_success_status(rx.await.unwrap()), 222);
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
        for response in [rx1.blocking_recv().unwrap(), rx2.blocking_recv().unwrap()] {
            assert!(matches!(response, Err(e) if e == error));
        }
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
                let mut buffer = pool.acquire();
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

    #[tokio::test]
    async fn test_packed_leftover_is_buffered_for_next_pipelined_response() {
        let pool = BufferPool::new(
            crate::types::BufferSize::try_new(3 * 1024 * 1024).expect("valid test buffer size"),
            2,
        );
        let mut buffer = pool.acquire();
        let mut result_buf = crate::pool::ChunkedResponse::default();

        let mut response = Vec::new();
        response.extend_from_slice(b"220 0 <first@example> article\r\nBody\r\n.\r\n");
        let mut second_response = Vec::new();
        second_response.extend_from_slice(b"220 0 <second@example> article\r\n");
        second_response.extend_from_slice(&vec![b'x'; 2 * 1024 * 1024]);
        response.extend_from_slice(&second_response);

        let mut conn = mock_backend_conn(&response).await;

        let result = buffer_response_for_request(
            ARTICLE_REQUEST,
            &mut buffer,
            &mut conn,
            &mut result_buf,
            &pool,
        )
        .await;

        assert!(result.is_ok(), "leftovers are valid in hot pipelines");
        assert!(conn.has_leftover(), "second response should be buffered");
        assert!(conn.leftover_len() <= second_response.len());
    }

    #[tokio::test]
    async fn test_leftover_single_line_response_is_buffered() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire();
        let mut result_buf = crate::pool::ChunkedResponse::default();

        let mut normal_data = Vec::new();
        normal_data.extend_from_slice(b"220 0 <msg@id> article\r\nBody\r\n.\r\n");
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

        assert!(
            result.is_ok(),
            "should succeed when the next response starts in the same read"
        );
        assert!(
            conn.has_leftover(),
            "Should have leftover from second response"
        );
        assert_eq!(conn.leftover_len(), b"430 No such article\r\n".len());
    }

    #[tokio::test]
    async fn client_disconnect_fails_remaining_batch_and_retires_connection() {
        let (current_req, current_rx) = queued_context("BODY <body-1@example>\r\n");
        let (remaining_req_1, remaining_rx_1) = queued_context("BODY <body-2@example>\r\n");
        let (remaining_req_2, remaining_rx_2) = queued_context("BODY <body-3@example>\r\n");

        let remaining = fail_batch_on_client_disconnect(
            BackendId::from_index(0),
            current_req,
            vec![remaining_req_1, remaining_req_2],
            0,
            3,
            17,
            std::io::Error::from(std::io::ErrorKind::BrokenPipe),
        );

        assert!(
            remaining.is_empty(),
            "batch should be fully drained on disconnect"
        );
        assert!(
            matches!(
                current_rx.await.unwrap(),
                Err(PipelineError::ClientDisconnect)
            ),
            "current streamed request should surface the disconnect"
        );
        assert!(
            matches!(
                remaining_rx_1.await.unwrap(),
                Err(PipelineError::ConnectionLost {
                    completed: 1,
                    batch_len: 3,
                })
            ),
            "later queued requests should fail as connection-lost so callers do not keep using this backend state"
        );
        assert!(
            matches!(
                remaining_rx_2.await.unwrap(),
                Err(PipelineError::ConnectionLost {
                    completed: 1,
                    batch_len: 3,
                })
            ),
            "all later queued requests should fail after the disconnect"
        );
    }
}
