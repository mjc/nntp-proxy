//! Article routing with availability-aware backend selection
//!
//! Handles routing article commands across backends, using `ArticleAvailability`
//! to skip backends that have already returned 430 for a given article.

use crate::router::backend_queue::QueuedContext;
use crate::router::{BackendSelector, CommandGuard};
use crate::session::ClientSession;
use crate::session::SessionError;
use crate::session::handlers::cache_operations::{
    CacheLookupResult, write_cached_article_response,
};
use crate::session::handlers::command_execution::{ArticleAttemptState, BackendAttemptResult};
use crate::types::{BackendId, BackendToClientBytes, ClientToBackendBytes};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

use crate::protocol::{RequestContext, ResponseShape};
use crate::session::handlers::pipeline::RequestBatch;
use crate::session::precheck;

/// Mutable pipeline batch state passed into `batch_execute_articles`
pub(super) struct BatchPipelineState<'a> {
    pub client_to_backend_bytes: &'a mut ClientToBackendBytes,
    pub backend_to_client_bytes: &'a mut BackendToClientBytes,
}

/// Backend connection context for `process_batch_response`
///
/// Groups the backend connection, its ID, and the scratch buffer to keep
/// `process_batch_response`'s parameter count within clippy limits.
struct BatchConnContext<'a> {
    conn: &'a mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
    backend_id: BackendId,
    buffer: &'a mut crate::pool::PooledBuffer,
    leftover: &'a mut crate::pool::PooledBuffer,
    chunk_data: &'a mut crate::pool::PooledBuffer,
}

/// Per-response outcome from `process_batch_response`
enum BatchStep {
    /// Response handled successfully; continue to the next command.
    Continue,
    /// Client disconnected; backend was cleanly drained.
    /// Caller should complete the guard and propagate the error.
    ClientDisconnect(std::io::Error),
    /// Backend/protocol failure; caller must close the client session and remove
    /// the backend connection. It is not safe to synthesize article responses.
    BackendDead,
}

