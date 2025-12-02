//! Per-command routing mode handler and command execution
//!
//! This module implements independent per-command routing where each command
//! can be routed to a different backend. It includes the core command execution
//! logic used by all routing modes.

use crate::pool::PooledBuffer;
use crate::session::common;
use crate::session::{ClientSession, backend, connection, streaming};
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::command::{CommandAction, CommandHandler, NntpCommand};
use crate::config::RoutingMode;
use crate::constants::buffer::{COMMAND, READER_CAPACITY};
use crate::protocol::PROXY_GREETING_PCR;
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

        let mut client_to_backend_bytes = ClientToBackendBytes::zero();
        let mut backend_to_client_bytes = BackendToClientBytes::zero();

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

            let action = CommandHandler::classify(&command);
            skip_auth_check = self.is_authenticated_cached(skip_auth_check);

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

                    backend_to_client_bytes = backend_to_client_bytes.add_u64(
                        match common::handle_auth_command(
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
        // Check cache early - returns Some(backend_id) on cache hit, None otherwise
        if let Some(backend_id) = self
            .check_cache_and_serve(router, command, client_write, backend_to_client_bytes)
            .await?
        {
            return Ok(backend_id);
        }

        // Determine if we need to cache this response after fetching
        let should_cache = self.should_cache_command(command);

        // Get reusable buffer from pool (eliminates 64KB Vec allocation on every command!)
        let mut buffer = self.buffer_pool.acquire().await;

        // Route the command to get a backend (lock-free!)
        let backend_id = router.route_command(self.client_id, command)?;

        // Get a connection from the router's backend pool
        let Some(provider) = router.backend_provider(backend_id) else {
            anyhow::bail!("Backend {:?} not found", backend_id);
        };

        let mut pooled_conn = provider.get_pooled_connection().await?;

        debug!(
            "Client {} got pooled connection for backend {:?}",
            self.client_addr, backend_id
        );

        // Record command execution in metrics
        self.record_command(backend_id);
        self.user_command();

        // Execute the command - returns (result, got_backend_data, unrecorded_cmd_bytes, unrecorded_resp_bytes)
        // If got_backend_data is true, we successfully communicated with backend
        //
        // Use caching path only for cache misses on ARTICLE by message-ID
        let (result, got_backend_data, cmd_bytes, resp_bytes) = if should_cache {
            self.execute_command_with_caching(
                &mut pooled_conn,
                command,
                client_write,
                backend_id,
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
                backend_id,
                client_to_backend_bytes,
                backend_to_client_bytes,
                &mut buffer, // Pass reusable buffer
            )
            .await
        };

        // Record metrics ONCE using type-safe API (prevents double-counting)
        if let Some(ref metrics) = self.metrics {
            // Peek byte counts before consuming
            let cmd_size = cmd_bytes.peek();
            let resp_size = resp_bytes.peek();

            // Record command + per-backend + global bytes in one call
            let _ = metrics.record_command_execution(backend_id, cmd_bytes, resp_bytes);

            // Record per-user metrics
            self.user_bytes_sent(cmd_size);
            self.user_bytes_received(resp_size);
        }

        // Handle errors functionally - remove from pool if backend error
        let result = if !got_backend_data {
            result.inspect_err(|e| {
                if crate::pool::is_connection_error(e) {
                    warn!(
                        "Backend {:?} connection error: {} - removing from pool",
                        backend_id, e
                    );
                    if let Some(ref metrics) = self.metrics {
                        metrics.record_error(backend_id);
                        metrics.user_error(self.username().as_deref());
                    }
                    crate::pool::remove_from_pool(pooled_conn);
                }
            })
        } else {
            result
        };

        // Complete the request - decrement pending count (lock-free!)
        router.complete_command(backend_id);

        result
            .map(|_| backend_id)
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
        client_to_backend_bytes: &mut ClientToBackendBytes,
        backend_to_client_bytes: &mut BackendToClientBytes,
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
        let (n, _response_code, is_multiline, ttfb_micros, send_micros, recv_micros) =
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

        // Record time to first byte and split timing
        if let Some(ref metrics) = self.metrics {
            metrics.record_ttfb_micros(backend_id, ttfb_micros);
            metrics.record_send_recv_micros(backend_id, send_micros, recv_micros);
        }

        *client_to_backend_bytes = client_to_backend_bytes.add(command.len());

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
            if let Some(code) = _response_code.status_code() {
                let raw_code = code.as_u16();
                // Warn only for truly unusual responses (not 223 or errors)
                if msgid.is_some() && raw_code != 223 && !code.is_error() {
                    warn!(
                        "Client {} ARTICLE {} got unusual single-line response: {}",
                        self.client_addr,
                        msgid.unwrap(),
                        code
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

        *backend_to_client_bytes = backend_to_client_bytes.add(bytes_written as usize);

        // Track metrics based on response code
        if let Some(ref metrics) = self.metrics
            && let Some(code) = _response_code.status_code()
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
        client_to_backend_bytes: &mut ClientToBackendBytes,
        backend_to_client_bytes: &mut BackendToClientBytes,
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

        *client_to_backend_bytes = client_to_backend_bytes.add(command.len());

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
                if crate::session::streaming::tail_buffer::has_terminator_at_end(chunk) {
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

        *backend_to_client_bytes = backend_to_client_bytes.add(bytes_written as usize);

        // Cache successful ARTICLE responses (2xx status codes)
        if let Some(ref cache) = self.cache
            && let Some(message_id_str) = crate::session::common::extract_message_id(command)
            && let Ok(message_id) = crate::types::MessageId::from_borrowed(message_id_str)
            && let Some(code) = response_code.status_code()
            && code.is_success()
        {
            info!(
                "Caching response for message-ID: {} ({} bytes)",
                message_id, bytes_written
            );
            cache
                .insert(
                    message_id,
                    crate::cache::CachedArticle {
                        response: std::sync::Arc::new(response_buffer),
                    },
                )
                .await;
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
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<Option<crate::types::BackendId>> {
        // Functional pipeline: cache → is ARTICLE → extract ID
        let Some((cache, message_id)) = self
            .cache
            .as_ref()
            .filter(|_| matches!(NntpCommand::parse(command), NntpCommand::ArticleByMessageId))
            .zip(crate::session::common::extract_message_id(command))
            .and_then(|(cache, msg_id_str)| {
                crate::types::MessageId::from_borrowed(msg_id_str)
                    .ok()
                    .map(|msg_id| (cache, msg_id))
            })
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
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<Option<crate::types::BackendId>> {
        match cache.get(&message_id).await {
            Some(cached) => {
                info!(
                    "Cache HIT for message-ID: {} (size: {} bytes)",
                    message_id,
                    cached.response.len()
                );
                client_write.write_all(&cached.response).await?;
                *backend_to_client_bytes = backend_to_client_bytes.add(cached.response.len());

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
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
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
        let buffer_pool = crate::pool::BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let auth_handler = Arc::new(AuthHandler::new(None, None).unwrap());

        ClientSession::builder(addr, buffer_pool, auth_handler).build()
    }
}
