//! Per-command routing mode handler and command execution
//!
//! This module implements independent per-command routing where each command
//! can be routed to a different backend. It includes the core command execution
//! logic used by all routing modes.

use crate::session::common;
use crate::session::{ClientSession, backend, connection, streaming};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::command::{CommandAction, CommandHandler, NntpCommand};
use crate::config::RoutingMode;
use crate::connection_error::ConnectionError;
use crate::constants::buffer::{COMMAND, READER_CAPACITY};
use crate::router::BackendSelector;
use crate::types::{BackendId, BackendToClientBytes, ClientToBackendBytes, TransferMetrics};

/// Calculate adaptive timeout based on average TTFB across backends
///
/// Returns TTFB³, with a minimum of 5 seconds.
/// This prevents slow backends from blocking while allowing reasonable TTFB detection.
fn calculate_adaptive_timeout(
    metrics: Option<&crate::metrics::MetricsCollector>,
) -> std::time::Duration {
    metrics
        .and_then(|m| {
            let snapshot = m.snapshot(None);
            let avg_ttfb_ms: f64 = snapshot
                .backend_stats
                .iter()
                .filter_map(|b| b.average_ttfb_ms())
                .sum::<f64>()
                / snapshot.backend_stats.len().max(1) as f64;

            (avg_ttfb_ms > 0.0).then(|| {
                let ttfb_cubed = (avg_ttfb_ms / 1000.0).powi(3);
                std::time::Duration::from_secs_f64(ttfb_cubed)
            })
        })
        .unwrap_or(std::time::Duration::from_secs(5))
        .max(std::time::Duration::from_secs(5))
}

/// Decision for how to handle a command in per-command routing mode
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CommandRoutingDecision {
    /// Intercept and handle authentication locally
    InterceptAuth,
    /// Forward command to backend (authenticated or auth disabled)
    Forward,
    /// Require authentication first
    RequireAuth,
    /// Switch to stateful mode (hybrid mode only)
    SwitchToStateful,
    /// Reject the command
    Reject,
}

/// Determine how to handle a command based on auth state and routing mode
///
/// This is a pure function that can be easily tested without I/O dependencies.
pub(super) fn decide_command_routing(
    command: &str,
    is_authenticated: bool,
    auth_enabled: bool,
    routing_mode: RoutingMode,
) -> CommandRoutingDecision {
    use CommandAction::*;

    // Classify the command
    let action = CommandHandler::classify(command);

    match action {
        // Auth commands - ALWAYS intercept
        InterceptAuth(_) => CommandRoutingDecision::InterceptAuth,

        // Stateless commands
        ForwardStateless => {
            if is_authenticated || !auth_enabled {
                CommandRoutingDecision::Forward
            } else {
                CommandRoutingDecision::RequireAuth
            }
        }

        // Rejected commands in hybrid mode with stateful command -> switch mode
        Reject(_)
            if routing_mode == RoutingMode::Hybrid && NntpCommand::parse(command).is_stateful() =>
        {
            CommandRoutingDecision::SwitchToStateful
        }

        // All other rejected commands
        Reject(_) => CommandRoutingDecision::Reject,
    }
}

/// Result of attempting to execute a command on a backend
enum BackendAttemptResult {
    /// Article found - response streamed successfully
    Success {
        backend_id: crate::types::BackendId,
        bytes_written: u64,
    },
    /// Article not found (430) - try next backend
    ArticleNotFound {
        backend_id: crate::types::BackendId,
        response: Vec<u8>,
    },
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
                            client_to_backend_bytes,
                            backend_to_client_bytes,
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

