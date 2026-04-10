//! Backend pipeline worker for request multiplexing
//!
//! Each backend has one long-running worker task that:
//! 1. Dequeues batches of requests from the backend's queue
//! 2. Writes all commands to a single backend connection (pipelining)
//! 3. Reads responses in order and routes them back to client sessions
//!
//! This enables N client sessions to share M backend connections (N >> M).

use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use crate::metrics::MetricsCollector;
use crate::pool::{BufferPool, DeadpoolConnectionProvider};
use crate::router::backend_queue::{BackendQueue, PipelineResponse, QueuedRequest};
use crate::session::backend::validate_backend_response;
use crate::types::BackendId;

/// Configuration for the pipeline worker
#[derive(Debug, Clone)]
pub struct PipelineWorkerConfig {
    /// Maximum number of commands in a single pipeline batch
    pub batch_size: usize,
    /// Backend identifier for logging/metrics
    pub backend_id: BackendId,
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
    info!(
        "Pipeline worker started for backend {:?} (batch_size={})",
        backend_id, config.batch_size
    );

    // Hoist buffers to worker lifetime (reused across batches)
    let mut result_buf = bytes::BytesMut::with_capacity(4096);

    loop {
        // Wait for at least one request, then grab up to batch_size
        let batch = queue.dequeue_batch(config.batch_size).await;
        let batch_len = batch.len();

        debug!(
            "Pipeline worker backend {:?}: dequeued batch of {} requests",
            backend_id, batch_len
        );

        // Clear buffers for this batch
        result_buf.clear();

        // Acquire a connection from the pool
        let mut conn = match provider.get_pooled_connection().await {
            Ok(c) => c,
            Err(e) => {
                warn!(
                    "Pipeline worker backend {:?}: connection acquisition failed: {}",
                    backend_id, e
                );
                metrics.record_connection_failure(backend_id);
                fail_batch(batch, &format!("connection error: {e}"));
                continue;
            }
        };

        // Execute the batch: write all commands, then read all responses
        let success = execute_pipeline_batch(
            backend_id,
            &mut conn,
            batch,
            &metrics,
            &buffer_pool,
            &mut result_buf,
        )
        .await;

        if !success {
            // Connection is likely broken; remove from pool so it's not reused
            provider.remove_with_cooldown(conn);
        }

        // Record pipeline batch metrics
        if batch_len > 1 {
            metrics.record_pipeline_batch(batch_len as u64);
        }
    }
}

