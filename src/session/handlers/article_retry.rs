//! Article routing with availability-aware backend selection
//!
//! Handles routing article commands across backends, using `ArticleAvailability`
//! to skip backends that have already returned 430 for a given article.

use crate::cache::ArticleAvailability;
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

use crate::protocol::RequestContext;
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

/// Outcome of writing a validated backend response to the client.
enum BatchWriteOutcome {
    /// Response bytes were written successfully.
    Written(u64),
    /// Processing must stop with the returned step.
    Step(BatchStep),
}

/// Client-side write state shared across cache, precheck, pipeline, and direct routing paths.
struct RequestExecutionIo<'a, 'b> {
    client_write: &'a mut tokio::net::tcp::WriteHalf<'b>,
    client_to_backend_bytes: &'a mut ClientToBackendBytes,
    backend_to_client_bytes: &'a mut BackendToClientBytes,
}

/// Result of preparing a request before pipeline/direct backend execution.
enum PreparedRequest {
    /// The response was already written to the client.
    Served,
    /// Continue with backend routing using resolved availability state.
    Continue {
        availability: Option<ArticleAvailability>,
    },
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
            if let Err(e) = batch.context(i).write_wire_to(&mut **conn).await {
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
                return Err(SessionError::Backend(e.into()));
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
        let mut buffer = self.buffer_pool.acquire();
        let mut leftover = self.buffer_pool.acquire();
        let mut chunk_data = self.buffer_pool.acquire();
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
        use crate::session::backend;

        let backend_id = bcc.backend_id;
        let request = batch.context(idx);
        let msg_id = request.message_id_value();
        let from_leftover = match self.read_batch_response_chunk(batch, idx, bcc).await {
            Ok(from_leftover) => from_leftover,
            Err(step) => return Ok(step),
        };
        let chunk_data = &mut *bcc.chunk_data;
        let chunk = &chunk_data[..];

        // --- Validate response ---
        let validated =
            backend::parse_backend_status(chunk, chunk.len(), crate::protocol::MIN_RESPONSE_LENGTH);
        let Some(status_code) = validated.status_code else {
            self.log_invalid_batch_response(request, idx, batch, backend_id, chunk, from_leftover);
            return Ok(BatchStep::BackendDead);
        };
        let is_multiline_body = request.expects_multiline_response(status_code);

        // --- Handle 430 (article not found on this backend) ---
        if status_code.as_u16() == 430 {
            return self
                .handle_batch_not_found(
                    request,
                    msg_id,
                    is_multiline_body,
                    bcc,
                    client_write,
                    state,
                )
                .await;
        }

        // --- Success: stream response to client ---
        *state.client_to_backend_bytes = state
            .client_to_backend_bytes
            .add(request.request_wire_len().get());

        let bytes_written = match self
            .write_batch_success_response(batch, idx, bcc, client_write, is_multiline_body)
            .await?
        {
            BatchWriteOutcome::Written(bytes) => bytes,
            BatchWriteOutcome::Step(step) => return Ok(step),
        };

        *state.backend_to_client_bytes = state.backend_to_client_bytes.add_u64(bytes_written);

        self.record_batch_response_metrics(request, backend_id, status_code, bytes_written);

        Ok(BatchStep::Continue)
    }