                    // If full article caching enabled, serve from cache
                    if self.cache_articles {
                        client_write.write_all(cached.response()).await?;
                        *backend_to_client_bytes =
                            backend_to_client_bytes.add(cached.response().len());

                        // Spawn background precheck for STAT/HEAD to update cache
                        if self.adaptive_precheck {
                            let cmd_upper = command.to_uppercase();
                            if cmd_upper.starts_with("STAT ") {
                                self.spawn_background_stat_precheck(
                                    router.clone(),
                                    command.to_string(),
                                    msg_id_ref.to_owned(),
                                );
                            }
                        }

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
            if cmd_upper.starts_with("STAT ") {
                return self
                    .handle_stat_precheck(router, command, msg_id_ref, client_write)
                    .await;
            } else if cmd_upper.starts_with("HEAD ") {
                return self
                    .handle_head_precheck(
                        router,
                        command,
                        msg_id_ref,
                        client_write,
                        backend_to_client_bytes,
                    )
                    .await;
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
            client_write.write_all(b"430 No such article\r\n").await?;
            *backend_to_client_bytes = backend_to_client_bytes.add(22);
            let backend_id = crate::types::BackendId::from_index(0);
            return Ok(backend_id);
        }

        // Track last 430 response to return if all backends fail
        let mut last_430_response: Option<Vec<u8>> = None;
        let mut last_430_backend: Option<crate::types::BackendId> = None;

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
                    return Ok(backend_id);
                }
                Ok(BackendAttemptResult::ArticleNotFound {
                    backend_id,
                    response,
                }) => {
                    last_430_response = Some(response);
                    last_430_backend = Some(backend_id);
                }
                Ok(BackendAttemptResult::BackendUnavailable) => {
                    // Continue to next backend
                }
                Err(e) => {
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

        // All backends exhausted - send final 430
        debug!(
            "Client {} all backends exhausted for {:?}, sending 430",
            self.client_addr, msg_id
        );
        self.send_final_430_response(client_write, backend_to_client_bytes, last_430_response)
            .await?;

        // Return the last backend we tried, or the first backend if none were tried
        // (shouldn't happen but handle gracefully)
        Ok(last_430_backend.unwrap_or_else(|| crate::types::BackendId::from_index(0)))
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

        // Apply timeout at this level so we can handle drain properly
        let timeout = calculate_adaptive_timeout(self.metrics.as_ref());
        // Execute with timeout - flatten the Result<Result<...>> with `?`
        let (n, response_code, is_multiline, ttfb, send, recv) = match tokio::time::timeout(
            timeout,
            self.execute_and_get_first_chunk(&mut conn, backend_id, command, buffer),
        )
        .await
        {
            Ok(result) => result?, // Flatten: propagate execution errors
            Err(_elapsed) => {
                // Timeout - spawn task to drain and return to pool (backend throttles new connections)
                self.handle_backend_error(backend_id, &router);

                // Spawn drain task - takes ownership of connection
                tokio::spawn(crate::pool::drain_connection_async(
                    conn,
                    self.buffer_pool.clone(),
                ));

                return Err(ConnectionError::BackendTimeout { timeout }.into());
            }
        };

        self.record_timing_metrics(backend_id, ttfb, send, recv);
        *client_to_backend_bytes = client_to_backend_bytes.add(command.len());

        // Handle 430 - article not found
        if self.is_430_response(&response_code) {
            let response_buffer = buffer[..n].to_vec();
            self.handle_430_response(
                backend_id,
                msg_id,
                router.clone(),
                availability,
                response_buffer.clone(),
            );
            return Ok(BackendAttemptResult::ArticleNotFound {
                backend_id,
                response: response_buffer,
            });
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
                self.handle_backend_error(backend_id, &router);
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
        use std::time::Instant;
        use tokio::io::AsyncWriteExt;

        self.record_command(backend_id);
        self.user_command();

        let start = Instant::now();

        // Send command
        let send_start = Instant::now();
        pooled_conn.as_mut().write_all(command.as_bytes()).await?;
        let send_elapsed = send_start.elapsed();

        // Read first chunk
        let recv_start = Instant::now();
        let n = buffer.read_from(&mut **pooled_conn).await?;
        if n == 0 {
            anyhow::bail!("Backend connection closed unexpectedly");
        }
        let recv_elapsed = recv_start.elapsed();

        // Validate response using pure function from backend module
        let validated = backend::validate_backend_response(
            &buffer[..n],
            n,
            crate::protocol::MIN_RESPONSE_LENGTH,
        );

        // Log warnings
        for warning in &validated.warnings {
            use backend::ResponseWarning;
            match warning {
                ResponseWarning::ShortResponse { bytes, min } => {
                    warn!(
                        "Client {} got short response from backend {:?} ({} bytes < {} min): {:02x?}",
                        self.client_addr,
                        backend_id,
                        bytes,
                        min,
                        &buffer[..n]
                    );
                }
                ResponseWarning::InvalidResponse => {
                    warn!(
                        "Client {} got invalid response from backend {:?} ({} bytes): {:?}",
                        self.client_addr,
                        backend_id,
                        n,
                        String::from_utf8_lossy(&buffer[..n.min(50)])
                    );
                }
                ResponseWarning::UnusualStatusCode(code) => {
                    warn!(
                        "Client {} got unusual status code {} from backend {:?}: {:?}",
                        self.client_addr,
                        code,
                        backend_id,
                        String::from_utf8_lossy(&buffer[..n.min(50)])
                    );
                }
            }
        }

        let elapsed = start.elapsed();

        Ok((
            n,
            validated.response,
            validated.is_multiline,
            elapsed.as_micros() as u64,
            send_elapsed.as_micros() as u64,
            recv_elapsed.as_micros() as u64,
        ))
    }

    /// Check if response is 430 (article not found)
    fn is_430_response(&self, response_code: &crate::protocol::NntpResponse) -> bool {
        response_code
            .status_code()
            .is_some_and(|code| code.as_u16() == 430)
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

    /// Send final 430 response to client when all backends fail
    async fn send_final_430_response(
        &self,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_to_client_bytes: &mut BackendToClientBytes,
        last_430_response: Option<Vec<u8>>,
    ) -> Result<()> {
        if let Some(response) = last_430_response {
            client_write.write_all(&response).await?;
            *backend_to_client_bytes = backend_to_client_bytes.add(response.len());
        }
        Ok(())
    }

    /// Handle 430 response and prepare for retry
    fn handle_430_response(
        &self,
        backend_id: crate::types::BackendId,
        msg_id: Option<&crate::types::MessageId<'_>>,
        router: Arc<BackendSelector>,
        availability: &mut crate::cache::ArticleAvailability,
        response_buffer: Vec<u8>,
    ) {
        availability.record_missing(backend_id);

        if let Some(msg_id_ref) = msg_id {
            let cache_clone = self.cache.clone();
            let msg_id_owned = msg_id_ref.to_owned();
            tokio::spawn(async move {
                cache_clone
                    .record_backend_missing(msg_id_owned, backend_id, response_buffer)
                    .await;
            });
        }

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
        let should_capture = self.cache_articles
            && is_multiline
            && matches!(NntpCommand::parse(command), NntpCommand::ArticleByMessageId)
            && msg_id.is_some()
            && response_code
                .status_code()
                .is_some_and(|c| matches!(c.as_u16(), 220..=222));

        if is_multiline {
            if should_capture {
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
                    let cache_clone = self.cache.clone();
                    let msg_id_owned = msg_id_ref.to_owned();
                    tokio::spawn(async move {
                        cache_clone.upsert(msg_id_owned, captured, backend_id).await;
                    });
                }
                Ok(bytes)
            } else {
                let should_track = msg_id.is_some()
                    && response_code
                        .status_code()
                        .is_some_and(|c| matches!(c.as_u16(), 220..=223));

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

                if should_track && let Some(msg_id_ref) = msg_id {
                    let cache_clone = self.cache.clone();
                    let msg_id_owned = msg_id_ref.to_owned();
                    let buffer_for_cache = first_chunk.to_vec();
                    tokio::spawn(async move {
                        cache_clone
                            .upsert(msg_id_owned, buffer_for_cache, backend_id)
                            .await;
                    });
                }
                Ok(bytes)
            }
        } else {
            let should_track = msg_id.is_some()
                && response_code
                    .status_code()
                    .is_some_and(|c| c.as_u16() == 223);

            client_write.write_all(first_chunk).await?;

            if should_track && let Some(msg_id_ref) = msg_id {
                let cache_clone = self.cache.clone();
                let msg_id_owned = msg_id_ref.to_owned();
                let buffer_for_cache = b"223\r\n".to_vec();
                tokio::spawn(async move {
                    cache_clone
                        .upsert(msg_id_owned, buffer_for_cache, backend_id)
                        .await;
                });
            }
            Ok(first_chunk_size as u64)
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
            let raw_code = code.as_u16();

            if (400..500).contains(&raw_code) && raw_code != 423 && raw_code != 430 {
                metrics.record_error_4xx(backend_id);
            } else if raw_code >= 500 {
                metrics.record_error_5xx(backend_id);
            }

            if is_multiline && matches!(raw_code, 220..=222) {
                metrics.record_article(backend_id, resp_bytes);
            }
        }

        let cmd_bytes_metric = MetricsBytes::new(cmd_bytes);
        let resp_bytes_metric = MetricsBytes::new(resp_bytes);
        let _ = metrics.record_command_execution(backend_id, cmd_bytes_metric, resp_bytes_metric);
        self.user_bytes_sent(cmd_bytes);
        self.user_bytes_received(resp_bytes);
    }

    /// Execute a command on a given backend connection (stateful mode helper)
    ///
    /// This is a simplified version for stateful/hybrid mode that doesn't handle routing or retries.
    /// It just executes the command on the provided connection and streams the response.
    ///
    /// Returns `(Result<()>, got_backend_data, cmd_bytes, resp_bytes)` where:
    /// - `got_backend_data = true` means we successfully read from backend before any error
    /// - `cmd_bytes`: Unrecorded command bytes (MUST be recorded by caller)
    /// - `resp_bytes`: Unrecorded response bytes (MUST be recorded by caller)
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_command_on_backend(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        command: &str,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_id: crate::types::BackendId,
        client_to_backend_bytes: &mut ClientToBackendBytes,
        backend_to_client_bytes: &mut BackendToClientBytes,
        chunk_buffer: &mut crate::pool::PooledBuffer,
    ) -> (
        Result<()>,
        bool,
        crate::types::MetricsBytes<crate::types::Unrecorded>,
        crate::types::MetricsBytes<crate::types::Unrecorded>,
    ) {
        use crate::types::MetricsBytes;

        // Execute command and get first chunk
        let (n, response_code, is_multiline, ttfb_micros, send_micros, recv_micros) =
            match backend::send_command_and_read_first_chunk(
                &mut **pooled_conn,
                command,
                backend_id,
                self.client_addr,
                chunk_buffer,
            )
            .await
            {
                Ok(result) => result,
                Err(e) => {
                    return (Err(e), false, MetricsBytes::new(0), MetricsBytes::new(0));
                }
            };

        // Record timing metrics
        self.record_timing_metrics(backend_id, ttfb_micros, send_micros, recv_micros);

        *client_to_backend_bytes = client_to_backend_bytes.add(command.len());

        // Extract message-ID for caching
        let msg_id = common::extract_message_id(command)
            .and_then(|s| crate::types::MessageId::from_borrowed(s).ok());

        // Stream response using shared logic
        let bytes_written = match self
            .stream_response_to_client(
                pooled_conn,
                client_write,
                backend_id,
                command,
                msg_id.as_ref(),
                &response_code,
                is_multiline,
                &chunk_buffer[..n],
                n,
            )
            .await
        {
            Ok(bytes) => bytes,
            Err(e) => {
                return (
                    Err(e),
                    true,
                    MetricsBytes::new(command.len() as u64),
                    MetricsBytes::new(0),
                );
            }
        };

        *backend_to_client_bytes = backend_to_client_bytes.add(bytes_written as usize);

        // Record response metrics
        self.record_response_metrics(
            backend_id,
            &response_code,
            is_multiline,
            command.len() as u64,
            bytes_written,
        );

        // Return unrecorded metrics bytes
        let cmd_bytes = MetricsBytes::new(command.len() as u64);
        let resp_bytes = MetricsBytes::new(bytes_written);

        (Ok(()), true, cmd_bytes, resp_bytes)
    }

    /// Spawn background STAT precheck to update cache (called after cache hit)
    ///
    /// Only checks backends that haven't been tried yet (not marked as missing or having).
    /// This keeps cache fresh while avoiding duplicate queries to backends we already know about.
    fn spawn_background_stat_precheck(
        &self,
        router: Arc<BackendSelector>,
        command: String,
        msg_id: crate::types::MessageId<'static>,
    ) {
        let cache = self.cache.clone();
        let buffer_pool = self.buffer_pool.clone();
        let metrics = self.metrics.clone();
        let client_addr = self.client_addr;

        tokio::spawn(async move {
            // Get current cache entry to see which backends to check
            let backends_to_check: Vec<_> = if let Some(entry) = cache.get(&msg_id).await {
                // Only check backends we haven't tried yet
                entry.available_backends(router.backend_count())
            } else {
                // Cache was evicted, check all backends
                (0..router.backend_count().get())
                    .map(crate::types::BackendId::from_index)
                    .collect()
            };

            // Check only untried backends
            for backend_id in backends_to_check {
                let provider = match router.backend_provider(backend_id) {
                    Some(p) => p,
                    None => continue,
                };

                let mut conn = match provider.get_pooled_connection().await {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                let mut buffer = buffer_pool.acquire().await;

                let result = backend::send_command_and_read_first_chunk(
                    &mut *conn,
                    &command,
                    backend_id,
                    client_addr,
                    &mut buffer,
                )
                .await;

                if let Ok((n, response_code, _, _, _, _)) = result
                    && let Some(code_num) = response_code.status_code().map(|c| c.as_u16())
                {
                    let response = buffer[..n].to_vec();

                    match code_num {
                        430 => {
                            cache
                                .record_backend_missing(msg_id.clone(), backend_id, response)
                                .await;
                            if let Some(ref m) = metrics {
                                m.record_command(backend_id);
                                m.record_error_4xx(backend_id);
                            }
                        }
                        220..=223 => {
                            cache.upsert(msg_id.clone(), response, backend_id).await;
                        }
                        _ => {}
                    }
                }
            }
        });
    }

    /// Handle STAT command with adaptive prechecking
    ///
    /// Returns first successful response to client while continuing to check
    /// remaining backends in background for cache population.
    /// Query all backends concurrently for article availability (STAT command)
    ///
    /// This eliminates cache race conditions by:
    /// 1. Spawning concurrent STAT queries to ALL backends
    /// 2. Waiting for ALL queries to complete
    /// 3. Updating cache serially with all results
    /// 4. Returning first successful response to client
    async fn handle_stat_precheck(
        &self,
        router: Arc<BackendSelector>,
        command: &str,
        msg_id: &crate::types::MessageId<'_>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
    ) -> Result<crate::types::BackendId> {
        enum StatResult {
            Has(BackendId, Vec<u8>),     // 220-223: Article exists
            Missing(BackendId, Vec<u8>), // 430 or other error
        }

        // Calculate adaptive timeout: TTFB * 2
        let timeout = calculate_adaptive_timeout(self.metrics.as_ref());

        // Query all backends concurrently
        let backend_queries = (0..router.backend_count().get())
            .map(BackendId::from_index)
            .map(|backend_id| {
                let (router, buffer_pool, metrics, command) = (
                    router.clone(),
                    self.buffer_pool.clone(),
                    self.metrics.clone(),
                    command.to_string(),
                );

                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;

                    // Get connection and send STAT command (no timeout on write)
                    let provider = router.backend_provider(backend_id)?;
                    let mut conn = provider.get_pooled_connection().await.ok()?;
                    let mut buffer = buffer_pool.acquire().await;

                    // Send command without timeout
                    conn.as_mut().write_all(command.as_bytes()).await.ok()?;

                    // Timeout only on first byte read (TTFB²)
                    let read_result = tokio::time::timeout(timeout, async {
                        let n = buffer.read_from(&mut *conn).await.ok()?;
                        if n == 0 {
                            return None;
                        }
                        Some((n, buffer[..n].to_vec()))
                    })
                    .await;

                    let (_bytes_read, response_bytes) = match read_result {
                        Ok(Some(data)) => data,
                        Ok(None) => return None,
                        Err(_) => {
                            debug!(
                                "STAT query to backend {} timed out waiting for first byte after {:?}",
                                backend_id.as_index(),
                                timeout
                            );
                            return None;
                        }
                    };

                    // Parse response code
                    let response_code = crate::protocol::NntpResponse::parse(&response_bytes);
                    let status_code = response_code.status_code()?.as_u16();

                    // Classify result: has article, or missing/error
                    Some(match status_code {
                        220..=223 => StatResult::Has(backend_id, response_bytes),
                        430 => {
                            if let Some(m) = metrics.as_ref() {
                                m.record_command(backend_id);
                                m.record_error_4xx(backend_id);
                            }
                            StatResult::Missing(backend_id, response_bytes)
                        }
                        _ => StatResult::Missing(backend_id, response_bytes),
                    })
                })
            })
            .collect::<Vec<_>>();

        // Wait for all backends to respond
        let all_results = futures::future::try_join_all(backend_queries)
            .await
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();

        // Update cache serially (all backends reported, no races)
        let first_success = all_results.iter().find_map(|result| match result {
            StatResult::Has(backend_id, response) => Some((*backend_id, response.clone())),
            _ => None,
        });

        for result in all_results {
            match result {
                StatResult::Has(backend_id, response) => {
                    self.cache
                        .upsert(msg_id.to_owned(), response, backend_id)
                        .await;
                }
                StatResult::Missing(backend_id, response) => {
                    self.cache
                        .record_backend_missing(msg_id.to_owned(), backend_id, response)
                        .await;
                }
            }
        }

        // Send response to client
        match first_success {
            Some((backend_id, response)) => {
                client_write.write_all(&response).await?;
                router.complete_command(backend_id);
                Ok(backend_id)
            }
            None => {
                // All backends returned 430 or failed
                client_write.write_all(b"430 No such article\r\n").await?;
                let backend_id = router.route_command(self.client_id, command)?;
                router.complete_command(backend_id);
                Ok(backend_id)
            }
        }
    }