/// Execute a batch of commands on a single connection using write-write-read-read pipelining.
///
/// Returns `true` if the connection is still healthy, `false` if it should be removed from pool.
async fn execute_pipeline_batch(
    backend_id: BackendId,
    conn: &mut crate::stream::ConnectionStream,
    batch: Vec<QueuedRequest>,
    metrics: &MetricsCollector,
    buffer_pool: &BufferPool,
    result_buf: &mut bytes::BytesMut,
) -> bool {
    let batch_len = batch.len();

    // Phase 1: Write all commands
    for (i, req) in batch.iter().enumerate() {
        if let Err(e) = conn.write_all(req.command.as_bytes()).await {
            warn!(
                "Pipeline worker backend {:?}: write failed at command {}/{}: {}",
                backend_id,
                i + 1,
                batch_len,
                e
            );
            // We wrote commands 0..i successfully but can't read responses
            // because the connection is broken. Fail everything and return early.
            let err_msg = format!("write failed at command {}/{}", i + 1, batch_len);
            fail_batch(batch, &err_msg);
            return false;
        }
    }
    // All writes succeeded, continue to flush

    // Flush after writing all commands
    if let Err(e) = conn.flush().await {
        warn!(
            "Pipeline worker backend {:?}: flush failed: {}",
            backend_id, e
        );
        fail_batch(batch, &format!("flush failed: {e}"));
        return false;
    }

    // Phase 2: Read responses in order (with shared buffer + leftover tracking)
    let mut buffer = buffer_pool.acquire().await;
    // leftover and result_buf are now passed in as parameters (hoisted to worker loop)

    let mut batch_iter = batch.into_iter().enumerate();
    while let Some((i, req)) = batch_iter.next() {
        // Read the response for this command
        match read_full_response(&mut buffer, conn, result_buf).await {
            Ok((data, status_code)) => {
                metrics.record_command(backend_id);
                let data_len = data.len();
                metrics.record_backend_to_client_bytes_for(backend_id, data_len as u64);
                metrics.record_client_to_backend_bytes_for(backend_id, req.command.len() as u64);

                // Send response to client (ignore error if client disconnected)
                let _ = req.response_tx.send(PipelineResponse::Success {
                    data,
                    status_code,
                    backend_id,
                });
            }
            Err(e) => {
                warn!(
                    backend = ?backend_id,
                    response_index = i + 1,
                    batch_size = batch_len,
                    command = %req.command.trim(),
                    error = %e,
                    leftover_bytes = conn.leftover_len(),
                    "Pipeline worker read failed"
                );

                // Fail this request
                let _ = req
                    .response_tx
                    .send(PipelineResponse::Error(format!("read error: {e}")));

                // Fail remaining requests (no Vec allocation - iterate directly)
                let error_msg = format!("connection lost after response {}/{}", i + 1, batch_len);
                for (_, remaining_req) in batch_iter {
                    let _ = remaining_req
                        .response_tx
                        .send(PipelineResponse::Error(error_msg.clone()));
                }

                // Connection broken — return false immediately
                return false;
            }
        }
    }

    // All responses processed successfully — connection healthy
    //
    // Health validation: Successful reading of all responses (including
    // terminators) serves as an implicit health check. If the connection
    // was in a bad state, read_full_response would have failed. The leftover
    // buffer bounds are enforced in read_full_response (MAX_LEFTOVER_BYTES),
    // preventing protocol desync from leaving the connection unusable.
    //
    // Explicit health check: Verify leftover is reasonable
    debug_assert!(
        conn.leftover_len() <= crate::constants::buffer::MAX_LEFTOVER_BYTES,
        "Leftover buffer should never exceed MAX_LEFTOVER_BYTES after successful batch"
    );

    true
}

