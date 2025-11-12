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
use tracing::{debug, error, info, warn};

use crate::command::{CommandHandler, NntpCommand};
use crate::config::RoutingMode;
use crate::constants::buffer::COMMAND;
use crate::protocol::{BACKEND_ERROR, PROXY_GREETING_PCR};
use crate::router::BackendSelector;
use crate::types::{BytesTransferred, TransferMetrics};

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
        let mut client_reader = BufReader::new(client_read);

        let mut client_to_backend_bytes = BytesTransferred::zero();
        let mut backend_to_client_bytes = BytesTransferred::zero();

        // Auth state: username from AUTHINFO USER command
        let mut auth_username: Option<String> = None;

        // Send initial greeting to client and flush immediately
        // This ensures the client receives the greeting before we start reading commands
        debug!(
            "Client {} sending greeting: {} | hex: {:02x?}",
            self.client_addr,
            String::from_utf8_lossy(PROXY_GREETING_PCR),
            PROXY_GREETING_PCR
        );

        if let Err(e) = client_write.write_all(PROXY_GREETING_PCR).await {
            debug!(
                "Client {} failed to send greeting: {} (kind: {:?}). \
                 This suggests the client disconnected immediately after connecting.",
                self.client_addr,
                e,
                e.kind()
            );
            return Err(e.into());
        }

        if let Err(e) = client_write.flush().await {
            debug!(
                "Client {} failed to flush greeting: {} (kind: {:?})",
                self.client_addr,
                e,
                e.kind()
            );
            return Err(e.into());
        }

        backend_to_client_bytes.add(PROXY_GREETING_PCR.len());

        debug!(
            "Client {} sent greeting successfully, entering command loop",
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
                        &e,
                        TransferMetrics {
                            client_to_backend: client_to_backend_bytes,
                            backend_to_client: backend_to_client_bytes,
                        },
                    );
                    break;
                }
            };

            client_to_backend_bytes.add(n);
            let trimmed = command.trim();

            debug!(
                "Client {} received command ({} bytes): {} | hex: {:02x?}",
                self.client_addr,
                n,
                trimmed,
                command.as_bytes()
            );

            // Handle QUIT locally
            match common::handle_quit_command(&command, &mut client_write).await? {
                common::QuitStatus::Quit(bytes) => {
                    backend_to_client_bytes += bytes;
                    debug!("Client {} sent QUIT, closing connection", self.client_addr);
                    break;
                }
                common::QuitStatus::Continue => {
                    // Not a QUIT, continue processing
                }
            }

            let action = CommandHandler::handle_command(&command);

            // ALWAYS intercept auth commands, even when auth is disabled
            // Auth commands must NEVER be forwarded to backend
            if matches!(action, CommandAction::InterceptAuth(_)) {
                match action {
                    CommandAction::InterceptAuth(auth_action) => {
                        let result = common::handle_auth_command(
                            &self.auth_handler,
                            auth_action,
                            &mut client_write,
                            &mut auth_username,
                            &self.authenticated,
                        )
                        .await?;

                        backend_to_client_bytes += result.bytes_written;
                        if result.authenticated {
                            skip_auth_check = true;
                        }
                    }
                    _ => unreachable!(),
                }
                continue;
            }

            // PERFORMANCE OPTIMIZATION: Fast path after authentication
            // Once authenticated, skip classification (just route everything)
            // Cache check to avoid atomic load on hot path
            skip_auth_check = skip_auth_check
                || self
                    .authenticated
                    .load(std::sync::atomic::Ordering::Acquire);
            if skip_auth_check {
                // Already authenticated - just route the command (HOT PATH after auth)
                self.route_command_with_error_handling(
                    router,
                    &command,
                    &mut client_write,
                    &mut client_to_backend_bytes,
                    &mut backend_to_client_bytes,
                    trimmed,
                )
                .await?;
                continue;
            }

            // Not yet authenticated - need to check for auth/stateful commands

            // In hybrid mode, stateful commands trigger a switch to stateful connection
            if self.routing_mode == RoutingMode::Hybrid
                && matches!(action, CommandAction::Reject(_))
                && NntpCommand::classify(&command).is_stateful()
            {
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

            // Handle the command - inline for performance
            // Check ForwardStateless FIRST (70%+ of traffic)
            use crate::command::CommandAction;
            match action {
                CommandAction::ForwardStateless => {
                    // Check if auth is required but not completed
                    if self.auth_handler.is_enabled() {
                        // Reject all non-auth commands before authentication
                        use crate::protocol::AUTH_REQUIRED_FOR_COMMAND;
                        client_write.write_all(AUTH_REQUIRED_FOR_COMMAND).await?;
                        backend_to_client_bytes.add(AUTH_REQUIRED_FOR_COMMAND.len());
                    } else {
                        // Auth disabled - forward to backend via router (HOT PATH - 70%+ of commands)
                        self.route_command_with_error_handling(
                            router,
                            &command,
                            &mut client_write,
                            &mut client_to_backend_bytes,
                            &mut backend_to_client_bytes,
                            trimmed,
                        )
                        .await?;
                    }
                }
                CommandAction::Reject(response) => {
                    // Send rejection response inline
                    client_write.write_all(response.as_bytes()).await?;
                    backend_to_client_bytes.add(response.len());
                }
                CommandAction::InterceptAuth(_) => {
                    // Already handled above before fast path check
                    unreachable!("Auth commands should be handled before reaching here");
                }
            }
        }

        // Log session summary for debugging, especially useful for test connections
        if (client_to_backend_bytes + backend_to_client_bytes).as_u64()
            < common::SMALL_TRANSFER_THRESHOLD
        {
            debug!(
                "Session summary {} | ↑{} ↓{} | Short session (likely test connection)",
                self.client_addr,
                crate::formatting::format_bytes(client_to_backend_bytes.as_u64()),
                crate::formatting::format_bytes(backend_to_client_bytes.as_u64())
            );
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
        client_to_backend_bytes: &mut BytesTransferred,
        backend_to_client_bytes: &mut BytesTransferred,
    ) -> Result<crate::types::BackendId> {
        use crate::pool::{is_connection_error, remove_from_pool};

        // Get reusable buffer from pool (eliminates 64KB Vec allocation on every command!)
        let mut buffer = self.buffer_pool.get_buffer().await;

        // Route the command to get a backend (lock-free!)
        let backend_id = router.route_command_sync(self.client_id, command)?;

        debug!(
            "Client {} routed command to backend {:?}: {}",
            self.client_addr,
            backend_id,
            command.trim()
        );

        // Get a connection from the router's backend pool
        let Some(provider) = router.get_backend_provider(backend_id) else {
            anyhow::bail!("Backend {:?} not found", backend_id);
        };

        debug!(
            "Client {} getting pooled connection for backend {:?}",
            self.client_addr, backend_id
        );

        let mut pooled_conn = provider.get_pooled_connection().await?;

        debug!(
            "Client {} got pooled connection for backend {:?}",
            self.client_addr, backend_id
        );

        // Record command execution in metrics
        if let Some(ref metrics) = self.metrics {
            metrics.record_command(backend_id.as_index());
        }

        // Execute the command - returns (result, got_backend_data, unrecorded_cmd_bytes, unrecorded_resp_bytes)
        // If got_backend_data is true, we successfully communicated with backend
        let (result, got_backend_data, cmd_bytes, resp_bytes) = self
            .execute_command_on_backend(
                &mut pooled_conn,
                command,
                client_write,
                backend_id,
                client_to_backend_bytes,
                backend_to_client_bytes,
                &mut buffer, // Pass reusable buffer
            )
            .await;

        // Record metrics ONCE using type-safe API (prevents double-counting)
        if let Some(ref metrics) = self.metrics {
            // Record per-backend metrics first (peek without consuming)
            metrics.record_client_to_backend_bytes_for(backend_id.as_index(), cmd_bytes.peek());
            metrics.record_backend_to_client_bytes_for(backend_id.as_index(), resp_bytes.peek());
            
            // Then record global metrics (consumes the Unrecorded bytes)
            let _ = metrics.record_client_to_backend(cmd_bytes);
            let _ = metrics.record_backend_to_client(resp_bytes);
        }

        // Return buffer to pool before handling result

        // Only remove backend connection if error occurred AND we didn't get data from backend
        // If we got data from backend, then any error is from writing to client
        let _ = result
            .as_ref()
            .err()
            .filter(|e| is_connection_error(e))
            .inspect(|e| {
                match got_backend_data {
                    true => debug!(
                        "Client {} disconnected while receiving data from backend {:?} - backend connection is healthy",
                        self.client_addr, backend_id
                    ),
                    false => {
                        warn!(
                            "Backend connection error for client {}, backend {:?}: {} - removing connection from pool",
                            self.client_addr, backend_id, e
                        );
                        // Record error in metrics
                        if let Some(ref metrics) = self.metrics {
                            metrics.record_error(backend_id.as_index());
                        }
                    }
                }
            })
            .filter(|_| !got_backend_data)
            .is_some_and(|_| { remove_from_pool(pooled_conn); true });

        // Complete the request - decrement pending count (lock-free!)
        router.complete_command_sync(backend_id);

        result.map(|_| backend_id)
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
        let (n, _response_code, is_multiline) = match backend::send_command_and_read_first_chunk(
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

        client_to_backend_bytes.add(command.len());

        // Extract message-ID from command if present (for correlation with SABnzbd errors)
        let msgid = common::extract_message_id(command);

        // For multiline responses, use pipelined streaming
        let bytes_written = if is_multiline {
            let log_msg = if let Some(id) = msgid {
                format!(
                    "Client {} ARTICLE {} → multiline ({:?}), streaming {}",
                    self.client_addr,
                    id,
                    _response_code.status_code(),
                    crate::formatting::format_bytes(n as u64)
                )
            } else {
                format!(
                    "Client {} '{}' → multiline ({:?}), streaming {}",
                    self.client_addr,
                    command.trim(),
                    _response_code.status_code(),
                    crate::formatting::format_bytes(n as u64)
                )
            };
            debug!("{}", log_msg);

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
            let log_msg = if let Some(id) = msgid {
                // 223 (No such article number), 430 (No such article), and other 4xx/5xx errors are expected single-line responses
                if let Some(code) = _response_code.status_code() {
                    let raw_code = code.as_u16();
                    if raw_code == 223 || (400..600).contains(&raw_code) {
                        format!(
                            "Client {} ARTICLE {} → error {} (single-line), writing {}",
                            self.client_addr,
                            id,
                            code,
                            crate::formatting::format_bytes(n as u64)
                        )
                    } else {
                        format!(
                            "Client {} ARTICLE {} → UNUSUAL single-line ({:?}), writing {}: {:02x?}",
                            self.client_addr,
                            id,
                            _response_code.status_code(),
                            crate::formatting::format_bytes(n as u64),
                            &chunk_buffer[..n.min(50)]
                        )
                    }
                } else {
                    format!(
                        "Client {} ARTICLE {} → UNUSUAL single-line ({:?}), writing {}: {:02x?}",
                        self.client_addr,
                        id,
                        _response_code.status_code(),
                        crate::formatting::format_bytes(n as u64),
                        &chunk_buffer[..n.min(50)]
                    )
                }
            } else {
                format!(
                    "Client {} '{}' → single-line ({:?}), writing {}: {:02x?}",
                    self.client_addr,
                    command.trim(),
                    _response_code.status_code(),
                    crate::formatting::format_bytes(n as u64),
                    &chunk_buffer[..n.min(50)]
                )
            };

            // Only warn if it's truly unusual (not 223 or 4xx/5xx error responses)
            if let Some(code) = _response_code.status_code() {
                let raw_code = code.as_u16();
                if raw_code == 223 || code.is_error() {
                    debug!("{}", log_msg); // 223 and errors are expected, just debug
                } else if msgid.is_some() {
                    warn!("{}", log_msg); // ARTICLE with other 2xx/3xx single-line is unusual
                } else {
                    debug!("{}", log_msg);
                }
            } else {
                debug!("{}", log_msg);
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

    /// Route a command and handle any errors with appropriate logging and client responses
    ///
    /// This helper consolidates the error handling logic that was duplicated in multiple places.
    /// It routes the command via the router, and if an error occurs, it:
    /// - Classifies the error (client disconnect vs auth error vs other)
    /// - Logs appropriately based on error type
    /// - Sends BACKEND_ERROR response to client (if not disconnected)
    /// - Includes debug logging for small transfers
    async fn route_command_with_error_handling(
        &self,
        router: &BackendSelector,
        command: &str,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        client_to_backend_bytes: &mut BytesTransferred,
        backend_to_client_bytes: &mut BytesTransferred,
        trimmed: &str,
    ) -> Result<()> {
        if let Err(e) = self
            .route_and_execute_command(
                router,
                command,
                client_write,
                client_to_backend_bytes,
                backend_to_client_bytes,
            )
            .await
        {
            use crate::session::error_classification::ErrorClassifier;
            let (up, down) = (
                crate::formatting::format_bytes(client_to_backend_bytes.as_u64()),
                crate::formatting::format_bytes(backend_to_client_bytes.as_u64()),
            );

            // Log based on error type, send error response if client still connected
            if ErrorClassifier::is_client_disconnect(&e) {
                debug!(
                    "Client {} command '{}' resulted in disconnect (already logged by streaming layer) | ↑{} ↓{}",
                    self.client_addr, trimmed, up, down
                );
            } else {
                if ErrorClassifier::is_authentication_error(&e) {
                    error!(
                        "Client {} command '{}' authentication error: {} | ↑{} ↓{}",
                        self.client_addr, trimmed, e, up, down
                    );
                } else {
                    warn!(
                        "Client {} error routing '{}': {} | ↑{} ↓{}",
                        self.client_addr, trimmed, e, up, down
                    );
                }
                let _ = client_write.write_all(BACKEND_ERROR).await;
                backend_to_client_bytes.add(BACKEND_ERROR.len());
            }

            // Debug logging for small transfers
            if (*client_to_backend_bytes + *backend_to_client_bytes).as_u64()
                < common::SMALL_TRANSFER_THRESHOLD
            {
                debug!(
                    "ERROR SUMMARY for small transfer - Client {}: Command '{}' failed with {}. \
                     Total session: {} bytes to backend, {} bytes from backend. \
                     This appears to be a short session (test connection?). \
                     Check debug logs above for full command/response hex dumps.",
                    self.client_addr, trimmed, e, client_to_backend_bytes, backend_to_client_bytes
                );
            }

            return Err(e);
        }
        Ok(())
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
}
