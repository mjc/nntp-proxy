//! Per-command routing mode handler and command execution
//!
//! This module implements independent per-command routing where each command
//! can be routed to a different backend. It includes the core command execution
//! logic used by all routing modes.

use crate::session::common;
use crate::session::metrics_ext::MetricsRecorder;
use crate::session::routing::{
    CacheAction, CommandRoutingDecision, MetricsAction, decide_command_routing,
    determine_cache_action, determine_metrics_action, is_430_status_code,
};
use crate::session::{ClientSession, backend, connection, precheck, streaming};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info};

use crate::command::{CommandAction, CommandHandler};
use crate::constants::buffer::{COMMAND, READER_CAPACITY};
use crate::router::BackendSelector;
use crate::types::{BackendId, BackendToClientBytes, ClientToBackendBytes, TransferMetrics};

/// Result of attempting to execute a command on a backend
enum BackendAttemptResult {
    /// Article found - response streamed successfully
    Success {
        backend_id: crate::types::BackendId,
        bytes_written: u64,
    },
    /// Article not found (430) - try next backend
    /// Note: The 430 response is read and drained, just not stored
    ArticleNotFound { backend_id: crate::types::BackendId },
    /// Backend unavailable or error - try next backend
    BackendUnavailable,
}

impl ClientSession {
    /// Handle a client connection with per-command routing
    /// Each command is routed independently to potentially different backends
    pub async fn handle_per_command_routing(
        &self,
        mut client_stream: TcpStream,
    ) -> Result<TransferMetrics> {
        use tokio::io::BufReader;

        let Some(router) = self.router.as_ref() else {
            anyhow::bail!("Per-command routing mode requires a router");
        };

        let (client_read, mut client_write) = client_stream.split();
        let mut client_reader = BufReader::with_capacity(READER_CAPACITY, client_read);

        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_to_client_bytes = BackendToClientBytes::zero();

        // Auth state: username from AUTHINFO USER command
        let mut auth_username: Option<String> = None;

        // NOTE: Greeting already sent by proxy.rs before session handler starts
        // This ensures clients get immediate response and avoids timing issues

        debug!("Client {} entering command loop", self.client_addr);

        // Reuse command buffer to avoid allocations per command
        let mut command = String::with_capacity(COMMAND);

        // PERFORMANCE: Cache authenticated state to avoid atomic loads after auth succeeds
        // If auth is disabled, skip checks from the start
        let mut skip_auth_check = !self.auth_handler.is_enabled();

        // Process commands one at a time
        loop {
            command.clear();

            debug!("Client {} waiting for command...", self.client_addr);
            let n = match client_reader.read_line(&mut command).await {
                Ok(0) => {
                    debug!("Client {} disconnected", self.client_addr);
                    break;
                }
                Ok(n) => {
                    debug!(
                        "Client {} received {} bytes: {:?}",
                        self.client_addr,
                        n,
                        command.trim()
                    );
                    n
                }
                Err(e) => {
                    connection::log_client_error(
                        self.client_addr,
                        self.username().as_deref(),
                        &e,
                        TransferMetrics {
                            client_to_backend: client_to_backend_bytes,
                            backend_to_client: backend_to_client_bytes,
                        },
                    );
                    break;
                }
            };

            client_to_backend_bytes = client_to_backend_bytes.add(n);
            let trimmed = command.trim();

            // Handle QUIT locally
            if let common::QuitStatus::Quit(bytes) =
                common::handle_quit_command(&command, &mut client_write).await?
            {
                backend_to_client_bytes = backend_to_client_bytes.add_u64(bytes.into());
                break;
            }

            skip_auth_check = self.is_authenticated_cached(skip_auth_check);

            let decision = decide_command_routing(
                &command,
                skip_auth_check,
                self.auth_handler.is_enabled(),
                self.mode_state.routing_mode(),
            );

            match decision {
                CommandRoutingDecision::InterceptAuth => {
                    debug!("Client {} decision: InterceptAuth", self.client_addr);
                    // Re-classify to extract auth action
                    let action = CommandHandler::classify(&command);
                    let auth_action = match action {
                        CommandAction::InterceptAuth(a) => a,
                        _ => unreachable!(
                            "InterceptAuth decision must come from InterceptAuth action"
                        ),
                    };

                    backend_to_client_bytes = backend_to_client_bytes.add_u64(
                        match common::handle_auth_command(
                            &self.auth_handler,
                            auth_action,
                            &mut client_write,
                            &mut auth_username,
                            &self.auth_state,
                        )
                        .await?
                        {
                            common::AuthResult::Authenticated(bytes) => {
                                common::on_authentication_success(
                                    self.client_addr,
                                    auth_username.clone(),
                                    &self.mode_state.routing_mode(),
                                    &self.metrics,
                                    self.connection_stats(),
                                    |username| self.set_username(username),
                                );
                                skip_auth_check = true;
                                bytes
                            }
                            common::AuthResult::NotAuthenticated(bytes) => bytes,
                        }
                        .as_u64(),
                    );
                }

                CommandRoutingDecision::Forward => {
                    debug!(
                        "Client {} decision: Forward ({})",
                        self.client_addr, trimmed
                    );
                    self.route_and_execute_command(
                        router.clone(),
                        &command,
                        &mut client_write,
                        &mut client_to_backend_bytes,
                        &mut backend_to_client_bytes,
                    )
                    .await?;
                }

                CommandRoutingDecision::RequireAuth => {
                    debug!("Client {} decision: RequireAuth", self.client_addr);
                    use crate::protocol::AUTH_REQUIRED_FOR_COMMAND;
                    client_write.write_all(AUTH_REQUIRED_FOR_COMMAND).await?;
                    backend_to_client_bytes =
                        backend_to_client_bytes.add(AUTH_REQUIRED_FOR_COMMAND.len());
                }

                CommandRoutingDecision::SwitchToStateful => {
                    debug!(
                        "Client {} decision: SwitchToStateful ({})",
                        self.client_addr, trimmed
                    );
                    info!(
                        "Client {} switching to stateful mode (command: {})",
                        self.client_addr, trimmed
                    );
                    return self
                        .switch_to_stateful_mode(
                            client_reader,
                            client_write,
                            &command,
                            client_to_backend_bytes.into(),
                            backend_to_client_bytes.into(),
                        )
                        .await;
                }

                CommandRoutingDecision::Reject => {
                    debug!("Client {} decision: Reject", self.client_addr);
                    // Re-classify to extract reject message
                    let action = CommandHandler::classify(&command);
                    let response = match action {
                        CommandAction::Reject(r) => r,
                        _ => unreachable!("Reject decision must come from Reject action"),
                    };
                    client_write.write_all(response.as_bytes()).await?;
                    backend_to_client_bytes = backend_to_client_bytes.add(response.len());
                }
            }
        }

        // Log session summary and close user connection
        if let Some(ref m) = self.metrics {
            m.user_connection_closed(self.username().as_deref());
        }

        Ok(TransferMetrics {
            client_to_backend: client_to_backend_bytes,
            backend_to_client: backend_to_client_bytes,
        })
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
        let msg_id = common::extract_message_id(command)
            .and_then(|s| crate::types::MessageId::from_borrowed(s).ok());

        debug!(
            "Client {} msg_id={:?}, cache_articles={}",
            self.client_addr, msg_id, self.cache_articles
        );

        // CRITICAL: Check cache FIRST before doing expensive backend queries
        // This must come before adaptive prechecking to avoid unnecessary backend load
        //
        // Cache serving logic:
        // - missing == 0: No backends tried yet → run precheck to find which backend has it
        // - missing != 0 with 430: At least one backend tried, all returned 430 → serve 430, run precheck
        // - missing != 0 with 2xx: At least one backend succeeded → serve 2xx, run precheck
        if let Some(msg_id_ref) = msg_id.as_ref() {
            debug!(
                "Client {} checking cache for {}",
                self.client_addr, msg_id_ref
            );
            match self.cache.get(msg_id_ref).await {
                Some(cached) if cached.has_availability_info() => {
                    debug!(
                        "Client {} cache HIT for {} (cache_articles={})",
                        self.client_addr, msg_id_ref, self.cache_articles
                    );

                    // Spawn background precheck for STAT to update availability
                    if self.adaptive_precheck && command.to_uppercase().starts_with("STAT ") {
                        precheck::spawn_background_precheck(
                            self.precheck_deps(&router),
                            command.to_string(),
                            msg_id_ref.to_owned(),
                        );
                    }

                    // If full article caching enabled, serve from cache
                    if self.cache_articles {
                        client_write.write_all(cached.response()).await?;
                        *backend_to_client_bytes =
                            backend_to_client_bytes.add(cached.response().len());

                        let backend_id = router.route_command(self.client_id, command)?;
                        router.complete_command(backend_id);
                        return Ok(backend_id);
                    }
                    // else: availability-only mode - fall through to use availability info for routing
                }
                Some(_cached) => {
                    debug!(
                        "Cache entry exists for {} but no availability info (missing=0) - running precheck",
                        msg_id_ref
                    );
                }
                None => {
                    debug!("Cache MISS for message-ID: {}", msg_id_ref);
                }
            }
        }

        // Adaptive prechecking for STAT/HEAD commands (if enabled and cache missed)
        if self.adaptive_precheck
            && let Some(ref msg_id_ref) = msg_id
        {
            let cmd_upper = command.to_uppercase();
            let is_stat = cmd_upper.starts_with("STAT ");
            let is_head = cmd_upper.starts_with("HEAD ");

            if is_stat || is_head {
                let deps = self.precheck_deps(&router);
                let response = match precheck::precheck(&deps, command, msg_id_ref, is_head).await {
                    Some(entry) => entry.buffer().to_vec(),
                    None => crate::protocol::NO_SUCH_ARTICLE.to_vec(),
                };
                client_write.write_all(&response).await?;
                *backend_to_client_bytes = backend_to_client_bytes.add(response.len());
                return Ok(BackendId::from_index(0));
            }
        }

        // Execute command with availability-aware backend selection
        debug!(
            "Client {} calling execute_with_availability_routing for command: {}",
            self.client_addr,
            command.trim()
        );
        self.execute_with_availability_routing(
            router,
            command,
            msg_id.as_ref(),
            client_write,
            client_to_backend_bytes,
            backend_to_client_bytes,
        )
        .await
    }