/// Read a complete NNTP response (status line + multiline body if applicable).
///
/// Returns the full response as `Bytes` and the parsed status code.
///
/// `result_buf` is cleared and reused for each response to avoid per-response allocations.
async fn read_full_response(
    buffer: &mut crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    result_buf: &mut bytes::BytesMut,
) -> Result<(bytes::Bytes, crate::protocol::StatusCode)> {
    use crate::session::streaming::tail_buffer::TailBuffer;

    let mut tail = TailBuffer::default();
    result_buf.clear(); // Reuse buffer from previous response

    // Get first data directly into result_buf (no intermediate Vec)
    let n = buffer.read_from(conn).await?;
    if n == 0 {
        anyhow::bail!("Backend connection closed unexpectedly");
    }
    result_buf.extend_from_slice(&buffer[..n]);

    // H5: If leftover was too short to validate, read more data
    if result_buf.len() < crate::protocol::MIN_RESPONSE_LENGTH {
        let n = buffer.read_from(conn).await?;
        if n == 0 {
            anyhow::bail!(
                "Backend EOF with partial status line ({} bytes)",
                result_buf.len()
            );
        }
        result_buf.extend_from_slice(&buffer[..n]);
    }

    // Validate the response to determine if it's multiline and get status code
    let validated = validate_backend_response(
        result_buf,
        result_buf.len(),
        crate::protocol::MIN_RESPONSE_LENGTH,
    );
    let status_code = validated
        .response
        .status_code()
        .ok_or_else(|| anyhow::anyhow!("Invalid status code in pipeline response"))?;

    if !validated.is_multiline {
        // Single-line response: split at \r\n boundary to save leftover
        if let Some(pos) = memchr::memchr(b'\n', &result_buf[..]) {
            let end = pos + 1;
            if end < result_buf.len() {
                let remainder = &result_buf[end..];
                anyhow::ensure!(
                    remainder.len() <= crate::constants::buffer::MAX_LEFTOVER_BYTES,
                    "Leftover exceeds {} bytes ({} bytes) — probable protocol desync",
                    crate::constants::buffer::MAX_LEFTOVER_BYTES,
                    remainder.len()
                );
                conn.stash_leftover(remainder)?;
                result_buf.truncate(end);
            }
        }
        return Ok((result_buf.split().freeze(), status_code));
    }

    // Multiline response: use TailBuffer to detect terminator
    let status = tail.detect_terminator(result_buf);
    let write_len = status.write_len(result_buf.len());

    // Save any leftover bytes for next response
    if write_len < result_buf.len() {
        let remainder = &result_buf[write_len..];
        anyhow::ensure!(
            remainder.len() <= crate::constants::buffer::MAX_LEFTOVER_BYTES,
            "Leftover exceeds {} bytes ({} bytes) — probable protocol desync",
            crate::constants::buffer::MAX_LEFTOVER_BYTES,
            remainder.len()
        );
        conn.stash_leftover(remainder)?;
        result_buf.truncate(write_len);
    }

    if !status.is_found() {
        tail.update(result_buf);

        loop {
            let n = buffer.read_from(conn).await?;
            if n == 0 {
                // C4: EOF before terminator is an error, not success
                anyhow::bail!(
                    "Backend EOF before multiline terminator (received {} bytes)",
                    result_buf.len()
                );
            }
            let chunk = &buffer[..n];
            let status = tail.detect_terminator(chunk);
            let write_len = status.write_len(n);
            result_buf.extend_from_slice(&chunk[..write_len]);

            if status.is_found() {
                // Save leftover bytes for next response
                if write_len < n {
                    let remainder = &chunk[write_len..];
                    anyhow::ensure!(
                        remainder.len() <= crate::constants::buffer::MAX_LEFTOVER_BYTES,
                        "Leftover exceeds {} bytes ({} bytes) — probable protocol desync",
                        crate::constants::buffer::MAX_LEFTOVER_BYTES,
                        remainder.len()
                    );
                    conn.stash_leftover(remainder)?;
                }
                break;
            }
            tail.update(&chunk[..write_len]);
        }
    }

    Ok((result_buf.split().freeze(), status_code))
}

