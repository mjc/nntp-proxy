//! Article routing with availability-aware backend selection
//!
//! Handles routing article commands across backends, using ArticleAvailability
//! to skip backends that have already returned 430 for a given article.

use crate::is_client_disconnect_error;
use crate::router::backend_queue::{PipelineResponse, QueuedRequest};
use crate::router::{BackendSelector, CommandGuard};
use crate::session::ClientSession;
use crate::session::handlers::cache_operations::CacheLookupResult;
use crate::session::handlers::command_execution::BackendAttemptResult;
use crate::types::{BackendId, BackendToClientBytes, ClientToBackendBytes};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

use crate::command::classifier::{is_head_command, is_large_transfer_command, is_stat_command};
use crate::session::precheck;

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
        commands: &[&str],
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        client_to_backend_bytes: &mut ClientToBackendBytes,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<()> {
        use crate::router::CommandGuard;
        use crate::session::routing::{MetricsAction, determine_metrics_action};
        use crate::session::{backend, streaming};

        // M1: Load availability for the first command's message-ID
        let first_msg_id = crate::types::MessageId::extract_from_command_borrowed(commands[0]);
        let availability = self
            .load_article_availability(first_msg_id.as_ref(), router.backend_count())
            .await;

        // Route to a single backend for the whole batch with availability awareness
        // One pending count for one connection — don't inflate pending by N,
        // as that skews load_ratio and can cause other sessions to over-allocate
        // connections on other backends, hitting provider connection limits.
        let backend_id = router.route_command_with_availability(
            self.client_id,
            commands[0],
            Some(&availability),
        )?;
        let guard = CommandGuard::new(router.clone(), backend_id);

        let Some(provider) = router.backend_provider(backend_id) else {
            return Err(anyhow::anyhow!(
                "Backend {:?} has no connection provider",
                backend_id
            ));
        };

        let mut conn = provider.get_pooled_connection().await?;

        // Phase 1: Write all commands, then flush
        for command in commands {
            if let Err(e) = conn.write_all(command.as_bytes()).await {
                warn!(
                    client = %self.client_addr,
                    backend = ?backend_id,
                    batch_size = commands.len(),
                    error = %e,
                    "Batch write failed, removing connection"
                );
                provider.remove_with_cooldown(conn);
                return Err(e.into());
            }
        }
        if let Err(e) = conn.flush().await {
            warn!(
                client = %self.client_addr,
                backend = ?backend_id,
                batch_size = commands.len(),
                error = %e,
                "Batch flush failed, removing connection"
            );
            provider.remove_with_cooldown(conn);
            return Err(e.into());
        }

        // Phase 2: Read and stream responses in order
        // Acquire buffer once before loop (reused across responses)
        let mut buffer = self.buffer_pool.acquire().await;
        let mut chunk_data = bytes::BytesMut::new(); // Holds leftover + new read combined

        for (i, command) in commands.iter().enumerate() {
            let msg_id = crate::types::MessageId::extract_from_command_borrowed(command);

            // Get first chunk of this response
            chunk_data.clear();
            let n = match buffer.read_from(&mut *conn).await {
                Ok(bytes_read) if bytes_read > 0 => {
                    chunk_data.extend_from_slice(&buffer[..bytes_read]);
                    bytes_read
                }
                Ok(_) => {
                    warn!(
                        client = %self.client_addr,
                        backend = ?backend_id,
                        response_index = i + 1,
                        total_commands = commands.len(),
                        "Backend EOF during batch read, sending 430 for remaining commands"
                    );
                    // Send 430 for remaining commands to maintain protocol sync
                    for remaining in &commands[i..] {
                        self.send_430_to_client(client_write, backend_to_client_bytes)
                            .await?;
                        *client_to_backend_bytes = client_to_backend_bytes.add(remaining.len());
                    }
                    provider.remove_with_cooldown(conn);
                    guard.complete();
                    return Ok(());
                }
                Err(e) => {
                    warn!(
                        client = %self.client_addr,
                        backend = ?backend_id,
                        response_index = i + 1,
                        total_commands = commands.len(),
                        error = %e,
                        "Backend read error during batch, sending 430 for remaining commands"
                    );
                    // Send 430 for remaining commands to maintain protocol sync
                    for remaining in &commands[i..] {
                        self.send_430_to_client(client_write, backend_to_client_bytes)
                            .await?;
                        *client_to_backend_bytes = client_to_backend_bytes.add(remaining.len());
                    }
                    provider.remove_with_cooldown(conn);
                    guard.complete();
                    return Ok(());
                }
            };

            // H5: If leftover was too short to validate, read more data
            if n < crate::protocol::MIN_RESPONSE_LENGTH {
                match buffer.read_from(&mut *conn).await {
                    Ok(bytes_read) if bytes_read > 0 => {
                        chunk_data.extend_from_slice(&buffer[..bytes_read]);
                        // Update n for chunk reference below
                    }
                    Ok(_) => {
                        warn!(
                            client = %self.client_addr,
                            backend = ?backend_id,
                            response_index = i + 1,
                            partial_bytes = chunk_data.len(),
                            "Backend EOF with partial status line, sending 430 for remaining commands"
                        );
                        for remaining in &commands[i..] {
                            self.send_430_to_client(client_write, backend_to_client_bytes)
                                .await?;
                            *client_to_backend_bytes = client_to_backend_bytes.add(remaining.len());
                        }
                        provider.remove_with_cooldown(conn);
                        guard.complete();
                        return Ok(());
                    }
                    Err(e) => {
                        warn!(
                            client = %self.client_addr,
                            backend = ?backend_id,
                            response_index = i + 1,
                            partial_bytes = chunk_data.len(),
                            error = %e,
                            "Backend read error with partial status line, sending 430 for remaining commands"
                        );
                        for remaining in &commands[i..] {
                            self.send_430_to_client(client_write, backend_to_client_bytes)
                                .await?;
                            *client_to_backend_bytes = client_to_backend_bytes.add(remaining.len());
                        }
                        provider.remove_with_cooldown(conn);
                        guard.complete();
                        return Ok(());
                    }
                }
            }
            let chunk = &chunk_data[..];
            let n = chunk.len(); // Update n after potential additional read

            // Validate response
            let validated =
                backend::validate_backend_response(chunk, n, crate::protocol::MIN_RESPONSE_LENGTH);
            let response_code = validated.response;
            let is_multiline = validated.is_multiline;

            // Handle 430 — article not found on this backend
            if response_code.is_430() {
                // For single-line 430, find boundary and save leftover
                if !is_multiline && let Some(pos) = memchr::memchr(b'\n', chunk) {
                    let end = pos + 1;
                    if end < n
                        && let Err(e) = conn.stash_leftover(&chunk[end..])
                    {
                        provider.remove_with_cooldown(conn);
                        return Err(e);
                    }
                }

                // Send 430 to client for this command
                self.send_430_to_client(client_write, backend_to_client_bytes)
                    .await?;
                *client_to_backend_bytes = client_to_backend_bytes.add(command.len());

                // O3: Record availability using atomic cache operation
                if let Some(mid) = msg_id.as_ref() {
                    self.cache
                        .record_backend_missing(mid.clone(), backend_id)
                        .await;
                }

                self.metrics.record_command(backend_id);
                self.metrics.record_error_4xx(backend_id);
                self.metrics.user_command(self.username());
                continue;
            }

            // Handle invalid response
            if response_code == crate::protocol::NntpResponse::Invalid {
                warn!(
                    client = %self.client_addr,
                    backend = ?backend_id,
                    command = %command.trim(),
                    first_bytes = ?&chunk[..chunk.len().min(64)],
                    "Backend returned invalid response during batch, sending 430"
                );
                // Send 430 to client as a safe fallback
                self.send_430_to_client(client_write, backend_to_client_bytes)
                    .await?;
                *client_to_backend_bytes = client_to_backend_bytes.add(command.len());
                // M6: Record metrics for invalid responses (same as sequential path)
                self.metrics.record_command(backend_id);
                self.metrics.record_error_4xx(backend_id);
                self.metrics.user_command(self.username());
                continue;
            }

            // Success — stream response to client
            *client_to_backend_bytes = client_to_backend_bytes.add(command.len());

            let bytes_written = if is_multiline {
                match streaming::stream_multiline_response_pipelined(
                    &mut conn,
                    client_write,
                    chunk,
                    n,
                    self.client_addr,
                    backend_id,
                    &self.buffer_pool,
                )
                .await
                {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        // C1: On streaming error, avoid duplicate responses
                        provider.remove_with_cooldown(conn);
                        if is_client_disconnect_error(&e) {
                            return Err(e);
                        }
                        warn!(
                            client = %self.client_addr,
                            backend = ?backend_id,
                            command_index = i + 1,
                            total_commands = commands.len(),
                            error = %e,
                            "Streaming error during pipelined batch, sending 430 for remaining commands"
                        );
                        // Backend died mid-stream. Commands 0..i already handled.
                        // Send 430 for remaining commands to maintain protocol sync.
                        for remaining in &commands[i + 1..] {
                            self.send_430_to_client(client_write, backend_to_client_bytes)
                                .await?;
                            *client_to_backend_bytes = client_to_backend_bytes.add(remaining.len());
                        }
                        guard.complete();
                        return Ok(());
                    }
                }
            } else {
                // Single-line response - find boundary and save leftover
                let write_len = if let Some(pos) = memchr::memchr(b'\n', chunk) {
                    let end = pos + 1;
                    if end < n
                        && let Err(e) = conn.stash_leftover(&chunk[end..])
                    {
                        provider.remove_with_cooldown(conn);
                        return Err(e);
                    }
                    end
                } else {
                    n
                };
                client_write.write_all(&chunk[..write_len]).await?;
                write_len as u64
            };

            *backend_to_client_bytes = backend_to_client_bytes.add(bytes_written as usize);

            // Record metrics
            self.metrics.record_command(backend_id);
            self.metrics.user_command(self.username());
            if let Some(status_code) = response_code.status_code() {
                match determine_metrics_action(status_code.as_u16(), is_multiline) {
                    MetricsAction::Article => {
                        self.metrics.record_article(backend_id, bytes_written);
                    }
                    MetricsAction::Error4xx => self.metrics.record_error_4xx(backend_id),
                    MetricsAction::Error5xx => self.metrics.record_error_5xx(backend_id),
                    MetricsAction::None => {}
                }
            }
        }

        // Connection healthy — conn drops here, returning to pool automatically
        guard.complete();
        Ok(())
    }

    /// Route a single command to a backend and execute it
    ///
    /// This function is `pub(super)` to allow reuse of per-command routing logic by sibling handler modules
    /// (such as `hybrid.rs`) that also need to route commands.
    pub(super) async fn route_and_execute_command(
        &self,
        router: Arc<BackendSelector>,
        command: &str,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        client_to_backend_bytes: &mut ClientToBackendBytes,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<crate::types::BackendId> {
        debug!(
            "Client {} ENTERED route_and_execute_command: {}",
            self.client_addr,
            command.trim()
        );
        // Extract message-ID early for cache/availability tracking
        let msg_id = crate::types::MessageId::extract_from_command_borrowed(command);

        debug!(
            "Client {} msg_id={:?}, cache_articles={}",
            self.client_addr, msg_id, self.cache_articles
        );

        // Try cache first - may return early if cache hit.
        // On partial hit, we get availability info to avoid a redundant cache.get() later.
        let cached_availability = match self
            .try_serve_from_cache(
                &msg_id,
                command,
                &router,
                client_write,
                backend_to_client_bytes,
            )
            .await?
        {
            CacheLookupResult::Hit(backend_id) => return Ok(backend_id),
            CacheLookupResult::PartialHit(availability) => Some(availability),
            CacheLookupResult::Miss => None,
        };

        // H3: Hoist availability loading before pipeline/retry branch (compute once, use in both paths)
        // O1: cached_availability already contains the result of cache.get() - reuse it
        let mut availability = cached_availability;

        // Adaptive prechecking for STAT/HEAD commands (if enabled and cache missed)
        if self.adaptive_precheck
            && let Some(ref msg_id_ref) = msg_id
        {
            // Use optimized command matching from classifier (direct byte comparison)
            let bytes = command.as_bytes();
            let is_head = is_head_command(bytes);
            if is_stat_command(bytes) || is_head {
                let deps = self.precheck_deps(&router);
                let bytes_written =
                    match precheck::precheck(&deps, command, msg_id_ref, is_head).await {
                        Some(entry) => {
                            let buf = entry.buffer();
                            client_write.write_all(buf).await?;
                            buf.len()
                        }
                        None => {
                            client_write
                                .write_all(crate::protocol::NO_SUCH_ARTICLE)
                                .await?;
                            crate::protocol::NO_SUCH_ARTICLE.len()
                        }
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
        let cmd_bytes = command.as_bytes();
        let is_large_transfer = is_large_transfer_command(cmd_bytes);

        // Try pipeline path: if the routed backend has a pipeline queue, enqueue
        // the command and await the response instead of acquiring a direct connection.
        // This allows N client sessions to share M backend connections (N >> M).
        // NOTE: Pipeline path uses route_command_with_availability to respect 430s
        if !is_large_transfer {
            // H3: Use pre-loaded availability (no redundant cache lookup)
            // Route with availability awareness (avoids backends that returned 430)
            if let Ok(backend_id) = router.route_command_with_availability(
                self.client_id,
                command,
                availability.as_ref(),
            ) && let Some(queue) = router.get_backend_queue(backend_id)
            {
                // Wrap in guard - decrements pending_count on all exit paths
                let guard = CommandGuard::new(router.clone(), backend_id);

                debug!(
                    "Client {} using pipeline path for backend {:?}: {}",
                    self.client_addr,
                    backend_id,
                    command.trim()
                );

                let (tx, rx) = tokio::sync::oneshot::channel();
                let request = QueuedRequest {
                    command: std::sync::Arc::from(command),
                    response_tx: tx,
                };

                // Enqueue with backpressure - fail fast if queue is full
                match queue.try_enqueue(request) {
                    Ok(()) => {
                        self.metrics.record_pipeline_enqueue();

                        // Await response from the pipeline worker
                        match rx.await {
                            Ok(PipelineResponse::Success {
                                data,
                                status_code,
                                backend_id,
                            }) if status_code.as_u16() != 430 => {
                                // Success - article found, return immediately
                                client_write.write_all(&data).await?;
                                *backend_to_client_bytes = backend_to_client_bytes.add(data.len());
                                *client_to_backend_bytes =
                                    client_to_backend_bytes.add(command.len());
                                self.metrics.record_pipeline_complete();
                                guard.complete();
                                return Ok(backend_id);
                            }
                            Ok(PipelineResponse::Success { backend_id, .. }) => {
                                // 430 (article not found) - fall through to retry loop
                                debug!(
                                    "Client {} pipeline got 430 from backend {:?}, falling through to retry loop",
                                    self.client_addr, backend_id
                                );
                                // O2: Record this backend as missing in local availability (no cache round-trip)
                                let avail = availability.get_or_insert_default();
                                avail.record_missing(backend_id);
                                self.metrics.record_pipeline_complete();
                                self.metrics.record_error_4xx(backend_id);
                                // Fall through - guard drops, decrementing pending_count
                            }
                            Ok(PipelineResponse::Error(e)) => {
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
            "Client {} starting availability routing for command: {}",
            self.client_addr,
            command.trim()
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
            match self
                .try_backend_for_article(
                    &router,
                    command,
                    msg_id.as_ref(),
                    client_write,
                    &mut availability,
                    &mut buffer,
                    client_to_backend_bytes,
                )
                .await
            {
                Ok(BackendAttemptResult::Success {
                    backend_id,
                    bytes_written,
                }) => {
                    *backend_to_client_bytes = backend_to_client_bytes.add(bytes_written as usize);
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
                Err(e) if is_client_disconnect_error(&e) => {
                    // Client disconnected - stop trying backends
                    debug!(
                        "Client {} disconnected, stopping retry loop",
                        self.client_addr
                    );
                    return Err(e);
                }
                Err(e) => {
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