    /// Execute command with availability-aware backend selection
    ///
    /// Uses ArticleAvailability to intelligently select backends, automatically retrying
    /// on 430 responses across backends that haven't returned 430 for this article yet.
    async fn execute_with_availability_routing(
        &self,
        router: Arc<BackendSelector>,
        command: &str,
        msg_id: Option<&crate::types::MessageId<'_>>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        client_to_backend_bytes: &mut ClientToBackendBytes,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<crate::types::BackendId> {
        let mut buffer = self.buffer_pool.acquire().await;

        // Initialize availability tracker from cache
        let mut availability = self.load_article_availability(msg_id, router.clone()).await;
        debug!(
            "Client {} starting availability routing, missing_bits={:08b}, backend_count={}",
            self.client_addr,
            availability.missing_bits(),
            router.backend_count().get()
        );

        // EARLY RETURN: If cache says all backends already returned 430, don't waste time
        if availability.all_exhausted(router.backend_count()) {
            debug!(
                "Client {} cache shows all backends exhausted for {:?}, returning 430 immediately",
                self.client_addr, msg_id
            );
            self.send_430_to_client(client_write, backend_to_client_bytes)
                .await?;
            return Ok(crate::types::BackendId::from_index(0));
        }

        // Track last backend tried for metrics/return value
        let mut last_backend: Option<crate::types::BackendId> = None;

        // Try backends until success or exhaustion
        while !availability.all_exhausted(router.backend_count()) {
            match self
                .try_backend_for_article(
                    router.clone(),
                    command,
                    msg_id,
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
                    router.complete_command(backend_id);
                    // Sync availability to cache before returning (got a success, but we may have
                    // recorded some 430s from other backends along the way)
                    if let Some(msg_id_ref) = msg_id {
                        self.cache
                            .sync_availability(msg_id_ref.clone(), &availability)
                            .await;
                    }
                    return Ok(backend_id);
                }
                Ok(BackendAttemptResult::ArticleNotFound { backend_id }) => {
                    last_backend = Some(backend_id);
                }
                Ok(BackendAttemptResult::BackendUnavailable) => {
                    // Continue to next backend
                }
                Err(e) => {
                    // Check if client disconnected - if so, stop trying backends
                    if crate::session::error_classification::ErrorClassifier::is_client_disconnect(
                        &e,
                    ) {
                        debug!(
                            "Client {} disconnected, stopping retry loop",
                            self.client_addr
                        );
                        return Err(e);
                    }

                    // Backend error (timeout, connection error, etc.)
                    // Log it but continue trying other backends
                    debug!(
                        "Client {} backend error (will try next): {:?}",
                        self.client_addr, e
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
        if let Some(msg_id_ref) = msg_id {
            self.cache
                .sync_availability(msg_id_ref.clone(), &availability)
                .await;
        }
        self.send_430_to_client(client_write, backend_to_client_bytes)
            .await?;

        Ok(last_backend.unwrap_or_else(|| crate::types::BackendId::from_index(0)))
    }

    /// Load article availability from cache or create fresh tracker
    async fn load_article_availability(
        &self,
        msg_id: Option<&crate::types::MessageId<'_>>,
        router: Arc<BackendSelector>,
    ) -> crate::cache::ArticleAvailability {
        match msg_id {
            Some(msg_id_ref) => self
                .cache
                .get(msg_id_ref)
                .await
                .map(|entry| {
                    let avail = entry.to_availability(router.backend_count());
                    debug!("Client {} loaded availability for {}: checked_bits={:08b}, missing_bits={:08b}", 
                        self.client_addr, msg_id_ref, avail.checked_bits(), avail.missing_bits());
                    avail
                })
                .unwrap_or_default(),
            None => crate::cache::ArticleAvailability::new(),
        }
    }

    /// Try executing command on next available backend
    #[allow(clippy::too_many_arguments)]
    async fn try_backend_for_article(
        &self,
        router: Arc<BackendSelector>,
        command: &str,
        msg_id: Option<&crate::types::MessageId<'_>>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        availability: &mut crate::cache::ArticleAvailability,
        buffer: &mut crate::pool::PooledBuffer,
        client_to_backend_bytes: &mut ClientToBackendBytes,
    ) -> Result<BackendAttemptResult> {
        // Select least-loaded available backend
        let backend_id =
            router.route_command_with_availability(self.client_id, command, Some(availability))?;

        // Get connection provider
        let Some(provider) = router.backend_provider(backend_id) else {
            availability.record_missing(backend_id);
            router.complete_command(backend_id);
            return Ok(BackendAttemptResult::BackendUnavailable);
        };

        // Execute command
        let mut conn = provider.get_pooled_connection().await?;

        // Execute command without timeout
        let (n, response_code, is_multiline, ttfb, send, recv) = self
            .execute_and_get_first_chunk(&mut conn, backend_id, command, buffer)
            .await?;

        self.record_timing_metrics(backend_id, ttfb, send, recv);
        *client_to_backend_bytes = client_to_backend_bytes.add(command.len());

        // Handle 430 - article not found
        // Note: response is already read into buffer, keeping connection clean
        if self.is_430_response(&response_code) {
            self.handle_430_response(backend_id, router.clone(), availability);
            return Ok(BackendAttemptResult::ArticleNotFound { backend_id });
        }

        // Success - stream response
        let bytes_written = match self
            .stream_response_to_client(
                &mut conn,
                client_write,
                backend_id,
                command,
                msg_id,
                &response_code,
                is_multiline,
                &buffer[..n],
                n,
            )
            .await
        {
            Ok(bytes) => bytes,
            Err(e) => {
                // Only mark as backend error if it's NOT a client disconnect
                // Client disconnects should not penalize the backend
                if !crate::session::error_classification::ErrorClassifier::is_client_disconnect(&e)
                {
                    self.handle_backend_error(backend_id, &router);
                }
                crate::pool::remove_from_pool(conn);
                return Err(e);
            }
        };

        self.record_response_metrics(
            backend_id,
            &response_code,
            is_multiline,
            command.len() as u64,
            bytes_written,
        );

        Ok(BackendAttemptResult::Success {
            backend_id,
            bytes_written,
        })
    }

    /// Execute command on backend and read first chunk
    async fn execute_and_get_first_chunk(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        backend_id: crate::types::BackendId,
        command: &str,
        buffer: &mut crate::pool::PooledBuffer,
    ) -> Result<(usize, crate::protocol::NntpResponse, bool, u64, u64, u64)> {
        self.metrics.record_command(backend_id);
        self.metrics.user_command(self.username().as_deref());

        let (response, ttfb, send, recv) =
            backend::send_command_timed(&mut **pooled_conn, command, buffer).await?;

        // Log any validation warnings
        backend::log_warnings(
            &response.warnings,
            &buffer[..response.bytes_read],
            response.bytes_read,
            self.client_addr,
            backend_id,
        );

        Ok((
            response.bytes_read,
            response.response,
            response.is_multiline,
            ttfb,
            send,
            recv,
        ))
    }

    /// Check if response is 430 (article not found)
    fn is_430_response(&self, response_code: &crate::protocol::NntpResponse) -> bool {
        response_code
            .status_code()
            .is_some_and(|code| is_430_status_code(code.as_u16()))
    }

    /// Spawn async cache upsert task
    ///
    /// This is fire-and-forget - we don't wait for the cache to update.
    /// Used after successfully streaming a response to update availability tracking.
    fn spawn_cache_upsert(
        &self,
        msg_id: &crate::types::MessageId<'_>,
        buffer: Vec<u8>,
        backend_id: crate::types::BackendId,
    ) {
        let cache_clone = self.cache.clone();
        let msg_id_owned = msg_id.to_owned();
        tokio::spawn(async move {
            cache_clone.upsert(msg_id_owned, buffer, backend_id).await;
        });
    }

    /// Record timing metrics for a backend response
    fn record_timing_metrics(
        &self,
        backend_id: crate::types::BackendId,
        ttfb: u64,
        send: u64,
        recv: u64,
    ) {
        if let Some(ref metrics) = self.metrics {
            metrics.record_ttfb_micros(backend_id, ttfb);
            metrics.record_send_recv_micros(backend_id, send, recv);
        }
    }

    /// Handle backend error (metrics and cleanup)
    fn handle_backend_error(&self, backend_id: crate::types::BackendId, router: &BackendSelector) {
        if let Some(ref metrics) = self.metrics {
            metrics.record_error(backend_id);
            metrics.user_error(self.username().as_deref());
        }
        router.complete_command(backend_id);
    }

    /// Send standardized 430 response to client
    async fn send_430_to_client(
        &self,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<()> {
        client_write
            .write_all(crate::protocol::NO_SUCH_ARTICLE)
            .await?;
        *backend_to_client_bytes =
            backend_to_client_bytes.add(crate::protocol::NO_SUCH_ARTICLE.len());
        Ok(())
    }

    /// Handle 430 response - update local availability tracker
    ///
    /// Note: Cache sync happens ONCE at end of retry loop via sync_availability,
    /// not here. This avoids spawning async tasks for each 430.
    fn handle_430_response(
        &self,
        backend_id: crate::types::BackendId,
        router: Arc<BackendSelector>,
        availability: &mut crate::cache::ArticleAvailability,
    ) {
        availability.record_missing(backend_id);

        // NOTE: 430 is NOT an error - it's normal retry behavior.
        // The backend is correctly reporting it doesn't have the article.
        // Error metrics should only track actual errors (connection failures, protocol violations, etc.)

        router.complete_command(backend_id);
    }

    /// Stream response from backend to client and handle caching
    #[allow(clippy::too_many_arguments)]
    async fn stream_response_to_client(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_id: crate::types::BackendId,
        command: &str,
        msg_id: Option<&crate::types::MessageId<'_>>,
        response_code: &crate::protocol::NntpResponse,
        is_multiline: bool,
        first_chunk: &[u8],
        first_chunk_size: usize,
    ) -> Result<u64> {
        let code = response_code.status_code().map(|c| c.as_u16()).unwrap_or(0);
        let cache_action = determine_cache_action(
            command,
            code,
            is_multiline,
            self.cache_articles,
            msg_id.is_some(),
        );

        match (is_multiline, cache_action) {
            (true, CacheAction::CaptureArticle) => {
                let mut captured = Vec::with_capacity(first_chunk.len() * 2);
                let bytes = streaming::stream_and_capture_multiline_response(
                    &mut **pooled_conn,
                    client_write,
                    first_chunk,
                    first_chunk_size,
                    self.client_addr,
                    backend_id,
                    &self.buffer_pool,
                    &mut captured,
                )
                .await?;

                if let Some(msg_id_ref) = msg_id {
                    self.spawn_cache_upsert(msg_id_ref, captured, backend_id);
                }
                Ok(bytes)
            }
            (true, CacheAction::TrackAvailability) => {
                let bytes = streaming::stream_multiline_response(
                    &mut **pooled_conn,
                    client_write,
                    first_chunk,
                    first_chunk_size,
                    self.client_addr,
                    backend_id,
                    &self.buffer_pool,
                )
                .await?;

                if let Some(msg_id_ref) = msg_id {
                    self.spawn_cache_upsert(msg_id_ref, first_chunk.to_vec(), backend_id);
                }
                Ok(bytes)
            }
            (true, _) => {
                // Multiline but no caching
                streaming::stream_multiline_response(
                    &mut **pooled_conn,
                    client_write,
                    first_chunk,
                    first_chunk_size,
                    self.client_addr,
                    backend_id,
                    &self.buffer_pool,
                )
                .await
            }
            (false, CacheAction::TrackStat) => {
                client_write.write_all(first_chunk).await?;
                if let Some(msg_id_ref) = msg_id {
                    self.spawn_cache_upsert(msg_id_ref, b"223\r\n".to_vec(), backend_id);
                }
                Ok(first_chunk_size as u64)
            }
            (false, _) => {
                // Single-line, no caching
                client_write.write_all(first_chunk).await?;
                Ok(first_chunk_size as u64)
            }
        }
    }

    /// Record response metrics (errors, article sizes, command execution)
    fn record_response_metrics(
        &self,
        backend_id: crate::types::BackendId,
        response_code: &crate::protocol::NntpResponse,
        is_multiline: bool,
        cmd_bytes: u64,
        resp_bytes: u64,
    ) {
        use crate::types::MetricsBytes;

        let Some(ref metrics) = self.metrics else {
            return;
        };

        if let Some(code) = response_code.status_code() {
            match determine_metrics_action(code.as_u16(), is_multiline) {
                MetricsAction::Error4xx => metrics.record_error_4xx(backend_id),
                MetricsAction::Error5xx => metrics.record_error_5xx(backend_id),
                MetricsAction::Article => metrics.record_article(backend_id, resp_bytes),
                MetricsAction::None => {}
            }
        }

        let cmd_bytes_metric = MetricsBytes::new(cmd_bytes);
        let resp_bytes_metric = MetricsBytes::new(resp_bytes);
        let _ = metrics.record_command_execution(backend_id, cmd_bytes_metric, resp_bytes_metric);
        self.metrics
            .user_bytes_sent(self.username().as_deref(), cmd_bytes);
        self.metrics
            .user_bytes_received(self.username().as_deref(), resp_bytes);
    }

    /// Create precheck dependencies
    fn precheck_deps<'a>(&'a self, router: &'a Arc<BackendSelector>) -> precheck::PrecheckDeps<'a> {
        precheck::PrecheckDeps {
            router,
            cache: &self.cache,
            buffer_pool: &self.buffer_pool,
            metrics: self.metrics.as_ref(),
            cache_articles: self.cache_articles,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_client_disconnect_is_detected() {
        use std::io::ErrorKind;

        // Broken pipe should be detected as client disconnect
        let broken_pipe = std::io::Error::new(ErrorKind::BrokenPipe, "broken pipe");
        let err: anyhow::Error = broken_pipe.into();
        assert!(
            crate::session::error_classification::ErrorClassifier::is_client_disconnect(&err),
            "BrokenPipe should be classified as client disconnect"
        );

        // Timeout is not a client disconnect
        let timeout = std::io::Error::new(ErrorKind::TimedOut, "timed out");
        let err: anyhow::Error = timeout.into();
        assert!(
            !crate::session::error_classification::ErrorClassifier::is_client_disconnect(&err),
            "TimedOut should NOT be classified as client disconnect"
        );

        // Other errors are not client disconnects
        let other = std::io::Error::other("other error");
        let err: anyhow::Error = other.into();
        assert!(
            !crate::session::error_classification::ErrorClassifier::is_client_disconnect(&err),
            "Other errors should NOT be classified as client disconnect"
        );
    }
}