    /// Query all backends concurrently for article headers (HEAD command)
    ///
    /// Same pattern as STAT precheck but handles multiline responses.
    /// Optionally caches full headers if cache_articles=true.
    async fn handle_head_precheck(
        &self,
        router: Arc<BackendSelector>,
        command: &str,
        msg_id: &crate::types::MessageId<'_>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<crate::types::BackendId> {
        enum HeadResult {
            Has(BackendId, Vec<u8>),     // 221: Headers retrieved
            Missing(BackendId, Vec<u8>), // 430 or other error
        }

        // Calculate adaptive timeout: TTFB * 2
        let timeout = calculate_adaptive_timeout(self.metrics.as_ref());

        // Query all backends concurrently
        let backend_queries = (0..router.backend_count().get())
            .map(BackendId::from_index)
            .map(|backend_id| {
                let (router, buffer_pool, metrics, command, cache_articles) = (
                    router.clone(),
                    self.buffer_pool.clone(),
                    self.metrics.clone(),
                    command.to_string(),
                    self.cache_articles,
                );

                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};

                    // Get connection and send HEAD command (no timeout on write)
                    let provider = router.backend_provider(backend_id)?;
                    let mut conn = provider.get_pooled_connection().await.ok()?;
                    let mut buffer = buffer_pool.acquire().await;

                    // Send command without timeout
                    conn.as_mut().write_all(command.as_bytes()).await.ok()?;

                    // Timeout only on first byte read (TTFB²)
                    let read_result = tokio::time::timeout(timeout, async {
                        let n = buffer.read_from(&mut *conn).await.ok()?;
                        if n == 0 {
                            return None;
                        }
                        Some((n, buffer[..n].to_vec()))
                    })
                    .await;

                    let (_bytes_read, mut response_bytes) = match read_result {
                        Ok(Some(data)) => data,
                        Ok(None) => return None,
                        Err(_) => {
                            debug!(
                                "HEAD query to backend {} timed out waiting for first byte after {:?}",
                                backend_id.as_index(),
                                timeout
                            );
                            return None;
                        }
                    };

                    // Parse response code
                    let response_code = crate::protocol::NntpResponse::parse(&response_bytes);
                    let status_code = response_code.status_code()?.as_u16();
                    let is_multiline = response_code.is_multiline();

                    Some(match status_code {
                        221 => {
                            // Read complete multiline response if needed (no timeout - we got first byte)
                            let full_response = if is_multiline {
                                loop {
                                    let n = conn
                                        .as_mut()
                                        .read(buffer.as_mut_slice())
                                        .await
                                        .unwrap_or(0);
                                    if n == 0 || response_bytes.ends_with(b".\r\n") {
                                        break;
                                    }
                                    response_bytes.extend_from_slice(&buffer[..n]);
                                }
                                response_bytes
                            } else {
                                response_bytes
                            };

                            // Cache full headers or just availability marker
                            let cache_data = if cache_articles {
                                full_response
                            } else {
                                b"221\r\n".to_vec() // Minimal availability marker
                            };

                            HeadResult::Has(backend_id, cache_data)
                        }
                        430 => {
                            if let Some(m) = metrics.as_ref() {
                                m.record_command(backend_id);
                                m.record_error_4xx(backend_id);
                            }
                            HeadResult::Missing(backend_id, response_bytes)
                        }
                        _ => HeadResult::Missing(backend_id, response_bytes),
                    })
                })
            })
            .collect::<Vec<_>>();

        // Wait for all backends to respond
        let all_results = futures::future::try_join_all(backend_queries)
            .await
            .unwrap_or_default()
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();

        // Update cache serially (all backends reported, no races)
        let first_success = all_results.iter().find_map(|result| match result {
            HeadResult::Has(backend_id, response) => Some((*backend_id, response.clone())),
            _ => None,
        });

        for result in all_results {
            match result {
                HeadResult::Has(backend_id, cache_data) => {
                    self.cache
                        .upsert(msg_id.to_owned(), cache_data, backend_id)
                        .await;
                }
                HeadResult::Missing(backend_id, response) => {
                    self.cache
                        .record_backend_missing(msg_id.to_owned(), backend_id, response)
                        .await;
                }
            }
        }

        // Send response to client
        match first_success {
            Some((backend_id, response)) => {
                client_write.write_all(&response).await?;
                *backend_to_client_bytes = backend_to_client_bytes.add(response.len());
                router.complete_command(backend_id);
                Ok(backend_id)
            }
            None => {
                // All backends returned 430 or failed
                let response = b"430 No such article\r\n";
                client_write.write_all(response).await?;
                *backend_to_client_bytes = backend_to_client_bytes.add(response.len());
                let backend_id = router.route_command(self.client_id, command)?;
                router.complete_command(backend_id);
                Ok(backend_id)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_message_id_valid() {
        let command = "BODY <test@example.com>";
        let msgid = common::extract_message_id(command);
        assert_eq!(msgid, Some("<test@example.com>"));
    }

    #[test]
    fn test_extract_message_id_article_command() {
        let command = "ARTICLE <1234@news.server>";
        let msgid = common::extract_message_id(command);
        assert_eq!(msgid, Some("<1234@news.server>"));
    }

    #[test]
    fn test_extract_message_id_head_command() {
        let command = "HEAD <article@host.domain>";
        let msgid = common::extract_message_id(command);
        assert_eq!(msgid, Some("<article@host.domain>"));
    }

    #[test]
    fn test_extract_message_id_stat_command() {
        let command = "STAT <msg@example.org>";
        let msgid = common::extract_message_id(command);
        assert_eq!(msgid, Some("<msg@example.org>"));
    }

    #[test]
    fn test_extract_message_id_no_brackets() {
        let command = "GROUP comp.lang.rust";
        let msgid = common::extract_message_id(command);
        assert_eq!(msgid, None);
    }

    #[test]
    fn test_extract_message_id_malformed() {
        let command = "BODY <incomplete";
        let msgid = common::extract_message_id(command);
        assert_eq!(msgid, None);
    }

    #[test]
    fn test_extract_message_id_with_extra_text() {
        let command = "BODY <msg@host> extra stuff";
        let msgid = common::extract_message_id(command);
        assert_eq!(msgid, Some("<msg@host>"));
    }

    #[test]
    fn test_extract_message_id_empty_brackets() {
        let command = "BODY <>";
        let msgid = common::extract_message_id(command);
        assert_eq!(msgid, Some("<>"));
    }

    #[test]
    fn test_extract_message_id_lowercase_command() {
        let command = "body <test@example.com>";
        let msgid = common::extract_message_id(command);
        assert_eq!(msgid, Some("<test@example.com>"));
    }

    #[test]
    fn test_extract_message_id_mixed_case() {
        let command = "BoDy <TeSt@ExAmPlE.cOm>";
        let msgid = common::extract_message_id(command);
        assert_eq!(msgid, Some("<TeSt@ExAmPlE.cOm>"));
    }

    // Tests for decide_command_routing pure function

    #[test]
    fn test_decide_routing_auth_commands_always_intercepted() {
        // Auth commands should always be intercepted regardless of other flags
        assert_eq!(
            decide_command_routing("AUTHINFO USER test", true, true, RoutingMode::PerCommand,),
            CommandRoutingDecision::InterceptAuth
        );
        assert_eq!(
            decide_command_routing("AUTHINFO USER test", false, true, RoutingMode::PerCommand,),
            CommandRoutingDecision::InterceptAuth
        );
        assert_eq!(
            decide_command_routing("AUTHINFO USER test", false, false, RoutingMode::Stateful,),
            CommandRoutingDecision::InterceptAuth
        );
    }

    #[test]
    fn test_decide_routing_forward_when_authenticated() {
        // Should forward when authenticated, regardless of auth_enabled
        assert_eq!(
            decide_command_routing("LIST", true, true, RoutingMode::PerCommand),
            CommandRoutingDecision::Forward
        );
        assert_eq!(
            decide_command_routing("LIST", true, false, RoutingMode::PerCommand),
            CommandRoutingDecision::Forward
        );
    }

    #[test]
    fn test_decide_routing_forward_when_auth_disabled() {
        // Should forward when auth is disabled, even if not authenticated
        assert_eq!(
            decide_command_routing("LIST", false, false, RoutingMode::PerCommand,),
            CommandRoutingDecision::Forward
        );
    }

    #[test]
    fn test_decide_routing_require_auth_when_needed() {
        // Should require auth when auth is enabled but not authenticated
        assert_eq!(
            decide_command_routing("LIST", false, true, RoutingMode::PerCommand),
            CommandRoutingDecision::RequireAuth
        );
    }

    #[test]
    fn test_decide_routing_switch_to_stateful_in_hybrid_mode() {
        // Hybrid mode with stateful command should switch to stateful
        assert_eq!(
            decide_command_routing("GROUP alt.test", true, false, RoutingMode::Hybrid,),
            CommandRoutingDecision::SwitchToStateful
        );

        // Also works when not authenticated
        assert_eq!(
            decide_command_routing("XOVER 1-100", false, false, RoutingMode::Hybrid,),
            CommandRoutingDecision::SwitchToStateful
        );
    }

    #[test]
    fn test_decide_routing_reject_in_per_command_mode() {
        // Per-command mode should reject stateful commands
        assert_eq!(
            decide_command_routing("GROUP alt.test", true, false, RoutingMode::PerCommand,),
            CommandRoutingDecision::Reject
        );
    }

    #[test]
    fn test_decide_routing_reject_in_stateful_mode() {
        // Stateful mode should reject non-routable commands
        assert_eq!(
            decide_command_routing("POST", true, false, RoutingMode::Stateful),
            CommandRoutingDecision::Reject
        );
    }

    #[test]
    fn test_decide_routing_hybrid_mode_stateless_forwarded() {
        // Hybrid mode with stateless command should forward
        assert_eq!(
            decide_command_routing("LIST", true, false, RoutingMode::Hybrid),
            CommandRoutingDecision::Forward
        );
    }

    #[test]
    fn test_decide_routing_hybrid_mode_reject_non_stateful() {
        // Hybrid mode with rejected but non-stateful command (like POST) should just reject
        assert_eq!(
            decide_command_routing("POST", true, false, RoutingMode::Hybrid),
            CommandRoutingDecision::Reject
        );
    }

    #[test]
    fn test_decide_routing_all_modes_with_stateful_commands() {
        let stateful_commands = vec!["GROUP alt.test", "NEXT", "LAST", "XOVER 1-100"];

        for cmd in stateful_commands {
            // Hybrid mode: switch to stateful
            assert_eq!(
                decide_command_routing(cmd, true, false, RoutingMode::Hybrid),
                CommandRoutingDecision::SwitchToStateful,
                "Failed for command: {}",
                cmd
            );

            // Per-command mode: reject
            assert_eq!(
                decide_command_routing(cmd, true, false, RoutingMode::PerCommand),
                CommandRoutingDecision::Reject,
                "Failed for command: {}",
                cmd
            );

            // Stateful mode: reject (though shouldn't reach this in practice)
            assert_eq!(
                decide_command_routing(cmd, true, false, RoutingMode::Stateful),
                CommandRoutingDecision::Reject,
                "Failed for command: {}",
                cmd
            );
        }
    }

    #[test]
    fn test_decide_routing_auth_flow_progression() {
        // Step 1: Not authenticated, auth enabled -> require auth
        assert_eq!(
            decide_command_routing("LIST", false, true, RoutingMode::PerCommand,),
            CommandRoutingDecision::RequireAuth
        );

        // Step 2: Authenticated, auth enabled -> forward
        assert_eq!(
            decide_command_routing("LIST", true, true, RoutingMode::PerCommand,),
            CommandRoutingDecision::Forward
        );
    }
}
