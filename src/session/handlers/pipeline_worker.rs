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
    _router: Arc<crate::router::BackendSelector>,
    metrics: MetricsCollector,
    buffer_pool: BufferPool,
) {
    let backend_id = config.backend_id;
    info!(
        "Pipeline worker started for backend {:?} (batch_size={})",
        backend_id, config.batch_size
    );

    loop {
        // Wait for at least one request, then grab up to batch_size
        let batch = queue.dequeue_batch(config.batch_size).await;
        let batch_len = batch.len();

        debug!(
            "Pipeline worker backend {:?}: dequeued batch of {} requests",
            backend_id, batch_len
        );

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
        let success =
            execute_pipeline_batch(backend_id, &mut conn, batch, &metrics, &buffer_pool).await;

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
    let mut leftover: Vec<u8> = Vec::new(); // Typically empty, small when non-empty
    let mut result_buf = bytes::BytesMut::with_capacity(4096); // Reused across responses

    let mut batch_iter = batch.into_iter().enumerate();
    while let Some((i, req)) = batch_iter.next() {
        // Read the response for this command
        match read_full_response(&mut buffer, conn, &mut leftover, &mut result_buf).await {
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
                    "Pipeline worker backend {:?}: read failed at response {}/{}: {}",
                    backend_id,
                    i + 1,
                    batch_len,
                    e
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
    true
}

/// Read a complete NNTP response (status line + multiline body if applicable).
///
/// Returns the full response as `Bytes` and the parsed status code.
/// If leftover bytes from a previous response are provided, they are processed first.
///
/// `result_buf` is cleared and reused for each response to avoid per-response allocations.
async fn read_full_response(
    buffer: &mut crate::pool::PooledBuffer,
    conn: &mut crate::stream::ConnectionStream,
    leftover: &mut Vec<u8>,
    result_buf: &mut bytes::BytesMut,
) -> Result<(bytes::Bytes, crate::protocol::StatusCode)> {
    use crate::session::streaming::tail_buffer::TailBuffer;

    let mut tail = TailBuffer::default();
    result_buf.clear(); // Reuse buffer from previous response

    // Get first data directly into result_buf (no intermediate Vec)
    if !leftover.is_empty() {
        result_buf.extend_from_slice(leftover);
        leftover.clear();
    } else {
        let n = buffer.read_from(conn).await?;
        if n == 0 {
            anyhow::bail!("Backend connection closed unexpectedly");
        }
        result_buf.extend_from_slice(&buffer[..n]);
    }

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
                leftover.extend_from_slice(&result_buf[end..]);
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
        leftover.extend_from_slice(&result_buf[write_len..]);
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
                    leftover.extend_from_slice(&chunk[write_len..]);
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
