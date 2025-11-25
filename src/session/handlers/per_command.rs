//! Per-command routing mode handler and command execution
//!
//! This module implements independent per-command routing where each command
//! can be routed to a different backend. It includes the core command execution
//! logic used by all routing modes.

use crate::pool::{ConnectionProvider, PooledBuffer};
use crate::session::common;
use crate::session::{ClientSession, backend, connection, streaming};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::command::{CommandAction, CommandHandler, NntpCommand};
use crate::config::RoutingMode;
use crate::constants::buffer::{COMMAND, READER_CAPACITY};
use crate::protocol::PROXY_GREETING_PCR;
use crate::router::BackendSelector;
use crate::types::{BytesTransferred, TransferMetrics};

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

/// Determine how to handle a command based on action, auth state, and routing mode
///
/// This is a pure function that can be easily tested without I/O dependencies.
pub(super) fn decide_command_routing(
    action: CommandAction,
    is_authenticated: bool,
    auth_enabled: bool,
    routing_mode: RoutingMode,
    command: &str,
) -> CommandRoutingDecision {
    use CommandAction::*;

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

impl ClientSession {
    /// Handle a client connection with per-command routing
    /// Each command is routed independently to potentially different backends
    pub async fn handle_per_command_routing(
        &self,
        mut client_stream: TcpStream,
    ) -> Result<TransferMetrics> {
        use tokio::io::BufReader;

        debug!(
            "Client {} starting per-command routing session",
            self.client_addr
        );

        let Some(router) = self.router.as_ref() else {
            anyhow::bail!("Per-command routing mode requires a router");
        };

        let (client_read, mut client_write) = client_stream.split();
        let mut client_reader = BufReader::with_capacity(READER_CAPACITY, client_read);

        let mut client_to_backend_bytes = BytesTransferred::zero();
        let mut backend_to_client_bytes = BytesTransferred::zero();

        // Auth state: username from AUTHINFO USER command
        let mut auth_username: Option<String> = None;

        // Send initial greeting to client
        client_write.write_all(PROXY_GREETING_PCR).await?;
        client_write.flush().await?;

        debug!(
            "Client {} sent greeting, entering command loop",
            self.client_addr
        );

        // Reuse command buffer to avoid allocations per command
        let mut command = String::with_capacity(COMMAND);

        // PERFORMANCE: Cache authenticated state to avoid atomic loads after auth succeeds
        // If auth is disabled, skip checks from the start
        let mut skip_auth_check = !self.auth_handler.is_enabled();

        // Process commands one at a time
        loop {
            command.clear();

            let n = match client_reader.read_line(&mut command).await {
                Ok(0) => {
                    debug!("Client {} disconnected", self.client_addr);
                    break;
                }
                Ok(n) => n,
                Err(e) => {
                    connection::log_client_error(
                        self.client_addr,
                        self.username().as_deref(),
                        &e,
                        TransferMetrics {
                            client_to_backend: client_to_backend_bytes.into(),
                            backend_to_client: backend_to_client_bytes.into(),
                        },
                    );
                    break;
                }
            };

            client_to_backend_bytes.add(n);
            let trimmed = command.trim();

            // Handle QUIT locally
            if let common::QuitStatus::Quit(bytes) =
                common::handle_quit_command(&command, &mut client_write).await?
            {
                backend_to_client_bytes += bytes;
                break;
            }

            let action = CommandHandler::classify(&command);
            skip_auth_check = skip_auth_check
                || self
                    .authenticated
                    .load(std::sync::atomic::Ordering::Acquire);

            let decision = decide_command_routing(
                action.clone(),
                skip_auth_check,
                self.auth_handler.is_enabled(),
                self.routing_mode,
                &command,
            );

            match decision {
                CommandRoutingDecision::InterceptAuth => {
                    // Extract auth action from the original CommandAction
                    let auth_action = match action {
                        CommandAction::InterceptAuth(a) => a,
                        _ => unreachable!(
                            "InterceptAuth decision must come from InterceptAuth action"
                        ),
                    };

                    backend_to_client_bytes += match common::handle_auth_command(
                        &self.auth_handler,
                        auth_action,
                        &mut client_write,
                        &mut auth_username,
                        &self.authenticated,
                    )
                    .await?
                    {
                        common::AuthResult::Authenticated(bytes) => {
                            common::on_authentication_success(
                                self.client_addr,
                                auth_username.clone(),
                                &self.routing_mode,
                                &self.metrics,
                                self.connection_stats(),
                                |username| self.set_username(username),
                            );
                            skip_auth_check = true;
                            bytes
                        }
                        common::AuthResult::NotAuthenticated(bytes) => bytes,
                    };
                }

                CommandRoutingDecision::Forward => {
                    if let Err(e) = self
                        .route_and_execute_command(
                            router,
                            &command,
                            &mut client_write,
                            &mut client_to_backend_bytes,
                            &mut backend_to_client_bytes,
                        )
                        .await
                    {
                        // Log error but continue session - don't terminate on single command failure
                        // Use debug level for 430 (article not found) as it's a normal operational event
                        let error_str = format!("{}", e);
                        if error_str.contains("430") {
                            debug!(
                                "Article not found after retries for client {}: {}",
                                self.client_addr, trimmed
                            );
                        } else {
                            warn!(
                                "Command {:?} failed for client {}: {}",
                                trimmed, self.client_addr, e
                            );
                        }

                        // Send error response to client
                        let error_response = b"503 Command failed\r\n";
                        if let Err(write_err) = client_write.write_all(error_response).await {
                            // If we can't write the error, the connection is broken - terminate
                            return Err(write_err.into());
                        }
                        backend_to_client_bytes.add(error_response.len());
                    }
                }

                CommandRoutingDecision::RequireAuth => {
                    use crate::protocol::AUTH_REQUIRED_FOR_COMMAND;
                    client_write.write_all(AUTH_REQUIRED_FOR_COMMAND).await?;
                    backend_to_client_bytes.add(AUTH_REQUIRED_FOR_COMMAND.len());
                }

                CommandRoutingDecision::SwitchToStateful => {
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
                    let response = match action {
                        CommandAction::Reject(r) => r,
                        _ => unreachable!("Reject decision must come from Reject action"),
                    };
                    client_write.write_all(response.as_bytes()).await?;
                    backend_to_client_bytes.add(response.len());
                }
            }
        }

        // Log session summary and close user connection
        let total_bytes = client_to_backend_bytes.as_u64() + backend_to_client_bytes.as_u64();
        if total_bytes < common::SMALL_TRANSFER_THRESHOLD {
            debug!(
                "Session summary {} | ↑{} ↓{} | Short session (likely test connection)",
                self.client_addr,
                crate::formatting::format_bytes(client_to_backend_bytes.as_u64()),
                crate::formatting::format_bytes(backend_to_client_bytes.as_u64())
            );
        }
        if let Some(ref m) = self.metrics {
            m.user_connection_closed(self.username().as_deref());
        }

        Ok(TransferMetrics {
            client_to_backend: client_to_backend_bytes.into(),
            backend_to_client: backend_to_client_bytes.into(),
        })
    }

    /// Get backend availability from location cache or run precheck
    async fn get_backend_availability(
        &self,
        router: &BackendSelector,
        message_id: Option<&crate::types::protocol::MessageId<'_>>,
    ) -> Option<crate::cache::BackendAvailability> {
        if !self.precheck_enabled() {
            return None;
        }

        let msg_id = message_id?;
        let location_cache = self.location_cache()?;

        // Check cache first
        match location_cache.get(msg_id).await {
            Some(availability) => {
                debug!(
                    "Location cache hit for {}: availability={:064b}",
                    msg_id,
                    availability.as_u64()
                );
                Some(availability)
            }
            None => {
                // Cache miss - precheck backends
                debug!("Location cache miss for {} - prechecking backends", msg_id);

                let backends = router.all_backend_providers();
                let num_backends = router.backend_count();

                // Fast-fail optimization: For all message-ID based commands (STAT/ARTICLE/BODY/HEAD),
                // return as soon as we find ONE backend with the article. The background task will
                // complete the full precheck and update cache for future requests.
                let fast_fail = true;

                match crate::session::precheck::precheck_article_availability(
                    msg_id,
                    &backends,
                    location_cache,
                    num_backends,
                    fast_fail,
                    self.metrics.as_ref(),
                )
                .await
                {
                    Ok(availability) => {
                        debug!(
                            "Precheck complete for {}: availability={:064b}",
                            msg_id,
                            availability.as_u64()
                        );
                        Some(availability)
                    }
                    Err(e) => {
                        warn!("Precheck failed for {}: {}", msg_id, e);
                        None
                    }
                }
            }
        }
    }

    /// Select best backend using Adaptive Weighted Routing or round-robin
    async fn select_backend_for_command(
        &self,
        router: &BackendSelector,
        command: &str,
        backend_availability: Option<&crate::cache::BackendAvailability>,
        message_id: Option<&crate::types::protocol::MessageId<'_>>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_to_client_bytes: &mut BytesTransferred,
    ) -> Result<crate::types::BackendId> {
        let Some(availability) = backend_availability else {
            // No availability info - use standard routing
            return router.route_command(self.client_id, command);
        };

        // Check if any backend has the article
        if !availability.has_any() {
            // No backend has this article - return 430 immediately instead of trying backends
            debug!(
                "Precheck shows no backend has article {} - returning 430",
                message_id.unwrap()
            );
            let error_msg = crate::protocol::error_response(430, "No such article");
            client_write.write_all(error_msg.as_bytes()).await?;
            backend_to_client_bytes.add(error_msg.len());

            // Complete routing even though we didn't use a backend
            // (no backend to complete since we didn't route)
            return Ok(crate::types::BackendId::from_index(0)); // Dummy backend ID
        }

        // Select best available backend using Adaptive Weighted Routing (AWR)
        self.select_backend_with_awr(router, availability)
    }

    /// Apply Adaptive Weighted Routing algorithm to select best backend
    fn select_backend_with_awr(
        &self,
        router: &BackendSelector,
        availability: &crate::cache::BackendAvailability,
    ) -> Result<crate::types::BackendId> {
        let mut selected_backend: Option<(crate::types::BackendId, f64)> = None;

        for i in 0..router.backend_count() {
            let backend_id = crate::types::BackendId::from_index(i);

            // Skip backends that don't have the article
            if !availability.has_article(backend_id) {
                continue;
            }

            // Calculate adaptive score for this backend
            if let Some(provider) = router.backend_provider(backend_id) {
                let status = provider.status();
                let max_size = status.max_size.get() as f64;
                let available = status.available.get() as f64;
                let pending = router.backend_load(backend_id).unwrap_or(0) as f64;

                // Prevent division by zero
                if max_size == 0.0 {
                    continue;
                }

                let used = max_size - available;

                // Get historical transfer performance if available
                let transfer_penalty = self.calculate_transfer_penalty(backend_id);

                // AWR scoring (lower is better):
                // - 30% connection availability
                // - 20% current load
                // - 20% saturation penalty (quadratic)
                // - 30% transfer speed penalty (prefer faster backends)
                let availability_score = 1.0 - (available / max_size);
                let load_score = pending / max_size;
                let saturation_ratio = used / max_size;
                let saturation_score = saturation_ratio * saturation_ratio;

                let score = (availability_score * 0.30)
                    + (load_score * 0.20)
                    + (saturation_score * 0.20)
                    + (transfer_penalty * 0.30);

                debug!(
                    "Backend {:?}: available={}/{}, pending={}, transfer_penalty={:.3}, score={:.3} (lower=better)",
                    backend_id,
                    available as usize,
                    max_size as usize,
                    pending as usize,
                    transfer_penalty,
                    score
                );

                // Select backend with lowest score
                if selected_backend.is_none() || score < selected_backend.unwrap().1 {
                    selected_backend = Some((backend_id, score));
                }
            }
        }

        // Extract the selected backend
        selected_backend
            .map(|(id, score)| {
                debug!("AWR selected backend {:?} with score {:.3}", id, score);
                Ok(id)
            })
            .unwrap_or_else(|| {
                warn!("Logic error: has_any() was true but no backend found - using round-robin");
                Err(anyhow::anyhow!("No backend available"))
            })
    }

    /// Calculate transfer penalty based on historical performance metrics
    fn calculate_transfer_penalty(&self, backend_id: crate::types::BackendId) -> f64 {
        let Some(ref metrics) = self.metrics else {
            return 0.5; // No metrics - neutral penalty
        };

        let snapshot = metrics.snapshot();
        let Some(backend_stats) = snapshot.backend_stats.get(backend_id.as_index()) else {
            return 0.5; // Unknown - neutral penalty
        };

        // Calculate bytes/sec from backend's historical performance
        // Higher transfer rate = lower penalty
        let uptime_secs = snapshot.uptime.as_secs_f64().max(1.0);
        let bytes_received = backend_stats.bytes_received.as_u64() as f64;
        let transfer_rate = bytes_received / uptime_secs;

        // Normalize: 0 MB/s = 1.0 penalty, 10+ MB/s = 0.0 penalty
        let mb_per_sec = transfer_rate / 1_000_000.0;
        1.0 - (mb_per_sec / 10.0).min(1.0)
    }

    /// Try to serve STAT command directly from cache without backend query
    #[allow(clippy::too_many_arguments)] // All parameters are necessary for this operation
    async fn try_serve_stat_from_cache(
        &self,
        backend_availability: Option<&crate::cache::BackendAvailability>,
        command: &str,
        message_id: Option<&crate::types::protocol::MessageId<'_>>,
        backend_id: crate::types::BackendId,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        client_to_backend_bytes: &mut BytesTransferred,
        backend_to_client_bytes: &mut BytesTransferred,
    ) -> Result<Option<crate::types::BackendId>> {
        // Only optimize STAT commands with cache hits
        if backend_availability.is_none()
            || !matches!(NntpCommand::parse(command), NntpCommand::ArticleByMessageId)
            || !command.trim_start().to_uppercase().starts_with("STAT ")
        {
            return Ok(None);
        }

        debug!(
            "Cache hit for STAT {}: responding 223 without backend query (attributing to {:?})",
            message_id.unwrap(),
            backend_id
        );

        let response = b"223 0 <article> Article exists\r\n";
        client_write.write_all(response).await?;
        backend_to_client_bytes.add(response.len());
        client_to_backend_bytes.add(command.len());

        // Record metrics for cached response
        if let Some(ref metrics) = self.metrics {
            use crate::types::MetricsBytes;
            let cmd_bytes = MetricsBytes::new(command.len() as u64);
            let resp_bytes = MetricsBytes::new(response.len() as u64);

            let _ = metrics.record_command_execution(backend_id, cmd_bytes, resp_bytes);
            self.user_bytes_sent(command.len() as u64);
            self.user_bytes_received(response.len() as u64);
        }

        self.record_command(backend_id);
        self.user_command();

        Ok(Some(backend_id))
    }

    /// Extract message-ID from article commands for location cache routing
    #[inline]
    fn extract_message_id_for_routing(
        command: &str,
    ) -> Option<crate::types::protocol::MessageId<'_>> {
        if matches!(NntpCommand::parse(command), NntpCommand::ArticleByMessageId) {
            crate::protocol::NntpResponse::extract_message_id(command)
        } else {
            None
        }
    }

    /// Route a single command to a backend and execute it
    ///
    /// This function is `pub(super)` to allow reuse of per-command routing logic by sibling handler modules
    /// (such as `hybrid.rs`) that also need to route commands.
    pub(super) async fn route_and_execute_command(
        &self,
        router: &BackendSelector,
        command: &str,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        client_to_backend_bytes: &mut BytesTransferred,
        backend_to_client_bytes: &mut BytesTransferred,
    ) -> Result<crate::types::BackendId> {
        // Check cache early - returns Some(backend_id) on cache hit, None otherwise
        if let Some(backend_id) = self
            .check_cache_and_serve(router, command, client_write, backend_to_client_bytes)
            .await?
        {
            return Ok(backend_id);
        }

        // Determine if we need to cache this response after fetching
        let should_cache = self.should_cache_command(command);

        // Check location cache for smart routing (STAT/ARTICLE/BODY/HEAD with message-ID)
        let message_id = Self::extract_message_id_for_routing(command);

        // Adaptive routing: If precheck enabled and we have a message-ID but no cached location,
        // precheck all backends to find which ones have the article
        let backend_availability = self
            .get_backend_availability(router, message_id.as_ref())
            .await;

        // Route the command to get a backend
        let backend_id = self
            .select_backend_for_command(
                router,
                command,
                backend_availability.as_ref(),
                message_id.as_ref(),
                client_write,
                backend_to_client_bytes,
            )
            .await?;

        // For STAT commands with cache hit, answer directly without backend query
        if let Some(backend) = self
            .try_serve_stat_from_cache(
                backend_availability.as_ref(),
                command,
                message_id.as_ref(),
                backend_id,
                client_write,
                client_to_backend_bytes,
                backend_to_client_bytes,
            )
            .await?
        {
            return Ok(backend);
        }

        // Retry on 430 or timeout - try backends until we find one with the article
        const BACKEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

        // Helper: Find next available backend from bitmap
        #[inline]
        fn find_next_available_backend(
            remaining: &crate::cache::BackendAvailability,
            backend_count: usize,
        ) -> Option<crate::types::BackendId> {
            (0..backend_count)
                .map(crate::types::BackendId::from_index)
                .find(|&id| remaining.has_article(id))
        }

        // Helper: Acquire pooled connection with timeout
        #[inline]
        async fn acquire_connection_with_timeout(
            provider: &crate::pool::DeadpoolConnectionProvider,
            backend: crate::types::BackendId,
            timeout: std::time::Duration,
        ) -> anyhow::Result<deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>>
        {
            tokio::time::timeout(timeout, provider.get_pooled_connection())
                .await
                .map_err(|_| anyhow::anyhow!("Backend {:?} pool timeout", backend))?
                .map_err(|e| anyhow::anyhow!("Backend {:?} pool error: {}", backend, e))
        }

        // Helper: Check if error is retryable (430 article not found)
        #[inline]
        fn is_article_not_found_error(result: &anyhow::Result<()>) -> bool {
            result
                .as_ref()
                .err()
                .map(|e| format!("{}", e).to_lowercase().contains("430"))
                .unwrap_or(false)
        }

        // Helper: Update location cache for failed backend
        #[inline]
        fn update_cache_for_failed_backend(
            message_id: Option<&crate::types::protocol::MessageId<'_>>,
            location_cache: Option<&Arc<crate::cache::ArticleLocationCache>>,
            backend: crate::types::BackendId,
        ) {
            if let (Some(msg_id), Some(cache)) = (message_id, location_cache) {
                let cache = Arc::clone(cache);
                let msg_id_owned = msg_id.to_owned();
                tokio::spawn(async move {
                    cache.update(&msg_id_owned, backend, false).await;
                });
                debug!(
                    "Updating location cache: {} -> backend {:?} (NOT available)",
                    msg_id, backend
                );
            }
        }

        // Track which backends have been tried by updating the availability bitmap
        let mut remaining_backends = backend_availability.unwrap_or_else(|| {
            // No cache - create bitmap with all backends available
            (0..router.backend_count())
                .map(crate::types::BackendId::from_index)
                .fold(
                    crate::cache::BackendAvailability::new(),
                    |mut bitmap, id| {
                        bitmap.mark_available(id);
                        bitmap
                    },
                )
        });

        // Retry loop - try backends until one succeeds or all fail
        let (result, cmd_bytes, resp_bytes, successful_backend) = 'retry: loop {
            // Find next available backend
            let tried_backend =
                find_next_available_backend(&remaining_backends, router.backend_count())
                    .ok_or_else(|| {
                        anyhow::anyhow!(
                            "All {} backends tried, article not found",
                            router.backend_count()
                        )
                    })?;

            // Get connection from pool with timeout
            let Some(provider) = router.backend_provider(tried_backend) else {
                remaining_backends.mark_unavailable(tried_backend);
                continue;
            };

            let mut pooled_conn =
                match acquire_connection_with_timeout(provider, tried_backend, BACKEND_TIMEOUT)
                    .await
                {
                    Ok(conn) => conn,
                    Err(e) => {
                        warn!("{}", e);
                        remaining_backends.mark_unavailable(tried_backend);
                        continue;
                    }
                };

            debug!(
                "Client {} got pooled connection for backend {:?}",
                self.client_addr, tried_backend
            );
            self.record_command(tried_backend);
            self.user_command();

            // Get fresh buffer for this attempt (prevents corruption from retries)
            let mut buffer = self.buffer_pool.acquire().await;

            // Execute command with timeout
            let exec_result = tokio::time::timeout(BACKEND_TIMEOUT, async {
                if should_cache {
                    self.execute_command_with_caching(
                        &mut pooled_conn,
                        command,
                        client_write,
                        tried_backend,
                        client_to_backend_bytes,
                        backend_to_client_bytes,
                        &mut buffer,
                    )
                    .await
                } else {
                    self.execute_command_on_backend(
                        &mut pooled_conn,
                        command,
                        client_write,
                        tried_backend,
                        client_to_backend_bytes,
                        backend_to_client_bytes,
                        &mut buffer,
                    )
                    .await
                }
            })
            .await;

            let (exec_res, _, exec_cmd_bytes, exec_resp_bytes) = match exec_result {
                Ok(result) => result,
                Err(_) => {
                    warn!("Backend {:?} execution timeout", tried_backend);
                    router.complete_command(tried_backend);
                    remaining_backends.mark_unavailable(tried_backend);
                    continue;
                }
            };

            // Check for 430 (article not found) - retry with another backend
            if is_article_not_found_error(&exec_res) {
                remaining_backends.mark_unavailable(tried_backend);
                update_cache_for_failed_backend(
                    message_id.as_ref(),
                    self.location_cache(),
                    tried_backend,
                );
                router.complete_command(tried_backend);
                continue;
            }

            // Success or non-retryable error - break out with result
            break 'retry (exec_res, exec_cmd_bytes, exec_resp_bytes, tried_backend);
        };

        // Record metrics ONCE using type-safe API (prevents double-counting)
        if let Some(ref metrics) = self.metrics {
            // Peek byte counts before consuming
            let cmd_size = cmd_bytes.peek();
            let resp_size = resp_bytes.peek();

            // Record command + per-backend + global bytes in one call
            let _ = metrics.record_command_execution(successful_backend, cmd_bytes, resp_bytes);

            // Record per-user metrics
            self.user_bytes_sent(cmd_size);
            self.user_bytes_received(resp_size);
        }

        // Update location cache if we have a message-ID (for ARTICLE/BODY/HEAD/STAT)
        // This helps future requests route directly to the right backend
        if let (Some(msg_id), Some(location_cache)) = (message_id, self.location_cache())
            && result.is_ok()
        {
            // Update cache in background (don't block hot path)
            let cache = Arc::clone(location_cache);
            let msg_id_owned = msg_id.to_owned();
            tokio::spawn(async move {
                cache.update(&msg_id_owned, successful_backend, true).await;
            });
            debug!(
                "Updating location cache: {} -> backend {:?} (available)",
                msg_id, successful_backend
            );
        }

        // Record errors to metrics if execution failed
        if let Err(ref e) = result {
            if let Some(ref metrics) = self.metrics {
                metrics.record_error(successful_backend);
                metrics.user_error(self.username().as_deref());
            }
            debug!(
                "Command execution error on backend {:?}: {}",
                successful_backend, e
            );
        }

        // Complete the request - decrement pending count (lock-free!)
        router.complete_command(successful_backend);

        result
            .map(|_| successful_backend)
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    /// Execute a command on a backend connection and stream the response to the client
    ///
    /// # Performance Critical Hot Path
    ///
    /// This function implements **pipelined streaming** for NNTP responses, which is essential
    /// for high-throughput article downloads. The double-buffering approach allows reading the
    /// next chunk from the backend while writing the current chunk to the client concurrently.
    ///
    /// **DO NOT refactor this to buffer entire responses** - that would kill performance:
    /// - Large articles (50MB+) would be fully buffered before streaming to client
    /// - No concurrent I/O = sequential read-then-write instead of pipelined read+write
    /// - Performance drops from 100+ MB/s to < 1 MB/s
    ///
    /// The complexity here is justified by the 100x+ performance gain on large transfers.
    ///
    /// Execute a command on a backend connection
    ///
    /// **IMPORTANT CHANGE**: This function now returns type-safe `MetricsBytes<Unrecorded>`
    /// to prevent double-counting. The caller MUST record these bytes to metrics exactly once.
    ///
    /// Returns `(Result<()>, got_backend_data, cmd_bytes, resp_bytes)` where:
    /// - `got_backend_data = true` means we successfully read from backend before any error
    /// - `cmd_bytes`: Unrecorded command bytes (MUST be recorded by caller)
    /// - `resp_bytes`: Unrecorded response bytes (MUST be recorded by caller)
    /// - This distinguishes backend failures (remove from pool) from client disconnects (keep backend)
    ///
    /// This function is `pub(super)` and is intended for use by `hybrid.rs` for stateful mode command execution.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_command_on_backend(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        command: &str,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_id: crate::types::BackendId,
        client_to_backend_bytes: &mut BytesTransferred,
        backend_to_client_bytes: &mut BytesTransferred,
        chunk_buffer: &mut PooledBuffer, // Reusable buffer from pool
    ) -> (
        Result<()>,
        bool,
        crate::types::MetricsBytes<crate::types::Unrecorded>,
        crate::types::MetricsBytes<crate::types::Unrecorded>,
    ) {
        use crate::types::MetricsBytes;
        let mut got_backend_data = false;

        // Send command and read first chunk into reusable buffer
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
                Ok(result) => {
                    got_backend_data = true;
                    result
                }
                Err(e) => {
                    return (
                        Err(e),
                        got_backend_data,
                        MetricsBytes::new(0),
                        MetricsBytes::new(0),
                    );
                }
            };

        // Check for 430 (no such article) - DON'T stream, return error for retry
        if let Some(code) = response_code.status_code()
            && code.as_u16() == 430
        {
            debug!(
                "Backend {:?} returned 430 (no such article), returning for retry",
                backend_id
            );
            return (
                Err(anyhow::anyhow!("Backend returned 430 (no such article)")),
                got_backend_data,
                MetricsBytes::new(0),
                MetricsBytes::new(0),
            );
        }

        // Record time to first byte and split timing
        if let Some(ref metrics) = self.metrics {
            metrics.record_ttfb_micros(backend_id, ttfb_micros);
            metrics.record_send_recv_micros(backend_id, send_micros, recv_micros);
        }

        client_to_backend_bytes.add(command.len());

        // Extract message-ID from command if present (for correlation with SABnzbd errors)
        let msgid = common::extract_message_id(command);

        // For multiline responses, use pipelined streaming
        let bytes_written = if is_multiline {
            match streaming::stream_multiline_response(
                &mut **pooled_conn,
                client_write,
                &chunk_buffer[..n], // Use buffer slice instead of owned Vec
                n,
                self.client_addr,
                backend_id,
                &self.buffer_pool,
            )
            .await
            {
                Ok(bytes) => bytes,
                Err(e) => {
                    return (
                        Err(e),
                        got_backend_data,
                        MetricsBytes::new(command.len() as u64),
                        MetricsBytes::new(0),
                    );
                }
            }
        } else {
            // Single-line response - just write the first chunk
            if let Some(code) = response_code.status_code() {
                let raw_code = code.as_u16();
                // Warn only for truly unusual responses (not 223 or errors)
                if let Some(id) = msgid
                    && raw_code != 223
                    && !code.is_error()
                {
                    warn!(
                        "Client {} ARTICLE {} got unusual single-line response: {}",
                        self.client_addr, id, code
                    );
                }
            }

            match client_write.write_all(&chunk_buffer[..n]).await {
                Ok(_) => n as u64,
                Err(e) => {
                    return (
                        Err(e.into()),
                        got_backend_data,
                        MetricsBytes::new(command.len() as u64),
                        MetricsBytes::new(0),
                    );
                }
            }
        };

        backend_to_client_bytes.add(bytes_written as usize);

        // Track metrics based on response code
        if let Some(ref metrics) = self.metrics
            && let Some(code) = response_code.status_code()
        {
            let raw_code = code.as_u16();

            // Track 4xx/5xx errors (excluding expected "not found" responses)
            // 423 = No such article (normal when searching)
            // 430 = No such article in group (normal when searching)
            if (400..500).contains(&raw_code) && raw_code != 423 && raw_code != 430 {
                metrics.record_error_4xx(backend_id);
            } else if raw_code >= 500 {
                metrics.record_error_5xx(backend_id);
            }

            // Track article size for successful article retrieval responses
            // 220 = ARTICLE (full article), 221 = HEAD (headers only), 222 = BODY (body only)
            if is_multiline && (raw_code == 220 || raw_code == 221 || raw_code == 222) {
                metrics.record_article(backend_id, bytes_written);
            }
        }

        // Return unrecorded metrics bytes - caller MUST record to prevent double-counting
        let cmd_bytes = MetricsBytes::new(command.len() as u64);
        let resp_bytes = MetricsBytes::new(bytes_written);

        if let Some(id) = msgid {
            debug!(
                "Client {} ARTICLE {} completed: wrote {} bytes to client",
                self.client_addr, id, bytes_written
            );
        }

        (Ok(()), got_backend_data, cmd_bytes, resp_bytes)
    }

    /// Execute a command with response caching
    ///
    /// Similar to `execute_command_on_backend` but buffers the entire response for caching.
    /// Only call this for ARTICLE by message-ID commands when cache is enabled.
    ///
    /// **Performance trade-off**: Buffers entire response instead of streaming.
    /// Acceptable for ARTICLE commands when using the caching proxy binary (`nntp-cache-proxy`).
    /// The main `nntp-proxy` binary does not include caching - it's a separate optional binary.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn execute_command_with_caching(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        command: &str,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_id: crate::types::BackendId,
        client_to_backend_bytes: &mut BytesTransferred,
        backend_to_client_bytes: &mut BytesTransferred,
        chunk_buffer: &mut PooledBuffer,
    ) -> (
        Result<()>,
        bool,
        crate::types::MetricsBytes<crate::types::Unrecorded>,
        crate::types::MetricsBytes<crate::types::Unrecorded>,
    ) {
        use crate::types::MetricsBytes;
        let mut got_backend_data = false;
        let mut response_buffer = Vec::new();

        // Send command and read first chunk
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
                Ok(result) => {
                    got_backend_data = true;
                    result
                }
                Err(e) => {
                    return (
                        Err(e),
                        got_backend_data,
                        MetricsBytes::new(0),
                        MetricsBytes::new(0),
                    );
                }
            };

        if let Some(ref metrics) = self.metrics {
            metrics.record_ttfb_micros(backend_id, ttfb_micros);
            metrics.record_send_recv_micros(backend_id, send_micros, recv_micros);
        }

        // Check for 430 (no such article) - DON'T cache or stream, return error for retry
        if let Some(code) = response_code.status_code()
            && code.as_u16() == 430
        {
            debug!(
                "Backend {:?} returned 430 (no such article) in caching path, returning for retry",
                backend_id
            );
            return (
                Err(anyhow::anyhow!("Backend returned 430 (no such article)")),
                got_backend_data,
                MetricsBytes::new(0),
                MetricsBytes::new(0),
            );
        }

        client_to_backend_bytes.add(command.len());

        // Buffer first chunk
        response_buffer.extend_from_slice(&chunk_buffer[..n]);

        // For multiline, read and buffer all chunks
        if is_multiline {
            use tokio::io::AsyncReadExt;
            loop {
                let bytes_read = match pooled_conn.read(chunk_buffer.as_mut_slice()).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => {
                        return (
                            Err(e.into()),
                            got_backend_data,
                            MetricsBytes::new(command.len() as u64),
                            MetricsBytes::new(0),
                        );
                    }
                };

                let chunk = &chunk_buffer[..bytes_read];
                response_buffer.extend_from_slice(chunk);

                // Check for terminator
                if crate::protocol::NntpResponse::has_terminator_at_end(chunk) {
                    break;
                }
            }
        }

        // Write entire buffered response to client
        let bytes_written = response_buffer.len() as u64;
        if let Err(e) = client_write.write_all(&response_buffer).await {
            return (
                Err(e.into()),
                got_backend_data,
                MetricsBytes::new(command.len() as u64),
                MetricsBytes::new(0),
            );
        }

        backend_to_client_bytes.add(bytes_written as usize);

        // Cache successful ARTICLE responses (2xx status codes) - spawn in background
        if let Some(ref cache) = self.cache
            && let Some(message_id) = crate::protocol::NntpResponse::extract_message_id(command)
            && let Some(code) = response_code.status_code()
            && code.is_success()
        {
            info!(
                "Caching response for message-ID: {} ({} bytes)",
                message_id, bytes_written
            );
            let cache = Arc::clone(cache);
            let message_id_owned = message_id.to_owned();
            let response = Arc::new(response_buffer);
            let article = crate::cache::CachedArticle { response };
            tokio::spawn(async move {
                cache.insert(message_id_owned, article).await;
            });
        }

        // Track metrics
        if let Some(ref metrics) = self.metrics
            && let Some(code) = response_code.status_code()
        {
            let raw_code = code.as_u16();

            if (400..500).contains(&raw_code) && raw_code != 423 && raw_code != 430 {
                metrics.record_error_4xx(backend_id);
            } else if raw_code >= 500 {
                metrics.record_error_5xx(backend_id);
            }

            if is_multiline && (raw_code == 220 || raw_code == 221 || raw_code == 222) {
                metrics.record_article(backend_id, bytes_written);
            }
        }

        (
            Ok(()),
            got_backend_data,
            MetricsBytes::new(command.len() as u64),
            MetricsBytes::new(bytes_written),
        )
    }

    /// Check cache and serve if hit, return backend_id on hit, None on miss
    #[inline]
    async fn check_cache_and_serve(
        &self,
        router: &BackendSelector,
        command: &str,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_to_client_bytes: &mut BytesTransferred,
    ) -> Result<Option<crate::types::BackendId>> {
        // Functional pipeline: cache → is ARTICLE → extract ID
        let Some((cache, message_id)) = self
            .cache
            .as_ref()
            .filter(|_| matches!(NntpCommand::parse(command), NntpCommand::ArticleByMessageId))
            .zip(crate::protocol::NntpResponse::extract_message_id(command))
        else {
            return Ok(None);
        };

        self.serve_from_cache_or_miss(
            cache,
            message_id,
            router,
            command,
            client_write,
            backend_to_client_bytes,
        )
        .await
    }

    /// Serve cached article or return None on miss
    #[inline]
    async fn serve_from_cache_or_miss(
        &self,
        cache: &crate::cache::ArticleCache,
        message_id: crate::types::protocol::MessageId<'_>,
        router: &BackendSelector,
        command: &str,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_to_client_bytes: &mut BytesTransferred,
    ) -> Result<Option<crate::types::BackendId>> {
        match cache.get(&message_id).await {
            Some(cached) => {
                info!(
                    "Cache HIT for message-ID: {} (size: {} bytes)",
                    message_id,
                    cached.response.len()
                );
                client_write.write_all(&cached.response).await?;
                backend_to_client_bytes.add(cached.response.len());

                let backend_id = router.route_command(self.client_id, command)?;
                router.complete_command(backend_id);
                Ok(Some(backend_id))
            }
            None => {
                info!("Cache MISS for message-ID: {}", message_id);
                Ok(None)
            }
        }
    }

    /// Determine if command should be cached (cache miss on ARTICLE by message-ID)
    #[inline]
    fn should_cache_command(&self, command: &str) -> bool {
        self.cache.is_some()
            && matches!(NntpCommand::parse(command), NntpCommand::ArticleByMessageId)
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

    #[test]
    fn test_should_cache_command_with_cache_enabled() {
        let session = create_test_session_with_cache();
        assert!(session.should_cache_command("ARTICLE <test@example.com>"));
    }

    #[test]
    fn test_should_cache_command_with_cache_disabled() {
        let session = create_test_session_without_cache();
        assert!(!session.should_cache_command("ARTICLE <test@example.com>"));
    }

    #[test]
    fn test_should_cache_command_non_article_command() {
        let session = create_test_session_with_cache();
        assert!(!session.should_cache_command("GROUP alt.test"));
        assert!(!session.should_cache_command("LIST"));
        assert!(!session.should_cache_command("QUIT"));
    }

    #[test]
    fn test_should_cache_command_article_by_number() {
        let session = create_test_session_with_cache();
        // ARTICLE by number (not message-ID) should not be cached
        assert!(!session.should_cache_command("ARTICLE 123"));
    }

    #[test]
    fn test_should_cache_command_variations() {
        let session = create_test_session_with_cache();
        // Only ARTICLE, BODY, HEAD, STAT with message-ID are cacheable
        assert!(session.should_cache_command("ARTICLE <msg@host>"));
        assert!(session.should_cache_command("BODY <msg@host>"));
        assert!(session.should_cache_command("HEAD <msg@host>"));
        assert!(session.should_cache_command("STAT <msg@host>"));
    }

    // Tests for decide_command_routing pure function

    #[test]
    fn test_decide_routing_auth_commands_always_intercepted() {
        use crate::command::AuthAction;

        // Auth commands should always be intercepted regardless of other flags
        let action = CommandAction::InterceptAuth(AuthAction::RequestPassword("test".to_string()));

        assert_eq!(
            decide_command_routing(
                action.clone(),
                true,
                true,
                RoutingMode::PerCommand,
                "AUTHINFO USER test"
            ),
            CommandRoutingDecision::InterceptAuth
        );
        assert_eq!(
            decide_command_routing(
                action.clone(),
                false,
                true,
                RoutingMode::PerCommand,
                "AUTHINFO USER test"
            ),
            CommandRoutingDecision::InterceptAuth
        );
        assert_eq!(
            decide_command_routing(
                action.clone(),
                false,
                false,
                RoutingMode::Stateful,
                "AUTHINFO USER test"
            ),
            CommandRoutingDecision::InterceptAuth
        );
    }

    #[test]
    fn test_decide_routing_forward_when_authenticated() {
        let action = CommandAction::ForwardStateless;

        // Should forward when authenticated, regardless of auth_enabled
        assert_eq!(
            decide_command_routing(action.clone(), true, true, RoutingMode::PerCommand, "LIST"),
            CommandRoutingDecision::Forward
        );
        assert_eq!(
            decide_command_routing(action.clone(), true, false, RoutingMode::PerCommand, "LIST"),
            CommandRoutingDecision::Forward
        );
    }

    #[test]
    fn test_decide_routing_forward_when_auth_disabled() {
        let action = CommandAction::ForwardStateless;

        // Should forward when auth is disabled, even if not authenticated
        assert_eq!(
            decide_command_routing(
                action.clone(),
                false,
                false,
                RoutingMode::PerCommand,
                "LIST"
            ),
            CommandRoutingDecision::Forward
        );
    }

    #[test]
    fn test_decide_routing_require_auth_when_needed() {
        let action = CommandAction::ForwardStateless;

        // Should require auth when auth is enabled but not authenticated
        assert_eq!(
            decide_command_routing(action, false, true, RoutingMode::PerCommand, "LIST"),
            CommandRoutingDecision::RequireAuth
        );
    }

    #[test]
    fn test_decide_routing_switch_to_stateful_in_hybrid_mode() {
        let action = CommandAction::Reject("502 Command not supported\r\n");

        // Hybrid mode with stateful command should switch to stateful
        assert_eq!(
            decide_command_routing(
                action.clone(),
                true,
                false,
                RoutingMode::Hybrid,
                "GROUP alt.test"
            ),
            CommandRoutingDecision::SwitchToStateful
        );

        // Also works when not authenticated
        assert_eq!(
            decide_command_routing(
                action.clone(),
                false,
                false,
                RoutingMode::Hybrid,
                "XOVER 1-100"
            ),
            CommandRoutingDecision::SwitchToStateful
        );
    }

    #[test]
    fn test_decide_routing_reject_in_per_command_mode() {
        let action = CommandAction::Reject("502 Command not supported\r\n");

        // Per-command mode should reject stateful commands
        assert_eq!(
            decide_command_routing(
                action.clone(),
                true,
                false,
                RoutingMode::PerCommand,
                "GROUP alt.test"
            ),
            CommandRoutingDecision::Reject
        );
    }

    #[test]
    fn test_decide_routing_reject_in_stateful_mode() {
        let action = CommandAction::Reject("502 Command not supported\r\n");

        // Stateful mode should reject (though this shouldn't happen in practice)
        assert_eq!(
            decide_command_routing(action, true, false, RoutingMode::Stateful, "INVALID"),
            CommandRoutingDecision::Reject
        );
    }

    #[test]
    fn test_decide_routing_hybrid_mode_stateless_forwarded() {
        let action = CommandAction::ForwardStateless;

        // Hybrid mode with stateless command should forward
        assert_eq!(
            decide_command_routing(action, true, false, RoutingMode::Hybrid, "LIST"),
            CommandRoutingDecision::Forward
        );
    }

    #[test]
    fn test_decide_routing_hybrid_mode_reject_non_stateful() {
        let action = CommandAction::Reject("502 Command not supported\r\n");

        // Hybrid mode with rejected but non-stateful command should just reject
        assert_eq!(
            decide_command_routing(action, true, false, RoutingMode::Hybrid, "INVALIDCMD"),
            CommandRoutingDecision::Reject
        );
    }

    #[test]
    fn test_decide_routing_all_modes_with_stateful_commands() {
        let action = CommandAction::Reject("502 Command not supported\r\n");
        let stateful_commands = vec!["GROUP alt.test", "NEXT", "LAST", "XOVER 1-100"];

        for cmd in stateful_commands {
            // Hybrid mode: switch to stateful
            assert_eq!(
                decide_command_routing(action.clone(), true, false, RoutingMode::Hybrid, cmd),
                CommandRoutingDecision::SwitchToStateful,
                "Failed for command: {}",
                cmd
            );

            // Per-command mode: reject
            assert_eq!(
                decide_command_routing(action.clone(), true, false, RoutingMode::PerCommand, cmd),
                CommandRoutingDecision::Reject,
                "Failed for command: {}",
                cmd
            );

            // Stateful mode: reject (though shouldn't reach this in practice)
            assert_eq!(
                decide_command_routing(action.clone(), true, false, RoutingMode::Stateful, cmd),
                CommandRoutingDecision::Reject,
                "Failed for command: {}",
                cmd
            );
        }
    }

    #[test]
    fn test_decide_routing_auth_flow_progression() {
        let stateless_action = CommandAction::ForwardStateless;

        // Step 1: Not authenticated, auth enabled -> require auth
        assert_eq!(
            decide_command_routing(
                stateless_action.clone(),
                false,
                true,
                RoutingMode::PerCommand,
                "LIST"
            ),
            CommandRoutingDecision::RequireAuth
        );

        // Step 2: Authenticated, auth enabled -> forward
        assert_eq!(
            decide_command_routing(
                stateless_action.clone(),
                true,
                true,
                RoutingMode::PerCommand,
                "LIST"
            ),
            CommandRoutingDecision::Forward
        );
    }

    #[test]
    fn test_should_cache_command_body_and_head() {
        let session = create_test_session_with_cache();
        // BODY and HEAD by message-ID should also be cached
        assert!(session.should_cache_command("BODY <test@example.com>"));
        assert!(session.should_cache_command("HEAD <msg@host>"));
        assert!(session.should_cache_command("STAT <stat@test>"));
    }

    #[test]
    fn test_should_cache_command_edge_cases() {
        let session = create_test_session_with_cache();
        // Empty or malformed commands should not be cached
        assert!(!session.should_cache_command(""));
        assert!(!session.should_cache_command("ARTICLE"));
        // Incomplete message-ID is still recognized by classifier
        // but won't be cached because extraction will fail later
        assert!(session.should_cache_command("ARTICLE <incomplete"));
    }

    // Helper functions for tests
    fn create_test_session_with_cache() -> ClientSession {
        use crate::auth::AuthHandler;
        use crate::cache::ArticleCache;
        use crate::types::BufferSize;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use std::sync::Arc;
        use std::time::Duration;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::new(1024).unwrap(), 4);
        let auth_handler = Arc::new(AuthHandler::new(None, None).unwrap());

        ClientSession::builder(addr, buffer_pool, auth_handler)
            .with_cache(Arc::new(ArticleCache::new(100, Duration::from_secs(3600))))
            .build()
    }

    fn create_test_session_without_cache() -> ClientSession {
        use crate::auth::AuthHandler;
        use crate::types::BufferSize;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use std::sync::Arc;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::new(1024).unwrap(), 4);
        let auth_handler = Arc::new(AuthHandler::new(None, None).unwrap());

        ClientSession::builder(addr, buffer_pool, auth_handler).build()
    }

    // Location cache tests

    #[tokio::test]
    async fn test_location_cache_passed_through_builder() {
        use crate::auth::AuthHandler;
        use crate::cache::ArticleLocationCache;
        use crate::types::BufferSize;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use std::sync::Arc;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::new(1024).unwrap(), 4);
        let auth_handler = Arc::new(AuthHandler::new(None, None).unwrap());
        let location_cache = Arc::new(ArticleLocationCache::new(1000, 4));

        let session = ClientSession::builder(addr, buffer_pool, auth_handler)
            .with_location_cache(location_cache.clone())
            .build();

        assert!(session.location_cache().is_some());
        // Verify it's the same Arc
        assert!(Arc::ptr_eq(
            session.location_cache().unwrap(),
            &location_cache
        ));

        // Test that cache operations work
        use crate::types::BackendId;
        use crate::types::protocol::MessageId;
        let msg_id = MessageId::try_from("<test@example.com>".to_string()).unwrap();
        location_cache
            .update(&msg_id, BackendId::from_index(0), true)
            .await;

        let availability = location_cache.get(&msg_id).await;
        assert!(availability.is_some());
        assert!(availability.unwrap().has_article(BackendId::from_index(0)));
    }

    #[test]
    fn test_location_cache_not_set_by_default() {
        let session = create_test_session_without_cache();
        assert!(session.location_cache().is_none());
    }

    #[test]
    fn test_message_id_extraction_for_all_article_commands() {
        use crate::protocol::NntpResponse;

        // Test all article retrieval commands
        let test_cases = vec![
            ("ARTICLE <test@example.com>", Some("<test@example.com>")),
            ("BODY <body@test.org>", Some("<body@test.org>")),
            ("HEAD <head@domain.net>", Some("<head@domain.net>")),
            ("STAT <stat@server.com>", Some("<stat@server.com>")),
            // Case insensitive
            ("article <msg@host>", Some("<msg@host>")),
            ("body <msg@host>", Some("<msg@host>")),
            ("head <msg@host>", Some("<msg@host>")),
            ("stat <msg@host>", Some("<msg@host>")),
            // No message-ID
            ("ARTICLE 123", None),
            ("GROUP alt.test", None),
            ("LIST", None),
        ];

        for (command, expected) in test_cases {
            let result = NntpResponse::extract_message_id(command);
            assert_eq!(
                result.as_ref().map(|m| m.as_str()),
                expected,
                "Failed for command: {}",
                command
            );
        }
    }

    #[tokio::test]
    async fn test_location_cache_update_requires_message_id() {
        use crate::auth::AuthHandler;
        use crate::cache::ArticleLocationCache;
        use crate::types::BufferSize;
        use std::net::{IpAddr, Ipv4Addr, SocketAddr};
        use std::sync::Arc;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::new(1024).unwrap(), 4);
        let auth_handler = Arc::new(AuthHandler::new(None, None).unwrap());
        let location_cache = Arc::new(ArticleLocationCache::new(1000, 4));

        let _session = ClientSession::builder(addr, buffer_pool, auth_handler)
            .with_location_cache(location_cache.clone())
            .build();

        // Verify cache starts empty
        assert_eq!(location_cache.entry_count(), 0);

        // Update cache with a test article
        use crate::types::protocol::MessageId;
        let msg_id = MessageId::try_from("<test@example.com>".to_string()).unwrap();
        location_cache
            .update(&msg_id, crate::types::BackendId::from_index(0), true)
            .await;
        location_cache.sync().await;

        // Verify cache was updated
        assert_eq!(location_cache.entry_count(), 1);

        // Verify we can retrieve it
        let availability = location_cache.get(&msg_id).await;
        assert!(availability.is_some());
        assert!(
            availability
                .unwrap()
                .has_article(crate::types::BackendId::from_index(0))
        );
    }

    #[test]
    fn test_article_by_message_id_classification() {
        use crate::command::NntpCommand;

        // All these should be classified as ArticleByMessageId
        let commands = vec![
            "ARTICLE <test@example.com>",
            "BODY <test@example.com>",
            "HEAD <test@example.com>",
            "STAT <test@example.com>",
        ];

        for cmd in commands {
            assert_eq!(
                NntpCommand::parse(cmd),
                NntpCommand::ArticleByMessageId,
                "Failed for: {}",
                cmd
            );
        }
    }
}