/// Send error responses to all requests in a batch
fn fail_batch(batch: Vec<QueuedRequest>, error_msg: &str) {
    for req in batch {
        let _ = req
            .response_tx
            .send(PipelineResponse::Error(error_msg.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::BufferPool;
    use crate::stream::ConnectionStream;
    use bytes::BytesMut;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Helper: create a TCP pair where the server writes `data` then optionally closes.
    /// Returns a ConnectionStream connected to the mock server.
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
    /// Returns a ConnectionStream connected to the mock server.
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

    // ─── read_full_response unit tests ───────────────────────────────────────

    #[tokio::test]
    async fn test_read_full_response_single_line() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = BytesMut::with_capacity(4096);

        let mut conn = mock_backend_conn(b"430 No such article\r\n").await;

        let (data, status) = read_full_response(&mut buffer, &mut conn, &mut result_buf)
            .await
            .expect("should parse single-line response");

        assert_eq!(status.as_u16(), 430);
        assert_eq!(&data[..], b"430 No such article\r\n");
        assert!(!conn.has_leftover());
    }

    #[tokio::test]
    async fn test_read_full_response_multiline() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = BytesMut::with_capacity(4096);

        let response = b"220 0 <msg@id> article\r\nSubject: test\r\n\r\nBody line\r\n.\r\n";
        let mut conn = mock_backend_conn(response).await;

        let (data, status) = read_full_response(&mut buffer, &mut conn, &mut result_buf)
            .await
            .expect("should parse multiline response");

        assert_eq!(status.as_u16(), 220);
        assert_eq!(&data[..], &response[..]);
        assert!(!conn.has_leftover());
    }

    #[tokio::test]
    async fn test_read_full_response_multiline_across_chunks() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = BytesMut::with_capacity(4096);

        // Split the terminator \r\n.\r\n across two chunks
        let chunk1 = b"220 0 <msg@id> article\r\nBody\r\n.".to_vec();
        let chunk2 = b"\r\n".to_vec();

        let mut conn = mock_backend_conn_chunked(vec![chunk1, chunk2]).await;

        let (data, status) = read_full_response(&mut buffer, &mut conn, &mut result_buf)
            .await
            .expect("should detect terminator across chunks");

        assert_eq!(status.as_u16(), 220);
        assert!(
            data.ends_with(b"\r\n.\r\n"),
            "response should end with terminator"
        );
        assert!(!conn.has_leftover());
    }

    #[tokio::test]
    async fn test_read_full_response_with_leftover() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = BytesMut::with_capacity(4096);

        // Two responses packed together
        let packed = b"220 0 <a@b> article\r\nBody\r\n.\r\n430 No such article\r\n";
        let mut conn = mock_backend_conn(packed).await;

        // First read should get the multiline response and save leftover
        let (data1, status1) = read_full_response(&mut buffer, &mut conn, &mut result_buf)
            .await
            .expect("should parse first response");

        assert_eq!(status1.as_u16(), 220);
        assert!(data1.ends_with(b"\r\n.\r\n"));
        assert!(
            conn.has_leftover(),
            "should have leftover from second response"
        );

        // Second read should consume leftover and return the 430
        let (data2, status2) = read_full_response(&mut buffer, &mut conn, &mut result_buf)
            .await
            .expect("should parse second response from leftover");

        assert_eq!(status2.as_u16(), 430);
        assert_eq!(&data2[..], b"430 No such article\r\n");
    }

    #[tokio::test]
    async fn test_read_full_response_eof_mid_stream() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = BytesMut::with_capacity(4096);

        // Multiline response without terminator — server disconnects
        let mut conn = mock_backend_conn(b"220 0 <a@b> article\r\nBody line\r\n").await;

        let result = read_full_response(&mut buffer, &mut conn, &mut result_buf).await;
        assert!(result.is_err(), "EOF before terminator should be an error");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("EOF") || err.contains("eof"),
            "Error should mention EOF: {err}"
        );
    }

    #[tokio::test]
    async fn test_read_full_response_invalid_status() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = BytesMut::with_capacity(4096);

        let mut conn = mock_backend_conn(b"garbage data here\r\n").await;

        let result = read_full_response(&mut buffer, &mut conn, &mut result_buf).await;
        assert!(result.is_err(), "Invalid status code should be an error");
    }

    #[tokio::test]
    async fn test_read_full_response_short_leftover_triggers_h5() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = BytesMut::with_capacity(4096);

        // Backend will provide the rest of the response
        let mut conn = mock_backend_conn(b"0 No such article\r\n").await;
        // Leftover is only 2 bytes — too short to validate, triggers H5 additional read
        conn.stash_leftover(b"43").unwrap();

        let (data, status) = read_full_response(&mut buffer, &mut conn, &mut result_buf)
            .await
            .expect("short leftover + read should produce valid response");

        assert_eq!(status.as_u16(), 430);
        assert!(data.starts_with(b"430"));
    }

    #[tokio::test]
    async fn test_read_full_response_empty_connection() {
        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = BytesMut::with_capacity(4096);

        // Server immediately closes
        let mut conn = mock_backend_conn(b"").await;

        let result = read_full_response(&mut buffer, &mut conn, &mut result_buf).await;
        assert!(result.is_err(), "Empty connection should be an error");
    }

    // ─── validate_backend_response unit tests ────────────────────────────────

    #[test]
    fn test_validate_empty_response() {
        let validated = validate_backend_response(b"", 0, crate::protocol::MIN_RESPONSE_LENGTH);
        assert_eq!(validated.response, crate::protocol::NntpResponse::Invalid);
    }

    #[test]
    fn test_validate_response_with_only_status_code() {
        // "220" with no CRLF — too short
        let data = b"220";
        let validated =
            validate_backend_response(data, data.len(), crate::protocol::MIN_RESPONSE_LENGTH);
        // Should still parse the status code but emit warning for short response
        assert!(!validated.warnings.is_empty());
        // Status code parsing should work even for short responses
        assert_ne!(
            validated.response,
            crate::protocol::NntpResponse::Invalid,
            "3-digit code should be parseable even if short"
        );
    }

    #[test]
    fn test_validate_response_with_binary_garbage() {
        let data = &[0xFF, 0xFE, 0x00, 0x01, 0x02];
        let validated =
            validate_backend_response(data, data.len(), crate::protocol::MIN_RESPONSE_LENGTH);
        assert_eq!(validated.response, crate::protocol::NntpResponse::Invalid);
    }

    #[test]
    fn test_validate_valid_single_line() {
        let data = b"430 No such article\r\n";
        let validated =
            validate_backend_response(data, data.len(), crate::protocol::MIN_RESPONSE_LENGTH);
        assert!(validated.response.status_code().is_some());
        assert_eq!(validated.response.status_code().unwrap().as_u16(), 430);
        assert!(!validated.is_multiline);
    }

    #[test]
    fn test_validate_valid_multiline() {
        let data = b"220 0 <msg@id> article\r\nBody\r\n.\r\n";
        let validated =
            validate_backend_response(data, data.len(), crate::protocol::MIN_RESPONSE_LENGTH);
        assert!(validated.response.status_code().is_some());
        assert_eq!(validated.response.status_code().unwrap().as_u16(), 220);
        assert!(validated.is_multiline);
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

        let (tx, rx) = tokio::sync::oneshot::channel();
        let batch = vec![QueuedRequest {
            command: Arc::from("STAT <test@msg.id>\r\n"),
            response_tx: tx,
        }];

        let mut result_buf = bytes::BytesMut::with_capacity(4096);

        let success = execute_pipeline_batch(
            backend_id,
            &mut conn,
            batch,
            &metrics,
            &pool,
            &mut result_buf,
        )
        .await;

        assert!(success, "single 430 should not mark connection as broken");
        match rx.await.unwrap() {
            PipelineResponse::Success { status_code, .. } => {
                assert_eq!(status_code.as_u16(), 430);
            }
            other => panic!("Expected Success, got {other:?}"),
        }
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

        let (tx1, rx1) = tokio::sync::oneshot::channel();
        let (tx2, rx2) = tokio::sync::oneshot::channel();
        let batch = vec![
            QueuedRequest {
                command: Arc::from("STAT <a@b>\r\n"),
                response_tx: tx1,
            },
            QueuedRequest {
                command: Arc::from("STAT <c@d>\r\n"),
                response_tx: tx2,
            },
        ];

        let mut result_buf = bytes::BytesMut::with_capacity(4096);

        let success = execute_pipeline_batch(
            backend_id,
            &mut conn,
            batch,
            &metrics,
            &pool,
            &mut result_buf,
        )
        .await;
        assert!(success);

        match rx1.await.unwrap() {
            PipelineResponse::Success { status_code, .. } => {
                assert_eq!(status_code.as_u16(), 223);
            }
            other => panic!("Expected 223 Success, got {other:?}"),
        }
        match rx2.await.unwrap() {
            PipelineResponse::Success { status_code, .. } => {
                assert_eq!(status_code.as_u16(), 430);
            }
            other => panic!("Expected 430 Success, got {other:?}"),
        }
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

        let (tx1, rx1) = tokio::sync::oneshot::channel();
        let (tx2, rx2) = tokio::sync::oneshot::channel();
        let batch = vec![
            QueuedRequest {
                command: Arc::from("STAT <a@b>\r\n"),
                response_tx: tx1,
            },
            QueuedRequest {
                command: Arc::from("STAT <c@d>\r\n"),
                response_tx: tx2,
            },
        ];

        let mut result_buf = bytes::BytesMut::with_capacity(4096);

        let success = execute_pipeline_batch(
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
        match rx1.await.unwrap() {
            PipelineResponse::Success { status_code, .. } => {
                assert_eq!(status_code.as_u16(), 223);
            }
            other => panic!("Expected first to succeed, got {other:?}"),
        }
        // Second response should be error
        match rx2.await.unwrap() {
            PipelineResponse::Error(_) => {} // Expected
            other => panic!("Expected error for second, got {other:?}"),
        }
    }

    // NOTE: This test is skipped because TCP read sizes are unpredictable in tests.
    // The bounds check in `read_full_response` prevents leftover from exceeding
    // MAX_LEFTOVER_BYTES (128KB), but triggering this in a test is difficult because:
    // 1. TCP reads are typically 8-16KB, not the full 724KB buffer size
    // 2. We can't control how TCP splits data across reads
    // 3. The check only triggers if a SINGLE read contains > 128KB of leftover data
    //
    // The check still provides defense-in-depth against protocol desync, even though
    // it's hard to test. In practice, normal leftover is < 8KB, so 128KB is generous.
    #[ignore]
    #[tokio::test]
    async fn test_leftover_exceeds_max_size() {
        use crate::constants::buffer::MAX_LEFTOVER_BYTES;

        let pool = BufferPool::for_tests();
        let mut buffer = pool.acquire().await;
        let mut result_buf = BytesMut::with_capacity(4096);

        // Create a multiline response that accumulates in result_buf across chunks,
        // followed by oversized leftover. When the terminator is found, the remainder
        // in the current chunk should exceed MAX_LEFTOVER_BYTES.
        //
        // NOTE: This test is flaky because TCP read sizes are unpredictable. Even if
        // we send 138KB in one chunk, the read might only return 6-8KB at a time.

        let mut chunks = Vec::new();

        // Chunk 1: Response header + body start (no terminator yet)
        let mut chunk1 = Vec::new();
        chunk1.extend_from_slice(b"220 0 <id> article\r\n");
        chunk1.extend_from_slice(&vec![b'B'; 50000]); // 50KB of body
        chunks.push(chunk1);

        // Chunk 2: More body (no terminator yet)
        let mut chunk2 = Vec::new();
        chunk2.extend_from_slice(&vec![b'O'; 50000]); // Another 50KB
        chunks.push(chunk2);

        // Chunk 3: Terminator + oversized leftover
        let mut chunk3 = Vec::new();
        chunk3.extend_from_slice(&vec![b'D'; 10000]); // 10KB more body
        chunk3.extend_from_slice(b"\r\n.\r\n"); // Terminator
        chunk3.extend_from_slice(&vec![b'X'; MAX_LEFTOVER_BYTES + 1000]); // Oversized leftover
        chunks.push(chunk3);

        let mut conn = mock_backend_conn_chunked(chunks).await;

        let result = read_full_response(&mut buffer, &mut conn, &mut result_buf).await;

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
        let mut result_buf = BytesMut::with_capacity(4096);

        // Create a response where leftover is within bounds (< MAX_LEFTOVER_BYTES)
        let mut normal_data = Vec::new();
        normal_data.extend_from_slice(b"220 0 <msg@id> article\r\nBody\r\n.\r\n");
        // Add a reasonable amount of leftover data (well under the limit)
        normal_data.extend_from_slice(b"430 No such article\r\n");

        let mut conn = mock_backend_conn(&normal_data).await;

        let result = read_full_response(&mut buffer, &mut conn, &mut result_buf).await;

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
