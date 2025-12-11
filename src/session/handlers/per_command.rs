//! Per-command routing mode handler and command execution
//!
//! This module implements independent per-command routing where each command
//! can be routed to a different backend. It includes the core command execution
//! logic used by all routing modes.

use crate::session::common;
use crate::session::{ClientSession, backend, connection, precheck, streaming};
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
use crate::types::{BackendToClientBytes, ClientToBackendBytes, TransferMetrics};

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

/// Check if a status code represents a 430 (article not found) response
#[inline]
pub(super) fn is_430_status_code(code: u16) -> bool {
    code == 430
}

/// Check if a response should be captured for article caching
///
/// Only full article responses (220) should be cached.
/// Response code 220 uniquely identifies ARTICLE command success:
/// - 220 = ARTICLE (full article - cache this)
/// - 221 = HEAD (headers only)
/// - 222 = BODY (body only)
/// - 223 = STAT (status only)
pub(super) fn should_capture_for_cache(
    response_code: u16,
    is_multiline: bool,
    cache_articles: bool,
    has_message_id: bool,
) -> bool {
    cache_articles && is_multiline && has_message_id && response_code == 220
}

/// Check if a response should be tracked for availability (HEAD/BODY/ARTICLE/STAT success)
pub(super) fn should_track_availability(response_code: u16, has_message_id: bool) -> bool {
    has_message_id && matches!(response_code, 220..=223)
}

/// Determine what action to take for metrics recording based on response code
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MetricsAction {
    /// Record 4xx error (excluding 423, 430)
    Error4xx,
    /// Record 5xx error
    Error5xx,
    /// Record article metrics (success response for multiline)
    Article,
    /// No special recording needed
    None,
}

/// Determine metrics recording action based on response code
pub(super) fn determine_metrics_action(response_code: u16, is_multiline: bool) -> MetricsAction {
    if (400..500).contains(&response_code) && response_code != 423 && response_code != 430 {
        MetricsAction::Error4xx
    } else if response_code >= 500 {
        MetricsAction::Error5xx
    } else if is_multiline && matches!(response_code, 220..=222) {
        MetricsAction::Article
    } else {
        MetricsAction::None
    }
}

/// Determine what caching action to take for a response
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CacheAction {
    /// Capture full article content and cache it
    CaptureArticle,
    /// Track availability only (for HEAD/BODY/STAT success)
    TrackAvailability,
    /// Track STAT availability (223 response)
    TrackStat,
    /// No caching action needed
    None,
}