impl ClientSession {
    /// Batch-execute ARTICLE/BODY commands using TCP pipelining.
    ///
    /// Sends all commands to one backend connection upfront, then reads/streams
    /// responses in order. This exploits TCP pipelining: the backend starts
    /// responding immediately, and responses queue in our receive buffer while
    /// we stream earlier ones to the client.
    pub(super) async fn batch_execute_articles(
        &self,
        router: &Arc<BackendSelector>,
        batch: &RequestBatch,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        pipeline: BatchPipelineState<'_>,
    ) -> Result<(), SessionError> {
        let mut state = pipeline;

        // M1: Load availability for the first command's message-ID
        let first_msg_id = batch.context(0).message_id_value();
        let availability = self
            .load_article_availability(first_msg_id.as_ref(), router.backend_count())
            .await;

        // Route to a single backend for the whole batch with availability awareness.
        // One pending count for one connection — don't inflate pending by N,
        // as that skews load_ratio and can cause other sessions to over-allocate
        // connections on other backends, hitting provider connection limits.
        let backend_id = router.route_with_availability(self.client_id, Some(&availability))?;
        let guard = CommandGuard::new(router.clone(), backend_id);

        let Some(provider) = router.backend_provider(backend_id) else {
            return Err(SessionError::Backend(anyhow::anyhow!(
                "Backend {backend_id:?} has no connection provider"
            )));
        };

        let conn_raw = provider
            .get_pooled_connection()
            .await
            .map_err(|e| SessionError::Backend(e.into()))?;
        let mut conn = crate::pool::ConnectionGuard::new(conn_raw, provider.clone());

        // Phase 1: Write all commands, then flush
        for i in 0..batch.len() {
            if let Err(e) =
                crate::session::backend::write_request(&mut **conn, batch.context(i)).await
            {
                warn!(
                    client = %self.client_addr,
                    backend = ?backend_id,
                    batch_size = batch.len(),
                    error = %e,
                    "Batch write failed, removing connection"
                );
                // Unconditional: write goes to the backend, not the client.
                // A client disconnect cannot cause a backend write error.
                // conn drops here → ConnectionGuard::remove_with_cooldown
                return Err(SessionError::Backend(e));
            }
        }
        if let Err(e) = conn.flush().await {
            warn!(
                client = %self.client_addr,
                backend = ?backend_id,
                batch_size = batch.len(),
                error = %e,
                "Batch flush failed, removing connection"
            );
            // Unconditional: flush goes to the backend, not the client.
            // A client disconnect cannot cause a backend flush error.
            // conn drops here → ConnectionGuard::remove_with_cooldown
            return Err(SessionError::Backend(e.into()));
        }

        // Phase 2: Read and stream responses in order.
        // Buffer acquired once, reused across all responses.
        // Scratch buffers are needed only while backend responses are being read.
        let mut buffer = self.buffer_pool.acquire().await;
        let mut leftover = self.buffer_pool.acquire().await;
        let mut chunk_data = self.buffer_pool.acquire().await;
        debug!(
            client = %self.client_addr,
            backend = ?backend_id,
            batch_size = batch.len(),
            first_msg_id = ?first_msg_id,
            "Batch Phase 2: reading responses"
        );

        for i in 0..batch.len() {
            let mut bcc = BatchConnContext {
                conn: &mut conn,
                backend_id,
                buffer: &mut buffer,
                leftover: &mut leftover,
                chunk_data: &mut chunk_data,
            };
            match self
                .process_batch_response(batch, i, &mut bcc, client_write, &mut state)
                .await?
            {
                BatchStep::Continue => {}
                BatchStep::ClientDisconnect(io_err) => {
                    // Articles i+1..N-1 responses are still unread on the backend stream.
                    // Releasing would return a dirty connection to the pool; the next request
                    // would read stale response bytes as if they were a fresh response,
                    // triggering Invalid → remove_with_cooldown cascade.
                    // ConnectionGuard drop → remove_with_cooldown is the correct fate.
                    guard.complete();
                    return Err(SessionError::ClientDisconnect(io_err));
                }
                BatchStep::BackendDead => {
                    // Non-430 backend/protocol failures must not be translated into
                    // "article not found". Close the client so it retries the batch
                    // on a fresh connection with clean protocol state.
                    guard.complete();
                    return Err(SessionError::ClientDisconnect(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "backend/protocol failure during pipelined batch; closing client",
                    )));
                }
            }
        }