    async fn read_batch_response_chunk(
        &self,
        batch: &RequestBatch,
        idx: usize,
        bcc: &mut BatchConnContext<'_>,
    ) -> std::result::Result<bool, BatchStep> {
        let BatchConnContext {
            conn,
            backend_id,
            buffer,
            leftover,
            chunk_data,
        } = bcc;
        let backend_id = *backend_id;
        let conn = &mut ***conn;
        let buffer = &mut **buffer;
        let leftover = &mut **leftover;
        let chunk_data = &mut **chunk_data;

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
                idx,
                leftover_len,
                leftover_hex = %leftover_hex,
                "Batch response starting from leftover bytes"
            );
        } else {
            match buffer.read_from(conn).await {
                Ok(bytes_read) if bytes_read > 0 => {
                    chunk_data.extend_from_slice(&buffer[..bytes_read]);
                }
                Ok(_) => {
                    warn!(
                        client = %self.client_addr,
                        backend = ?backend_id,
                        response_index = idx + 1,
                        total_commands = batch.len(),
                        "Backend EOF during batch read; closing client connection"
                    );
                    return Err(BatchStep::BackendDead);
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
                    return Err(BatchStep::BackendDead);
                }
            }
        }

        match self.extend_batch_status_line(idx, bcc).await {
            Ok(()) => {}
            Err(step) => return Err(step),
        }
        Ok(from_leftover)
    }

    async fn extend_batch_status_line(
        &self,
        idx: usize,
        bcc: &mut BatchConnContext<'_>,
    ) -> std::result::Result<(), BatchStep> {
        let BatchConnContext {
            conn,
            backend_id,
            buffer,
            chunk_data,
            ..
        } = bcc;
        let backend_id = *backend_id;
        let conn = &mut ***conn;
        let buffer = &mut **buffer;
        let chunk_data = &mut **chunk_data;

        while crate::session::backend::status_line_end(chunk_data).is_none() {
            match buffer.read_from(conn).await {
                Ok(bytes_read) if bytes_read > 0 => {
                    chunk_data.extend_from_slice(&buffer[..bytes_read]);
                }
                Ok(_) => {
                    warn!(
                        client = %self.client_addr,
                        backend = ?backend_id,
                        response_index = idx + 1,
                        partial_bytes = chunk_data.len(),
                        "Backend EOF before complete status line; closing client connection"
                    );
                    return Err(BatchStep::BackendDead);
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
                    return Err(BatchStep::BackendDead);
                }
            }
        }

        Ok(())
    }

    fn log_invalid_batch_response(
        &self,
        request: &RequestContext,
        idx: usize,
        batch: &RequestBatch,
        backend_id: BackendId,
        chunk: &[u8],
        from_leftover: bool,
    ) {
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
    }

    async fn handle_batch_not_found(
        &self,
        request: &RequestContext,
        msg_id: Option<crate::types::MessageId<'_>>,
        is_multiline_body: bool,
        bcc: &mut BatchConnContext<'_>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        state: &mut BatchPipelineState<'_>,
    ) -> Result<BatchStep> {
        let backend_id = bcc.backend_id;
        let leftover = &mut *bcc.leftover;
        let chunk_data = &mut *bcc.chunk_data;

        if !is_multiline_body {
            super::split_single_line_response(chunk_data, leftover);
        }

        self.send_430_to_client(client_write, state.backend_to_client_bytes)
            .await?;
        *state.client_to_backend_bytes = state
            .client_to_backend_bytes
            .add(request.request_wire_len().get());

        if let Some(mid) = msg_id.as_ref() {
            self.cache
                .record_backend_missing(mid.clone(), backend_id)
                .await;
        }
        self.metrics.record_command(backend_id);
        self.metrics.record_error_4xx(backend_id);
        self.metrics.user_command(self.username());
        Ok(BatchStep::Continue)
    }

    async fn write_batch_success_response(
        &self,
        batch: &RequestBatch,
        idx: usize,
        bcc: &mut BatchConnContext<'_>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        is_multiline_body: bool,
    ) -> Result<BatchWriteOutcome> {
        use crate::session::streaming;

        let BatchConnContext {
            conn,
            backend_id,
            leftover,
            chunk_data,
            ..
        } = bcc;
        let backend_id = *backend_id;
        let conn = &mut ***conn;
        let leftover = &mut **leftover;
        let chunk_data = &mut **chunk_data;

        if is_multiline_body {
            let ctx = streaming::StreamContext {
                client_addr: self.client_addr,
                backend_id,
                buffer_pool: &self.buffer_pool,
            };
            return match streaming::stream_multiline_response_pipelined(
                conn,
                client_write,
                &chunk_data[..],
                &ctx,
                leftover,
            )
            .await
            {
                Ok(bytes) => {
                    debug!(
                        client = %self.client_addr,
                        backend = ?backend_id,
                        idx,
                        bytes_written = bytes,
                        new_leftover_len = leftover.len(),
                        new_leftover_hex = %crate::session::backend::format_hex_preview(leftover, 64),
                        "Batch multiline response streamed"
                    );
                    Ok(BatchWriteOutcome::Written(bytes))
                }
                Err(crate::session::streaming::StreamingError::ClientDisconnect(io_err)) => {
                    Ok(BatchWriteOutcome::Step(BatchStep::ClientDisconnect(io_err)))
                }
                Err(e) => {
                    warn!(
                        client = %self.client_addr,
                        backend = ?backend_id,
                        command_index = idx + 1,
                        total_commands = batch.len(),
                        error = %e,
                        "Backend died mid-batch article after partial data sent to client; \
                         closing client connection to prevent protocol corruption"
                    );
                    Ok(BatchWriteOutcome::Step(BatchStep::ClientDisconnect(
                        std::io::Error::new(
                            std::io::ErrorKind::BrokenPipe,
                            "backend error mid-batch; partial article already sent",
                        ),
                    )))
                }
            };
        }

        super::split_single_line_response(chunk_data, leftover);
        client_write.write_all(&chunk_data[..]).await?;
        Ok(BatchWriteOutcome::Written(chunk_data.len() as u64))
    }

    fn record_batch_response_metrics(
        &self,
        request: &RequestContext,
        backend_id: BackendId,
        status_code: crate::protocol::StatusCode,
        bytes_written: u64,
    ) {
        use crate::session::routing::{MetricsAction, determine_metrics_action_for_request};

        self.metrics.record_command(backend_id);
        self.metrics.user_command(self.username());
        match determine_metrics_action_for_request(request, status_code) {
            MetricsAction::Article => self.metrics.record_article(backend_id, bytes_written),
            MetricsAction::Error4xx => self.metrics.record_error_4xx(backend_id),
            MetricsAction::Error5xx => self.metrics.record_error_5xx(backend_id),
            MetricsAction::None => {}
        }
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
    ) -> Result<(), SessionError> {
        self.log_route_request(request);

        let mut io = RequestExecutionIo {
            client_write,
            client_to_backend_bytes,
            backend_to_client_bytes,
        };
        let mut availability = match self
            .prepare_request_execution(&router, request, &mut io)
            .await?
        {
            PreparedRequest::Served => return Ok(()),
            PreparedRequest::Continue { availability } => availability,
        };

        if !request.is_large_transfer()
            && self
                .try_pipeline_request(&router, request, &mut io, &mut availability)
                .await?
        {
            return Ok(());
        }

        self.execute_article_retry_loop(&router, request, availability, &mut io)
            .await
    }

    fn log_route_request(&self, request: &RequestContext) {
        debug!(
            "Client {} ENTERED route_and_execute_request: kind={:?}, verb={:?}",
            self.client_addr,
            request.kind(),
            request.verb()
        );
        debug!(
            "Client {} msg_id={:?}, cache_articles={}",
            self.client_addr,
            request.message_id(),
            self.cache_articles
        );
    }

    async fn prepare_request_execution(
        &self,
        router: &Arc<BackendSelector>,
        request: &mut RequestContext,
        io: &mut RequestExecutionIo<'_, '_>,
    ) -> Result<PreparedRequest, SessionError> {
        let availability = match self
            .try_serve_from_cache(request, router, io.client_write, io.backend_to_client_bytes)
            .await?
        {
            CacheLookupResult::Hit => return Ok(PreparedRequest::Served),
            CacheLookupResult::PartialHit => request
                .cache_availability()
                .map(Self::request_cache_availability),
            CacheLookupResult::Miss => None,
        };
        if self.try_adaptive_precheck(router, request, io).await? {
            return Ok(PreparedRequest::Served);
        }

        Ok(PreparedRequest::Continue { availability })
    }

    const fn request_cache_availability(
        availability: crate::protocol::RequestCacheAvailability,
    ) -> ArticleAvailability {
        ArticleAvailability::from_bits(availability.checked_bits(), availability.missing_bits())
    }

    async fn try_adaptive_precheck(
        &self,
        router: &Arc<BackendSelector>,
        request: &RequestContext,
        io: &mut RequestExecutionIo<'_, '_>,
    ) -> Result<bool, SessionError> {
        if !self.adaptive_precheck || !(request.is_stat() || request.is_head()) {
            return Ok(false);
        }
        let Some(msg_id) = request.message_id_value() else {
            return Ok(false);
        };

        let deps = self.precheck_deps(router);
        let bytes_written = if let Some(entry) = precheck::precheck(&deps, request, &msg_id).await {
            if let Some(write) = write_cached_article_response(
                io.client_write,
                &entry,
                request.kind(),
                msg_id.as_str(),
            )
            .await
            .map_err(|e| SessionError::from(anyhow::Error::from(e)))?
            {
                write.wire_len.get()
            } else {
                Self::write_no_such_article_response(io.client_write).await?
            }
        } else {
            Self::write_no_such_article_response(io.client_write).await?
        };
        *io.backend_to_client_bytes = io.backend_to_client_bytes.add(bytes_written);
        Ok(true)
    }

    async fn write_no_such_article_response(
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
    ) -> Result<usize, SessionError> {
        client_write
            .write_all(crate::protocol::NO_SUCH_ARTICLE)
            .await
            .map(|()| crate::protocol::NO_SUCH_ARTICLE.len())
            .map_err(|e| SessionError::from(anyhow::Error::from(e)))
    }

    async fn try_pipeline_request(
        &self,
        router: &Arc<BackendSelector>,
        request: &RequestContext,
        io: &mut RequestExecutionIo<'_, '_>,
        availability: &mut Option<ArticleAvailability>,
    ) -> Result<bool, SessionError> {
        if let Ok(backend_id) =
            router.route_with_availability(self.client_id, availability.as_ref())
            && let Some(queue) = router.get_backend_queue(backend_id)
        {
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

            match queue.try_enqueue(queued_context) {
                Ok(()) => {
                    self.metrics.record_pipeline_enqueue();

                    match rx.await {
                        Ok(Ok(completed))
                            if completed
                                .context
                                .response_metadata()
                                .is_some_and(|response| response.status().as_u16() != 430) =>
                        {
                            completed
                                .context
                                .response_payload()
                                .expect("completed request context carries response payload")
                                .write_all_to(io.client_write)
                                .await
                                .map_err(|e| SessionError::from(anyhow::Error::from(e)))?;
                            let response = completed
                                .context
                                .response_metadata()
                                .expect("completed queued request records response metadata");
                            *io.backend_to_client_bytes =
                                io.backend_to_client_bytes.add(response.wire_len().get());
                            *io.client_to_backend_bytes = io
                                .client_to_backend_bytes
                                .add(completed.context.request_wire_len().get());
                            self.metrics.record_pipeline_complete();
                            guard.complete();
                            return Ok(true);
                        }
                        Ok(Ok(completed)) => {
                            let completed_backend_id = completed
                                .context
                                .backend_id()
                                .expect("completed queued request records backend id");
                            debug!(
                                "Client {} pipeline got 430 from backend {:?}, falling through to retry loop",
                                self.client_addr, completed_backend_id
                            );
                            self.handle_430_availability(
                                completed_backend_id,
                                availability.get_or_insert_default(),
                            );
                            self.metrics.record_pipeline_complete();
                        }
                        Ok(Err(e)) => {
                            debug!(
                                "Client {} pipeline error for backend {:?}: {}",
                                self.client_addr, backend_id, e
                            );
                        }
                        Err(_) => {
                            debug!(
                                "Client {} pipeline worker dropped response channel",
                                self.client_addr
                            );
                        }
                    }
                }
                Err(e) => {
                    debug!(
                        "Client {} pipeline queue full for backend {:?}: {}",
                        self.client_addr, backend_id, e
                    );
                }
            }
        }

        Ok(false)
    }

    async fn execute_article_retry_loop(
        &self,
        router: &Arc<BackendSelector>,
        request: &mut RequestContext,
        availability: Option<ArticleAvailability>,
        io: &mut RequestExecutionIo<'_, '_>,
    ) -> Result<(), SessionError> {
        debug!(
            "Client {} starting availability routing for request kind={:?}, verb={:?}",
            self.client_addr,
            request.kind(),
            request.verb()
        );

        let mut buffer = self.buffer_pool.acquire();
        let mut availability = availability.unwrap_or_default();
        debug!(
            "Client {} availability routing: missing_bits={:08b}, backend_count={}",
            self.client_addr,
            availability.missing_bits(),
            router.backend_count().get()
        );

        while !availability.all_exhausted(router.backend_count()) {
            let attempt = self
                .try_backend_for_article(
                    router,
                    request,
                    io.client_write,
                    &mut ArticleAttemptState {
                        availability: &mut availability,
                        buffer: &mut buffer,
                        client_to_backend_bytes: io.client_to_backend_bytes,
                    },
                )
                .await;
            match attempt {
                Ok(BackendAttemptResult::Success) => {
                    let response = request
                        .response_metadata()
                        .expect("successful direct attempt records response metadata");
                    *io.backend_to_client_bytes =
                        io.backend_to_client_bytes.add(response.wire_len().get());
                    let msg_id = request.message_id_value();
                    self.sync_availability_if_needed(msg_id.as_ref(), &availability)
                        .await;
                    return Ok(());
                }
                Ok(BackendAttemptResult::ArticleNotFound { backend_id }) => {
                    debug!(
                        "Client {} backend {:?} returned 430 during retry",
                        self.client_addr, backend_id
                    );
                }
                Ok(BackendAttemptResult::BackendUnavailable) => {}
                Err(e @ SessionError::ClientDisconnect(_)) => {
                    debug!(
                        "Client {} disconnected, stopping retry loop",
                        self.client_addr
                    );
                    return Err(e);
                }
                Err(SessionError::Backend(e)) => {
                    warn!(
                        client = %self.client_addr,
                        error = %e,
                        "Backend error during article retry (will try next backend)"
                    );
                }
            }
        }

        debug!(
            "Client {} all backends exhausted for {:?}, sending 430",
            self.client_addr,
            request.message_id_value()
        );
        let msg_id = request.message_id_value();
        self.sync_availability_if_needed(msg_id.as_ref(), &availability)
            .await;
        self.send_430_to_client(io.client_write, io.backend_to_client_bytes)
            .await?;

        Ok(())
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
        if availability.missing_bits() == 0 {
            return;
        }

        if let Some(msg_id_ref) = msg_id {
            self.cache
                .sync_availability(msg_id_ref.clone(), availability)
                .await;
        }
    }
}