/// Determine caching action for a response
///
/// Response codes uniquely identify the command type:
/// - 220 = ARTICLE (cache full article if enabled)
/// - 221 = HEAD (track availability)
/// - 222 = BODY (track availability)
/// - 223 = STAT (track availability)
///
/// The command is validated to ensure it's not a stateful command
/// that would require mode switching (GROUP, NEXT, XOVER, etc.)
pub(super) fn determine_cache_action(
    command: &str,
    response_code: u16,
    is_multiline: bool,
    cache_articles: bool,
    has_message_id: bool,
) -> CacheAction {
    // Defensive check: don't cache responses from stateful commands
    // (These should have triggered mode switch, so this shouldn't happen)
    if NntpCommand::parse(command).is_stateful() {
        return CacheAction::None;
    }

    if !has_message_id {
        return CacheAction::None;
    }

    // Full article caching only for 220 response (ARTICLE command)
    if should_capture_for_cache(response_code, is_multiline, cache_articles, has_message_id) {
        CacheAction::CaptureArticle
    } else if is_multiline && should_track_availability(response_code, has_message_id) {
        // Track availability for HEAD (221), BODY (222), and ARTICLE (220) when cache disabled
        CacheAction::TrackAvailability
    } else if response_code == 223 {
        // STAT response - not multiline but track availability
        CacheAction::TrackStat
    } else {
        CacheAction::None
    }
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

                    // If full article caching enabled, serve from cache
                    if self.cache_articles {
                        client_write.write_all(cached.response()).await?;
                        *backend_to_client_bytes =
                            backend_to_client_bytes.add(cached.response().len());

                        // Spawn background precheck for STAT/HEAD to update cache
                        if self.adaptive_precheck {
                            let cmd_upper = command.to_uppercase();
                            if cmd_upper.starts_with("STAT ") {
                                precheck::spawn_background_stat_precheck(
                                    self.precheck_context(router.clone()),
                                    command.to_string(),
                                    msg_id_ref.to_owned(),
                                    self.client_addr,
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
                let ctx = self.precheck_context(router);
                return precheck::handle_stat_precheck(
                    &ctx,
                    self.client_id,
                    command,
                    msg_id_ref,
                    client_write,
                )
                .await;
            } else if cmd_upper.starts_with("HEAD ") {
                let ctx = self.precheck_context(router);
                return precheck::handle_head_precheck(
                    &ctx,
                    self.client_id,
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
            client_write
                .write_all(crate::protocol::NO_SUCH_ARTICLE)
                .await?;
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
        let timeout = precheck::calculate_adaptive_timeout(self.metrics.as_ref());
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
            .is_some_and(|code| is_430_status_code(code.as_u16()))
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
                    let cache_clone = self.cache.clone();
                    let msg_id_owned = msg_id_ref.to_owned();
                    tokio::spawn(async move {
                        cache_clone.upsert(msg_id_owned, captured, backend_id).await;
                    });
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
        self.user_bytes_sent(cmd_bytes);
        self.user_bytes_received(resp_bytes);
    }

    /// Create a PrecheckContext for precheck operations
    fn precheck_context(&self, router: Arc<BackendSelector>) -> precheck::PrecheckContext {
        precheck::PrecheckContext {
            router,
            cache: self.cache.clone(),
            buffer_pool: self.buffer_pool.clone(),
            metrics: self.metrics.clone(),
            cache_articles: self.cache_articles,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // Tests for is_430_status_code

    #[test]
    fn test_is_430_status_code() {
        assert!(is_430_status_code(430));
        assert!(!is_430_status_code(429));
        assert!(!is_430_status_code(431));
        assert!(!is_430_status_code(200));
        assert!(!is_430_status_code(500));
    }

    // Tests for should_capture_for_cache

    #[test]
    fn test_should_capture_for_cache_article_response() {
        // 220 response (ARTICLE) with all conditions met should capture
        assert!(should_capture_for_cache(220, true, true, true));

        // 221 (HEAD) and 222 (BODY) should NOT capture full article
        assert!(!should_capture_for_cache(221, true, true, true));
        assert!(!should_capture_for_cache(222, true, true, true));
    }

    #[test]
    fn test_should_capture_for_cache_requires_all_conditions() {
        // Not multiline
        assert!(!should_capture_for_cache(220, false, true, true));

        // Cache disabled
        assert!(!should_capture_for_cache(220, true, false, true));

        // No message-ID
        assert!(!should_capture_for_cache(220, true, true, false));

        // Wrong response code
        assert!(!should_capture_for_cache(430, true, true, true));
    }

    #[test]
    fn test_should_capture_for_cache_only_220() {
        // Only 220 (ARTICLE) responses should be captured
        assert!(should_capture_for_cache(220, true, true, true));
        assert!(!should_capture_for_cache(221, true, true, true)); // HEAD
        assert!(!should_capture_for_cache(222, true, true, true)); // BODY
        assert!(!should_capture_for_cache(223, true, true, true)); // STAT
    }

    // Tests for should_track_availability

    #[test]
    fn test_should_track_availability_success_responses() {
        assert!(should_track_availability(220, true)); // ARTICLE
        assert!(should_track_availability(221, true)); // HEAD
        assert!(should_track_availability(222, true)); // BODY
        assert!(should_track_availability(223, true)); // STAT
    }

    #[test]
    fn test_should_track_availability_requires_message_id() {
        assert!(!should_track_availability(220, false));
        assert!(!should_track_availability(223, false));
    }

    #[test]
    fn test_should_track_availability_error_responses() {
        assert!(!should_track_availability(430, true)); // Article not found
        assert!(!should_track_availability(500, true)); // Server error
        assert!(!should_track_availability(200, true)); // Greeting
    }

    // Tests for determine_metrics_action

    #[test]
    fn test_determine_metrics_action_4xx_errors() {
        // 4xx errors (excluding 423, 430) should record error_4xx
        assert_eq!(
            determine_metrics_action(400, false),
            MetricsAction::Error4xx
        );
        assert_eq!(
            determine_metrics_action(401, false),
            MetricsAction::Error4xx
        );
        assert_eq!(
            determine_metrics_action(480, false),
            MetricsAction::Error4xx
        );
    }

    #[test]
    fn test_determine_metrics_action_4xx_exclusions() {
        // 423 (no such article number) and 430 (no such article) are not errors
        assert_eq!(determine_metrics_action(423, false), MetricsAction::None);
        assert_eq!(determine_metrics_action(430, false), MetricsAction::None);
    }

    #[test]
    fn test_determine_metrics_action_5xx_errors() {
        assert_eq!(
            determine_metrics_action(500, false),
            MetricsAction::Error5xx
        );
        assert_eq!(
            determine_metrics_action(502, false),
            MetricsAction::Error5xx
        );
        assert_eq!(
            determine_metrics_action(503, false),
            MetricsAction::Error5xx
        );
    }

    #[test]
    fn test_determine_metrics_action_article_success() {
        // Multiline 220-222 should record article
        assert_eq!(determine_metrics_action(220, true), MetricsAction::Article);
        assert_eq!(determine_metrics_action(221, true), MetricsAction::Article);
        assert_eq!(determine_metrics_action(222, true), MetricsAction::Article);
    }

    #[test]
    fn test_determine_metrics_action_article_not_multiline() {
        // Non-multiline shouldn't record article
        assert_eq!(determine_metrics_action(220, false), MetricsAction::None);
    }

    #[test]
    fn test_determine_metrics_action_stat_not_article() {
        // STAT (223) is not an article even if multiline flag is true
        assert_eq!(determine_metrics_action(223, true), MetricsAction::None);
    }

    // Tests for determine_cache_action

    #[test]
    fn test_determine_cache_action_capture_article() {
        // Full article capture for 220 response when cache enabled
        assert_eq!(
            determine_cache_action("ARTICLE <test@example.com>", 220, true, true, true),
            CacheAction::CaptureArticle
        );
    }

    #[test]
    fn test_determine_cache_action_track_availability() {
        // Track availability for HEAD (221) and BODY (222) responses
        assert_eq!(
            determine_cache_action("HEAD <test@example.com>", 221, true, true, true),
            CacheAction::TrackAvailability
        );
        assert_eq!(
            determine_cache_action("BODY <test@example.com>", 222, true, true, true),
            CacheAction::TrackAvailability
        );
    }

    #[test]
    fn test_determine_cache_action_track_stat() {
        // Track STAT (223) - not multiline
        assert_eq!(
            determine_cache_action("STAT <test@example.com>", 223, false, false, true),
            CacheAction::TrackStat
        );
    }

    #[test]
    fn test_determine_cache_action_no_message_id() {
        // No caching without message-ID
        assert_eq!(
            determine_cache_action("ARTICLE 123", 220, true, true, false),
            CacheAction::None
        );
        assert_eq!(
            determine_cache_action("STAT 123", 223, false, false, false),
            CacheAction::None
        );
    }

    #[test]
    fn test_determine_cache_action_error_responses() {
        // No caching for error responses
        assert_eq!(
            determine_cache_action("ARTICLE <test@example.com>", 430, true, true, true),
            CacheAction::None
        );
        assert_eq!(
            determine_cache_action("ARTICLE <test@example.com>", 500, true, true, true),
            CacheAction::None
        );
    }

    #[test]
    fn test_determine_cache_action_cache_disabled() {
        // When cache_articles is false, don't capture full article but still track availability
        assert_eq!(
            determine_cache_action("ARTICLE <test@example.com>", 220, true, false, true),
            CacheAction::TrackAvailability
        );
    }

    #[test]
    fn test_determine_cache_action_rejects_stateful_commands() {
        // Stateful commands should never reach cache action determination,
        // but if they do, we defensively reject them
        assert_eq!(
            determine_cache_action("GROUP alt.test", 211, false, true, false),
            CacheAction::None
        );
        assert_eq!(
            determine_cache_action("XOVER 1-100", 224, true, true, true),
            CacheAction::None
        );
        assert_eq!(
            determine_cache_action("NEXT", 223, false, true, false),
            CacheAction::None
        );
    }
}