        // All commands handled — connection healthy, return to pool.
        let _ = conn.release();
        guard.complete();
        Ok(())
    }

    /// Process one response in a pipelined batch.
    ///
    /// Reads the next response from `conn`, validates it, and streams it to the client.
    /// Returns a [`BatchStep`] that tells the caller how to proceed.
    ///
    /// # Connection pool discipline
    /// This method does **not** call `remove_with_cooldown` — that responsibility belongs
    /// to the caller. Instead it returns `BackendDead` to signal that the
    /// backend connection must be discarded and the client session closed.
    async fn process_batch_response(
        &self,
        batch: &RequestBatch,
        idx: usize,
        bcc: &mut BatchConnContext<'_>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        state: &mut BatchPipelineState<'_>,
    ) -> Result<BatchStep> {
        use crate::session::routing::{MetricsAction, determine_metrics_action};
        use crate::session::{backend, streaming};

        let backend_id = bcc.backend_id;
        let request = batch.context(idx);
        let msg_id = request.message_id_value();
        let leftover = &mut bcc.leftover;
        let chunk_data = &mut bcc.chunk_data;

        // --- Read first chunk (from leftover or fresh read) ---
        chunk_data.clear();
        let from_leftover = !leftover.is_empty();
        if from_leftover {
            let leftover_len = leftover.len();
            let leftover_hex = crate::session::backend::format_hex_preview(leftover, 64);
            chunk_data.extend_from_slice(leftover);
            leftover.clear();
            debug!(
                client = %self.client_addr,
                backend = ?backend_id,
                idx = idx,
                leftover_len = leftover_len,
                leftover_hex = %leftover_hex,
                "Batch response starting from leftover bytes"
            );
        } else {
            match bcc.buffer.read_from(&mut **bcc.conn).await {
                Ok(bytes_read) if bytes_read > 0 => {
                    chunk_data.extend_from_slice(&bcc.buffer[..bytes_read]);
                }
                Ok(_) => {
                    warn!(
                        client = %self.client_addr,
                        backend = ?backend_id,
                        response_index = idx + 1,
                        total_commands = batch.len(),
                        "Backend EOF during batch read; closing client connection"
                    );
                    // Backend dead — unconditional remove (write/flush already succeeded,
                    // so this is a backend-side failure, not a client disconnect).
                    return Ok(BatchStep::BackendDead);
                }
                Err(e) => {
                    warn!(
                        client = %self.client_addr,
                        backend = ?backend_id,
                        response_index = idx + 1,
                        total_commands = batch.len(),
                        error = %e,
                        "Backend read error during batch; closing client connection"
                    );
                    // Backend dead — unconditional remove.
                    return Ok(BatchStep::BackendDead);
                }
            }
        }

        // --- Read through the complete status line before classifying ---
        while backend::status_line_end(chunk_data).is_none() {
            match bcc.buffer.read_from(&mut **bcc.conn).await {
                Ok(bytes_read) if bytes_read > 0 => {
                    chunk_data.extend_from_slice(&bcc.buffer[..bytes_read]);
                }
                Ok(_) => {
                    warn!(
                        client = %self.client_addr,
                        backend = ?backend_id,
                        response_index = idx + 1,
                        partial_bytes = chunk_data.len(),
                        "Backend EOF before complete status line; closing client connection"
                    );
                    // Backend dead — partial read means backend state unknown.
                    return Ok(BatchStep::BackendDead);
                }
                Err(e) => {
                    warn!(
                        client = %self.client_addr,
                        backend = ?backend_id,
                        response_index = idx + 1,
                        partial_bytes = chunk_data.len(),
                        error = %e,
                        "Backend read error before complete status line; closing client connection"
                    );
                    // Backend dead — partial read means backend state unknown.
                    return Ok(BatchStep::BackendDead);
                }
            }
        }

        let chunk = &chunk_data[..];

        // --- Validate response ---
        let validated = backend::validate_backend_response(
            chunk,
            chunk.len(),
            crate::protocol::MIN_RESPONSE_LENGTH,
        );
        let Some(status_code) = validated.status_code else {
            warn!(
                client = %self.client_addr,
                backend = ?backend_id,
                request_kind = ?request.kind(),
                request_verb = ?request.verb(),
                response_index = idx + 1,
                total_commands = batch.len(),
                chunk_len = chunk.len(),
                first_bytes_hex = %crate::session::backend::format_hex_preview(chunk, 256),
                first_bytes_utf8 = %String::from_utf8_lossy(&chunk[..chunk.len().min(256)]),
                source = if from_leftover { "leftover" } else { "fresh_read" },
                "Backend returned Invalid response during batch, aborting batch"
            );

            return Ok(BatchStep::BackendDead);
        };
        let is_multiline = matches!(
            request.response_shape(status_code),
            ResponseShape::Multiline
        );

        // --- Handle 430 (article not found on this backend) ---
        if status_code.as_u16() == 430 {
            // Single-line 430: split at line boundary, saving the next response's prefix.
            if !is_multiline {
                super::split_single_line_response(chunk_data, leftover);
            }

            self.send_430_to_client(client_write, state.backend_to_client_bytes)
                .await?;
            *state.client_to_backend_bytes = state.client_to_backend_bytes.add(request.wire_len());

            if let Some(mid) = msg_id.as_ref() {
                self.cache
                    .record_backend_missing(mid.clone(), backend_id)
                    .await;
            }
            self.metrics.record_command(backend_id);
            self.metrics.record_error_4xx(backend_id);
            self.metrics.user_command(self.username());
            return Ok(BatchStep::Continue);
        }

        // --- Success: stream response to client ---
        *state.client_to_backend_bytes = state.client_to_backend_bytes.add(request.wire_len());

        let bytes_written = if is_multiline {
            let ctx = streaming::StreamContext {
                client_addr: self.client_addr,
                backend_id,
                buffer_pool: &self.buffer_pool,
            };
            match streaming::stream_multiline_response_pipelined(
                &mut **bcc.conn,
                client_write,
                chunk,
                &ctx,
                leftover,
            )
            .await
            {
                Ok(bytes) => {
                    debug!(
                        client = %self.client_addr,
                        backend = ?backend_id,
                        idx = idx,
                        bytes_written = bytes,
                        new_leftover_len = leftover.len(),
                        new_leftover_hex = %crate::session::backend::format_hex_preview(leftover, 64),
                        "Batch multiline response streamed"
                    );
                    bytes
                }
                Err(crate::session::streaming::StreamingError::ClientDisconnect(io_err)) => {
                    // Client disconnected — backend was cleanly drained by the streaming layer.
                    return Ok(BatchStep::ClientDisconnect(io_err));
                }
                Err(e) => {
                    // Backend died mid-article after partial data was already written to the
                    // client. Sending 430s for the remaining commands would inject garbage into
                    // the middle of the in-progress article body — corrupting the protocol
                    // stream and causing nzbget/sabnzbd to see BrokenPipe or hang waiting for
                    // \r\n.\r\n that never arrives.
                    //
                    // The only safe recovery is to close the client connection; the download
                    // client will retry the segment on a fresh connection.
                    //
                    // ConnectionGuard drop → remove_with_cooldown (must_remove_connection()
                    // is always true for BackendEof / BackendDirty / Io).
                    warn!(
                        client = %self.client_addr,
                        backend = ?backend_id,
                        command_index = idx + 1,
                        total_commands = batch.len(),
                        error = %e,
                        "Backend died mid-batch article after partial data sent to client; \
                         closing client connection to prevent protocol corruption"
                    );
                    return Ok(BatchStep::ClientDisconnect(std::io::Error::new(
                        std::io::ErrorKind::BrokenPipe,
                        "backend error mid-batch; partial article already sent",
                    )));
                }
            }
        } else {
            // Single-line response: split at line boundary, saving the next response's prefix.
            super::split_single_line_response(chunk_data, leftover);
            client_write.write_all(&chunk_data[..]).await?;
            chunk_data.len() as u64
        };

        *state.backend_to_client_bytes = state.backend_to_client_bytes.add(bytes_written as usize);

        // Record metrics
        self.metrics.record_command(backend_id);
        self.metrics.user_command(self.username());
        match determine_metrics_action(status_code.as_u16(), is_multiline) {
            MetricsAction::Article => {
                self.metrics.record_article(backend_id, bytes_written);
            }
            MetricsAction::Error4xx => self.metrics.record_error_4xx(backend_id),
            MetricsAction::Error5xx => self.metrics.record_error_5xx(backend_id),
            MetricsAction::None => {}
        }

        Ok(BatchStep::Continue)
    }

    /// Route a single request to a backend and execute it
    ///
    /// This function is `pub(super)` to allow reuse of per-command routing logic by sibling handler modules
    /// (such as `hybrid.rs`) that also need to route commands.
    pub(super) async fn route_and_execute_request(
        &self,
        router: Arc<BackendSelector>,
        request: &mut RequestContext,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        client_to_backend_bytes: &mut ClientToBackendBytes,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<crate::types::BackendId, SessionError> {
        debug!(
            "Client {} ENTERED route_and_execute_request: kind={:?}, verb={:?}",
            self.client_addr,
            request.kind(),
            request.verb()
        );
        // Extract message-ID early for cache/availability tracking
        let msg_id = request.message_id_value().map(|msg_id| msg_id.to_owned());

        debug!(
            "Client {} msg_id={:?}, cache_articles={}",
            self.client_addr, msg_id, self.cache_articles
        );

        // Try cache first - may return early if cache hit.
        // On partial hit, the request carries availability info to avoid a redundant cache.get().
        let cached_availability = match self
            .try_serve_from_cache(
                msg_id.as_ref(),
                request,
                &router,
                client_write,
                backend_to_client_bytes,
            )
            .await?
        {
            CacheLookupResult::Hit => {
                return Ok(request
                    .backend_id()
                    .expect("cache hit records selected backend id"));
            }
            CacheLookupResult::PartialHit => request.cache_availability().map(|availability| {
                crate::cache::ArticleAvailability::from_bits(
                    availability.checked_bits(),
                    availability.missing_bits(),
                )
            }),
            CacheLookupResult::Miss => None,
        };

        // H3: Hoist availability loading before pipeline/retry branch (compute once, use in both paths)
        // O1: cached_availability already contains the result of cache.get() - reuse it
        let mut availability = cached_availability;

        // Adaptive prechecking for STAT/HEAD commands (if enabled and cache missed)
        if self.adaptive_precheck
            && let Some(ref msg_id_ref) = msg_id
        {
            let is_head = request.is_head();
            if request.is_stat() || is_head {
                let deps = self.precheck_deps(&router);
                let bytes_written = if let Some(entry) =
                    precheck::precheck(&deps, request, msg_id_ref, is_head).await
                {
                    if let Some(write) = write_cached_article_response(
                        client_write,
                        &entry,
                        request.verb(),
                        msg_id_ref,
                    )
                    .await
                    .map_err(|e| SessionError::from(anyhow::Error::from(e)))?
                    {
                        write.wire_len.get()
                    } else {
                        client_write
                            .write_all(crate::protocol::NO_SUCH_ARTICLE)
                            .await
                            .map_err(|e| SessionError::from(anyhow::Error::from(e)))?;
                        crate::protocol::NO_SUCH_ARTICLE.len()
                    }
                } else {
                    client_write
                        .write_all(crate::protocol::NO_SUCH_ARTICLE)
                        .await
                        .map_err(|e| SessionError::from(anyhow::Error::from(e)))?;
                    crate::protocol::NO_SUCH_ARTICLE.len()
                };
                *backend_to_client_bytes = backend_to_client_bytes.add(bytes_written);
                return Ok(BackendId::from_index(0));
            }
        }

        // Detect large-transfer commands that should skip the pipeline.
        // ARTICLE and BODY responses can be many megabytes; the pipeline worker
        // serializes all clients through one connection per backend, killing
        // throughput. These commands fall through to the direct streaming path
        // which gives each client its own pooled connection (~120 MB/s).
        let is_large_transfer = request.is_large_transfer();

        // Try pipeline path: if the routed backend has a pipeline queue, enqueue
        // the command and await the response instead of acquiring a direct connection.
        // This allows N client sessions to share M backend connections (N >> M).
        // NOTE: Pipeline path uses availability-aware routing to respect 430s.
        if !is_large_transfer {
            // H3: Use pre-loaded availability (no redundant cache lookup)
            // Route with availability awareness (avoids backends that returned 430)
            if let Ok(backend_id) =
                router.route_with_availability(self.client_id, availability.as_ref())
                && let Some(queue) = router.get_backend_queue(backend_id)
            {
                // Wrap in guard - decrements pending_count on all exit paths
                let guard = CommandGuard::new(router.clone(), backend_id);

                debug!(
                    "Client {} using pipeline path for backend {:?}: kind={:?}, verb={:?}",
                    self.client_addr,
                    backend_id,
                    request.kind(),
                    request.verb()
                );

                let (tx, rx) = tokio::sync::oneshot::channel();
                let queued_context = QueuedContext::new(request.clone(), tx);

                // Enqueue with backpressure - fail fast if queue is full
                match queue.try_enqueue(queued_context) {
                    Ok(()) => {
                        self.metrics.record_pipeline_enqueue();

                        // Await response from the pipeline worker
                        match rx.await {
                            Ok(Ok(completed))
                                if completed
                                    .context
                                    .response_metadata()
                                    .is_some_and(|response| response.status().as_u16() != 430) =>
                            {
                                // Success - article found, return immediately
                                completed
                                    .context
                                    .write_response_payload_to(client_write)
                                    .await
                                    .map_err(|e| SessionError::from(anyhow::Error::from(e)))?;
                                let response = completed
                                    .context
                                    .response_metadata()
                                    .expect("completed queued request records response metadata");
                                *backend_to_client_bytes =
                                    backend_to_client_bytes.add(response.wire_len().get());
                                *client_to_backend_bytes =
                                    client_to_backend_bytes.add(completed.context.wire_len());
                                self.metrics.record_pipeline_complete();
                                guard.complete();
                                return Ok(completed
                                    .context
                                    .backend_id()
                                    .expect("completed queued request records backend id"));
                            }
                            Ok(Ok(completed)) => {
                                let completed_backend_id = completed
                                    .context
                                    .backend_id()
                                    .expect("completed queued request records backend id");
                                // 430 (article not found) - fall through to retry loop
                                debug!(
                                    "Client {} pipeline got 430 from backend {:?}, falling through to retry loop",
                                    self.client_addr, completed_backend_id
                                );
                                // O2: Record this backend as missing in local availability (no cache round-trip)
                                let avail = availability.get_or_insert_default();
                                avail.record_missing(completed_backend_id);
                                self.metrics.record_pipeline_complete();
                                self.metrics.record_error_4xx(completed_backend_id);
                                // Fall through - guard drops, decrementing pending_count
                            }
                            Ok(Err(e)) => {
                                debug!(
                                    "Client {} pipeline error for backend {:?}: {}",
                                    self.client_addr, backend_id, e
                                );
                                // Fall through - guard drops, decrementing pending_count
                            }
                            Err(_) => {
                                debug!(
                                    "Client {} pipeline worker dropped response channel",
                                    self.client_addr
                                );
                                // Fall through - guard drops, decrementing pending_count
                            }
                        }
                    }
                    Err(e) => {
                        debug!(
                            "Client {} pipeline queue full for backend {:?}: {}",
                            self.client_addr, backend_id, e
                        );
                        // Fall through - guard drops, decrementing pending_count
                    }
                }
                // Guard drops here if we fell through from any path
            }
        }

        // Execute command with availability-aware backend selection.
        // Load availability from cache if not already checked
        debug!(
            "Client {} starting availability routing for request kind={:?}, verb={:?}",
            self.client_addr,
            request.kind(),
            request.verb()
        );

        let mut buffer = self.buffer_pool.acquire().await;

        // H3: Use pre-loaded availability (already computed above)
        let mut availability = availability.unwrap_or_default();
        debug!(
            "Client {} availability routing: missing_bits={:08b}, backend_count={}",
            self.client_addr,
            availability.missing_bits(),
            router.backend_count().get()
        );

        // DESIGN DECISION: No early-return on cached all_exhausted()
        //
        // We intentionally DO NOT return 430 immediately when cache indicates all
        // backends returned 430, even though this would be faster. Reasons:
        //
        // 1. 430 responses don't mean "article doesn't exist" - they mean "this
        //    backend doesn't have it right now". Backends can acquire articles
        //    via propagation or spool reconstruction.
        //
        // 2. Cached 430s can become stale:
        //    - Backends come back online after being down
        //    - New articles arrive on backends via propagation
        //    - Cache entry is old (close to TTL expiration)
        //
        // 3. The cache TTL (default 5-10 min) isn't an authoritative staleness
        //    indicator - it's a tradeoff between memory and query reduction.
        //
        // Trade-off: We prefer resilience (always checking) over latency (trusting
        // cache completely). The availability tracking still skips backends we
        // know returned 430, so we only try backends we haven't heard from.
        //
        // Future: This could be made configurable if latency becomes critical for
        // workloads where articles definitively don't exist.

        // Track last backend tried for metrics/return value
        let mut last_backend: Option<crate::types::BackendId> = None;

        // Try backends until success or exhaustion
        while !availability.all_exhausted(router.backend_count()) {
            let attempt = self
                .try_backend_for_article(
                    &router,
                    request,
                    msg_id.as_ref(),
                    client_write,
                    &mut ArticleAttemptState {
                        availability: &mut availability,
                        buffer: &mut buffer,
                        client_to_backend_bytes,
                    },
                )
                .await;
            if let Ok(result) = &attempt {
                result.record_on_request(request);
            }
            match attempt {
                Ok(BackendAttemptResult::Success {
                    backend_id,
                    response,
                }) => {
                    *backend_to_client_bytes =
                        backend_to_client_bytes.add(response.wire_len().get());
                    // complete_command already called by CommandGuard inside try_backend_for_article
                    // Sync availability to cache before returning (got a success, but we may have
                    // recorded some 430s from other backends along the way)
                    self.sync_availability_if_needed(msg_id.as_ref(), &availability)
                        .await;
                    return Ok(backend_id);
                }
                Ok(BackendAttemptResult::ArticleNotFound { backend_id }) => {
                    last_backend = Some(backend_id);
                }
                Ok(BackendAttemptResult::BackendUnavailable) => {
                    // Continue to next backend
                }
                Err(e @ SessionError::ClientDisconnect(_)) => {
                    // Client disconnected - stop trying backends
                    debug!(
                        "Client {} disconnected, stopping retry loop",
                        self.client_addr
                    );
                    return Err(e);
                }
                Err(SessionError::Backend(e)) => {
                    // Backend error (timeout, connection error, etc.)
                    // Log at warn level so root cause is visible before any
                    // downstream ConnectionLimitExceeded errors
                    warn!(
                        client = %self.client_addr,
                        error = %e,
                        "Backend error during article retry (will try next backend)"
                    );
                    // Continue to next backend
                }
            }
        }

        // All backends exhausted - sync availability to cache and send final 430
        debug!(
            "Client {} all backends exhausted for {:?}, sending 430",
            self.client_addr, msg_id
        );
        self.sync_availability_if_needed(msg_id.as_ref(), &availability)
            .await;
        self.send_430_to_client(client_write, backend_to_client_bytes)
            .await?;

        Ok(last_backend.unwrap_or_else(|| crate::types::BackendId::from_index(0)))
    }

    /// Load article availability from cache or create fresh tracker
    pub(super) async fn load_article_availability(
        &self,
        msg_id: Option<&crate::types::MessageId<'_>>,
        backend_count: crate::router::BackendCount,
    ) -> crate::cache::ArticleAvailability {
        match msg_id {
            Some(msg_id_ref) => self
                .cache
                .get(msg_id_ref)
                .await
                .map(|entry| {
                    let avail = entry.to_availability(backend_count);
                    debug!("Client {} loaded availability for {}: checked_bits={:08b}, missing_bits={:08b}",
                        self.client_addr, msg_id_ref, avail.checked_bits(), avail.missing_bits());
                    avail
                })
                .unwrap_or_default(),
            None => crate::cache::ArticleAvailability::new(),
        }
    }

    /// Record 430 response in availability tracker.
    ///
    /// Note: `complete_command` is handled by [`crate::router::CommandGuard`] RAII, not here.
    pub(super) fn handle_430_availability(
        &self,
        backend_id: crate::types::BackendId,
        availability: &mut crate::cache::ArticleAvailability,
    ) {
        availability.record_missing(backend_id);

        // Track 430 responses in 4xx metrics for visibility in TUI
        // While 430 is normal retry behavior (not a failure), users want to see
        // these counted to understand backend article distribution
        self.metrics.record_error_4xx(backend_id);
    }

    /// Sync availability to cache if a message ID is present.
    pub(super) async fn sync_availability_if_needed(
        &self,
        msg_id: Option<&crate::types::MessageId<'_>>,
        availability: &crate::cache::ArticleAvailability,
    ) {
        if let Some(msg_id_ref) = msg_id {
            self.cache
                .sync_availability(msg_id_ref.clone(), availability)
                .await;
        }
    }
}
