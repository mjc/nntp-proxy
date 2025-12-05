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
        use crate::types::MetricsBytes;

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

        // Get reusable buffer from pool
        let mut buffer = self.buffer_pool.acquire().await;

        // Track 430 responses in case all backends fail
        let mut last_430_response: Option<Vec<u8>> = None;

        // Track article availability across backends (which ones DON'T have it)
        // Default: assume all backends have the article (bitset = 0b00000000)
        // When a backend returns 430, mark it as missing (set the bit)
        // Exit when: article found OR all backends tried and returned 430
        let mut availability = crate::cache::ArticleAvailability::new();

        // Try backends until we get a non-430 response
        loop {
            // Route to next backend
            let backend_id = router.route_command(self.client_id, command)?;

            // Check if we should try this backend (haven't tried it yet OR it didn't return 430)
            if !availability.should_try(backend_id) {
                // This backend already returned 430 - check if all backends exhausted
                let backend_count = router.backend_count();
                if availability.all_exhausted(backend_count) {
                    if let Some(response) = last_430_response {
                        client_write.write_all(&response).await?;
                        *backend_to_client_bytes = backend_to_client_bytes.add(response.len());
                    }
                    anyhow::bail!("Article not found on any backend");
                }
                // Some backends still untried - continue looping to get next backend
                continue;
            }

            // Get connection from pool
            let Some(provider) = router.backend_provider(backend_id) else {
                anyhow::bail!("Backend {:?} not found", backend_id);
            };

            let mut pooled_conn = provider.get_pooled_connection().await?;

            // Record command execution
            self.record_command(backend_id);
            self.user_command();

            // Send command and read first chunk
            let (n, response_code, is_multiline, ttfb_micros, send_micros, recv_micros) =
                match backend::send_command_and_read_first_chunk(
                    &mut *pooled_conn,
                    command,
                    backend_id,
                    self.client_addr,
                    &mut buffer,
                )
                .await
                {
                    Ok(result) => result,
                    Err(e) => {
                        if let Some(ref metrics) = self.metrics {
                            metrics.record_error(backend_id);
                            metrics.user_error(self.username().as_deref());
                        }
                        crate::pool::remove_from_pool(pooled_conn);
                        router.complete_command(backend_id);
                        return Err(e);
                    }
                };

            // Record timing metrics
            if let Some(ref metrics) = self.metrics {
                metrics.record_ttfb_micros(backend_id, ttfb_micros);
                metrics.record_send_recv_micros(backend_id, send_micros, recv_micros);
            }

            *client_to_backend_bytes = client_to_backend_bytes.add(command.len());

            // Check for 430 (article not found) - retry with next backend
            if let Some(code) = response_code.status_code()
                && code.as_u16() == 430
            {
                // Record this backend as NOT having the article
                availability.record_missing(backend_id);

                // Track availability in cache: this backend doesn't have it
                if let Some(ref msg_id_ref) = msg_id {
                    let cache_clone = self.cache.clone();
                    let msg_id_owned = msg_id_ref.to_owned();
                    tokio::spawn(async move {
                        cache_clone
                            .record_backend_missing(msg_id_owned, backend_id)
                            .await;
                    });
                }

                // Track 430 as 4xx error for this backend
                if let Some(ref metrics) = self.metrics {
                    metrics.record_error_4xx(backend_id);
                }

                router.complete_command(backend_id);

                // Save the 430 response in case this is the last backend
                // We'll send it to client only if all backends fail
                last_430_response = Some(buffer[..n].to_vec());

                // Try next backend (router will round-robin to next one)
                continue;
            }

            // Non-430 response - this is the final result
            // Determine if we should capture the full body (only for ARTICLE/HEAD/BODY)
            let should_capture = self.cache_articles
                && is_multiline
                && matches!(NntpCommand::parse(command), NntpCommand::ArticleByMessageId)
                && msg_id.is_some()
                && response_code
                    .status_code()
                    .is_some_and(|c| matches!(c.as_u16(), 220..=222));

            // Stream response to client
            let bytes_written = if is_multiline {
                if should_capture {
                    // Capture and cache the article body (upsert with full response)
                    let mut captured = Vec::with_capacity(buffer.capacity());
                    match streaming::stream_and_capture_multiline_response(
                        &mut *pooled_conn,
                        client_write,
                        &buffer[..n],
                        n,
                        self.client_addr,
                        backend_id,
                        &self.buffer_pool,
                        &mut captured,
                    )
                    .await
                    {
                        Ok(bytes) => {
                            // Upsert into cache asynchronously (creates or updates entry)
                            if let Some(msg_id_ref) = msg_id.as_ref() {
                                let cache_clone = self.cache.clone();
                                let msg_id_owned = msg_id_ref.to_owned();
                                tokio::spawn(async move {
                                    cache_clone.upsert(msg_id_owned, captured, backend_id).await;
                                });
                            }
                            bytes
                        }
                        Err(e) => {
                            if let Some(ref metrics) = self.metrics {
                                metrics.record_error(backend_id);
                                metrics.user_error(self.username().as_deref());
                            }
                            crate::pool::remove_from_pool(pooled_conn);
                            router.complete_command(backend_id);
                            return Err(e);
                        }
                    }
                } else {
                    // Stream without capturing for cache_articles=false
                    let should_track = msg_id.is_some()
                        && response_code
                            .status_code()
                            .is_some_and(|c| matches!(c.as_u16(), 220..=223));

                    let bytes_result = streaming::stream_multiline_response(
                        &mut *pooled_conn,
                        client_write,
                        &buffer[..n],
                        n,
                        self.client_addr,
                        backend_id,
                        &self.buffer_pool,
                    )
                    .await;

                    match bytes_result {
                        Ok(bytes) => {
                            // Pass status code to cache - it will create minimal stub
                            if should_track && let Some(msg_id_ref) = msg_id.as_ref() {
                                let cache_clone = self.cache.clone();
                                let msg_id_owned = msg_id_ref.to_owned();
                                // Pass response buffer (cache will strip to stub internally)
                                let buffer_for_cache = buffer[..n].to_vec();
                                tokio::spawn(async move {
                                    cache_clone
                                        .upsert(msg_id_owned, buffer_for_cache, backend_id)
                                        .await;
                                });
                            }
                            bytes
                        }
                        Err(e) => {
                            if let Some(ref metrics) = self.metrics {
                                metrics.record_error(backend_id);
                                metrics.user_error(self.username().as_deref());
                            }
                            crate::pool::remove_from_pool(pooled_conn);
                            router.complete_command(backend_id);
                            return Err(e);
                        }
                    }
                }
            } else {
                // Single-line response (e.g., STAT 223)
                let should_track = msg_id.is_some()
                    && response_code
                        .status_code()
                        .is_some_and(|c| c.as_u16() == 223);

                match client_write.write_all(&buffer[..n]).await {
                    Ok(_) => {
                        // Track STAT responses
                        if should_track && let Some(msg_id_ref) = msg_id.as_ref() {
                            let cache_clone = self.cache.clone();
                            let msg_id_owned = msg_id_ref.to_owned();
                            let buffer_for_cache = b"223\r\n".to_vec();
                            tokio::spawn(async move {
                                cache_clone
                                    .upsert(msg_id_owned, buffer_for_cache, backend_id)
                                    .await;
                            });
                        }
                        n as u64
                    }
                    Err(e) => {
                        if let Some(ref metrics) = self.metrics {
                            metrics.record_error(backend_id);
                            metrics.user_error(self.username().as_deref());
                        }
                        router.complete_command(backend_id);
                        return Err(e.into());
                    }
                }
            };

            *backend_to_client_bytes = backend_to_client_bytes.add(bytes_written as usize);

            // Track metrics based on response code
            if let Some(ref metrics) = self.metrics
                && let Some(code) = response_code.status_code()
            {
                let raw_code = code.as_u16();

                // Track 4xx/5xx errors (excluding expected "not found" responses)
                if (400..500).contains(&raw_code) && raw_code != 423 && raw_code != 430 {
                    metrics.record_error_4xx(backend_id);
                } else if raw_code >= 500 {
                    metrics.record_error_5xx(backend_id);
                }

                // Track article size for successful article retrieval
                if is_multiline && matches!(raw_code, 220..=222) {
                    metrics.record_article(backend_id, bytes_written);
                }
            }

            // Record metrics
            if let Some(ref metrics) = self.metrics {
                let cmd_bytes = MetricsBytes::new(command.len() as u64);
                let resp_bytes = MetricsBytes::new(bytes_written);
                let _ = metrics.record_command_execution(backend_id, cmd_bytes, resp_bytes);
                self.user_bytes_sent(command.len() as u64);
                self.user_bytes_received(bytes_written);
            }

            router.complete_command(backend_id);

            return Ok(backend_id);
        }
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
                Ok(result) => result,
                Err(e) => {
                    return (Err(e), false, MetricsBytes::new(0), MetricsBytes::new(0));
                }
            };

        // Record timing metrics
        if let Some(ref metrics) = self.metrics {
            metrics.record_ttfb_micros(backend_id, ttfb_micros);
            metrics.record_send_recv_micros(backend_id, send_micros, recv_micros);
        }

        *client_to_backend_bytes = client_to_backend_bytes.add(command.len());

        // Extract message-ID for correlation
        let msgid = common::extract_message_id(command);

        // Determine if we should capture the full body
        let should_capture = self.cache_articles
            && is_multiline
            && matches!(NntpCommand::parse(command), NntpCommand::ArticleByMessageId)
            && msgid.is_some()
            && response_code
                .status_code()
                .is_some_and(|c| matches!(c.as_u16(), 220..=222));

        // Stream response to client
        let bytes_written = if is_multiline {
            if should_capture {
                // Capture and cache the article body
                let mut captured = Vec::with_capacity(chunk_buffer.capacity());
                match streaming::stream_and_capture_multiline_response(
                    &mut **pooled_conn,
                    client_write,
                    &chunk_buffer[..n],
                    n,
                    self.client_addr,
                    backend_id,
                    &self.buffer_pool,
                    &mut captured,
                )
                .await
                {
                    Ok(bytes) => {
                        // Insert into cache asynchronously (full body)
                        if let Some(msg_id_str) = msgid {
                            let cache_clone = self.cache.clone();
                            let msg_id_string = msg_id_str.to_string();
                            tokio::spawn(async move {
                                if let Ok(msg_id) = crate::types::MessageId::new(msg_id_string) {
                                    cache_clone.upsert(msg_id, captured, backend_id).await;
                                }
                            });
                        }
                        bytes
                    }
                    Err(e) => {
                        return (
                            Err(e),
                            true,
                            MetricsBytes::new(command.len() as u64),
                            MetricsBytes::new(0),
                        );
                    }
                }
            } else {
                // Stream without capturing - track availability only
                let should_track_availability = msgid.is_some()
                    && response_code
                        .status_code()
                        .is_some_and(|c| matches!(c.as_u16(), 220..=223));

                match streaming::stream_multiline_response(
                    &mut **pooled_conn,
                    client_write,
                    &chunk_buffer[..n],
                    n,
                    self.client_addr,
                    backend_id,
                    &self.buffer_pool,
                )
                .await
                {
                    Ok(bytes) => {
                        // Track availability - pass first chunk buffer to cache
                        if should_track_availability && let Some(msg_id_str) = msgid {
                            let cache_clone = self.cache.clone();
                            let msg_id_string = msg_id_str.to_string();
                            let buffer_for_cache = chunk_buffer[..n].to_vec();
                            tokio::spawn(async move {
                                if let Ok(msg_id) = crate::types::MessageId::new(msg_id_string) {
                                    cache_clone
                                        .upsert(msg_id, buffer_for_cache, backend_id)
                                        .await;
                                }
                            });
                        }
                        bytes
                    }
                    Err(e) => {
                        return (
                            Err(e),
                            true,
                            MetricsBytes::new(command.len() as u64),
                            MetricsBytes::new(0),
                        );
                    }
                }
            }
        } else {
            // Single-line response (e.g., STAT 223)
            let should_track_availability = msgid.is_some()
                && response_code
                    .status_code()
                    .is_some_and(|c| c.as_u16() == 223);

            match client_write.write_all(&chunk_buffer[..n]).await {
                Ok(_) => {
                    // Track STAT responses for availability
                    if should_track_availability && let Some(msg_id_str) = msgid {
                        let cache_clone = self.cache.clone();
                        let msg_id_string = msg_id_str.to_string();
                        let buffer_for_cache = chunk_buffer[..n].to_vec();
                        tokio::spawn(async move {
                            if let Ok(msg_id) = crate::types::MessageId::new(msg_id_string) {
                                cache_clone
                                    .upsert(msg_id, buffer_for_cache, backend_id)
                                    .await;
                            }
                        });
                    }
                    n as u64
                }
                Err(e) => {
                    return (
                        Err(e.into()),
                        true,
                        MetricsBytes::new(command.len() as u64),
                        MetricsBytes::new(0),
                    );
                }
            }
        };

        *backend_to_client_bytes = backend_to_client_bytes.add(bytes_written as usize);

        // Track metrics based on response code
        if let Some(ref metrics) = self.metrics
            && let Some(code) = response_code.status_code()
        {
            let raw_code = code.as_u16();

            // Track 4xx/5xx errors (excluding 423 which is group not selected)
            if (400..500).contains(&raw_code) && raw_code != 423 {
                metrics.record_error_4xx(backend_id);
            } else if raw_code >= 500 {
                metrics.record_error_5xx(backend_id);
            }

            // Track article size for successful article retrieval
            if is_multiline && matches!(raw_code, 220..=222) {
                metrics.record_article(backend_id, bytes_written);
            }
        }

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
