//! Per-command routing mode handler and command execution
//!
//! This module implements independent per-command routing where each command
//! can be routed to a different backend. It includes the core command execution
//! logic used by all routing modes.

use crate::session::common;
use crate::session::{ClientSession, backend, connection, streaming};
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info};

use crate::command::{CommandAction, CommandHandler, NntpCommand};
use crate::config::RoutingMode;
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
                    self.route_and_execute_command(
                        router,
                        &command,
                        &mut client_write,
                        &mut client_to_backend_bytes,
                        &mut backend_to_client_bytes,
                    )
                    .await?;
                }

                CommandRoutingDecision::RequireAuth => {
                    use crate::protocol::AUTH_REQUIRED_FOR_COMMAND;
                    client_write.write_all(AUTH_REQUIRED_FOR_COMMAND).await?;
                    backend_to_client_bytes =
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
                            client_to_backend_bytes,
                            backend_to_client_bytes,
                        )
                        .await;
                }

                CommandRoutingDecision::Reject => {
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
        router: &BackendSelector,
        command: &str,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        client_to_backend_bytes: &mut ClientToBackendBytes,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<crate::types::BackendId> {
        // Extract message-ID early for cache/availability tracking
        let msg_id = common::extract_message_id(command)
            .and_then(|s| crate::types::MessageId::from_borrowed(s).ok());

        // Check cache for full response (only works if cache_articles=true)
        if let Some(msg_id_ref) = msg_id.as_ref()
            && self.cache_articles
        {
            match self.cache.get(msg_id_ref).await {
                Some(cached) => {
                    client_write.write_all(cached.response()).await?;
                    *backend_to_client_bytes = backend_to_client_bytes.add(cached.response().len());

                    let backend_id = router.route_command(self.client_id, command)?;
                    router.complete_command(backend_id);
                    return Ok(backend_id);
                }
                None => {
                    debug!("Cache MISS for message-ID: {}", msg_id_ref);
                }
            }
        }

        // Execute command with availability-aware backend selection
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
        router: &BackendSelector,
        command: &str,
        msg_id: Option<&crate::types::MessageId<'_>>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        client_to_backend_bytes: &mut ClientToBackendBytes,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<crate::types::BackendId> {
        let mut buffer = self.buffer_pool.acquire().await;

        // Initialize availability tracker from cache
        let mut availability = self.load_article_availability(msg_id, router).await;

        // Track last 430 response to return if all backends fail
        let mut last_430_response: Option<Vec<u8>> = None;
        let mut last_430_backend: Option<crate::types::BackendId> = None;

        // Try backends until success or exhaustion
        while !availability.all_exhausted(router.backend_count()) {
            match self
                .try_backend_for_article(
                    router,
                    command,
                    msg_id,
                    client_write,
                    &mut availability,
                    &mut buffer,
                    client_to_backend_bytes,
                )
                .await?
            {
                BackendAttemptResult::Success {
                    backend_id,
                    bytes_written,
                } => {
                    *backend_to_client_bytes = backend_to_client_bytes.add(bytes_written as usize);
                    router.complete_command(backend_id);
                    return Ok(backend_id);
                }
                BackendAttemptResult::ArticleNotFound {
                    backend_id,
                    response,
                } => {
                    last_430_response = Some(response);
                    last_430_backend = Some(backend_id);
                }
                BackendAttemptResult::BackendUnavailable => {
                    // Continue to next backend
                }
            }
        }

        // All backends exhausted - send final 430
        self.send_final_430_response(client_write, backend_to_client_bytes, last_430_response)
            .await?;

        Ok(last_430_backend.expect("must have tried at least one backend"))
    }

    /// Load article availability from cache or create fresh tracker
    async fn load_article_availability(
        &self,
        msg_id: Option<&crate::types::MessageId<'_>>,
        router: &BackendSelector,
    ) -> crate::cache::ArticleAvailability {
        match msg_id {
            Some(msg_id_ref) => self
                .cache
                .get(msg_id_ref)
                .await
                .map(|entry| entry.to_availability(router.backend_count()))
                .unwrap_or_default(),
            None => crate::cache::ArticleAvailability::new(),
        }
    }

    /// Try executing command on next available backend
    #[allow(clippy::too_many_arguments)]
    async fn try_backend_for_article(
        &self,
        router: &BackendSelector,
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
        let (n, response_code, is_multiline, ttfb, send, recv) = match self
            .execute_and_get_first_chunk(&mut conn, backend_id, command, buffer)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                self.handle_backend_error(backend_id, router);
                crate::pool::remove_from_pool(conn);
                return Err(e);
            }
        };

        self.record_timing_metrics(backend_id, ttfb, send, recv);
        *client_to_backend_bytes = client_to_backend_bytes.add(command.len());

        // Handle 430 - article not found
        if self.is_430_response(&response_code) {
            self.handle_430_response(backend_id, msg_id, router, availability);
            return Ok(BackendAttemptResult::ArticleNotFound {
                backend_id,
                response: buffer[..n].to_vec(),
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
                self.handle_backend_error(backend_id, router);
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
        self.record_command(backend_id);
        self.user_command();

        backend::send_command_and_read_first_chunk(
            &mut **pooled_conn,
            command,
            backend_id,
            self.client_addr,
            buffer,
        )
        .await
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
        router: &BackendSelector,
        availability: &mut crate::cache::ArticleAvailability,
    ) {
        availability.record_missing(backend_id);

        if let Some(msg_id_ref) = msg_id {
            let cache_clone = self.cache.clone();
            let msg_id_owned = msg_id_ref.to_owned();
            tokio::spawn(async move {
                cache_clone
                    .record_backend_missing(msg_id_owned, backend_id)
                    .await;
            });
        }

        if let Some(ref metrics) = self.metrics {
            metrics.record_error_4xx(backend_id);
        }

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
