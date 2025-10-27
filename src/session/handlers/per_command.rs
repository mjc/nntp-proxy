//! Per-command routing mode handler and command execution
//!
//! This module implements independent per-command routing where each command
//! can be routed to a different backend. It includes the core command execution
//! logic used by all routing modes.

use super::common::{SMALL_TRANSFER_THRESHOLD, extract_message_id};
use crate::session::{ClientSession, backend, connection, streaming};
use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

use crate::auth::AuthHandler;
use crate::command::{AuthAction, CommandAction, CommandHandler, NntpCommand};
use crate::config::RoutingMode;
use crate::constants::buffer::COMMAND;
use crate::protocol::{
    BACKEND_ERROR, COMMAND_NOT_SUPPORTED_STATELESS, CONNECTION_CLOSING, PROXY_GREETING_PCR,
};
use crate::router::BackendSelector;
use crate::types::{BytesTransferred, TransferMetrics};

impl ClientSession {
    /// Handle a client connection with per-command routing
    /// Each command is routed independently to potentially different backends
    pub async fn handle_per_command_routing(
        &self,
        mut client_stream: TcpStream,
    ) -> Result<(u64, u64)> {
        use tokio::io::BufReader;

        debug!(
            "Client {} starting per-command routing session",
            self.client_addr
        );

        let router = self
            .router
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Per-command routing mode requires a router"))?;

        let (client_read, mut client_write) = client_stream.split();
        let mut client_reader = BufReader::new(client_read);

        let mut client_to_backend_bytes = BytesTransferred::zero();
        let mut backend_to_client_bytes = BytesTransferred::zero();

        // Send initial greeting to client
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
        backend_to_client_bytes.add(PROXY_GREETING_PCR.len());

        debug!(
            "Client {} sent greeting successfully, entering command loop",
            self.client_addr
        );

        // Reuse command buffer to avoid allocations per command
        let mut command = String::with_capacity(COMMAND);

        // Process commands one at a time
        loop {
            command.clear();

            match client_reader.read_line(&mut command).await {
                Ok(0) => {
                    debug!("Client {} disconnected", self.client_addr);
                    break; // Client disconnected
                }
                Ok(n) => {
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
                    if trimmed.eq_ignore_ascii_case("QUIT") {
                        // Send closing message - ignore errors if client already disconnected, but log for debugging
                        if let Err(e) = client_write.write_all(CONNECTION_CLOSING).await {
                            debug!(
                                "Failed to write CONNECTION_CLOSING to client {}: {}",
                                self.client_addr, e
                            );
                        }
                        backend_to_client_bytes.add(CONNECTION_CLOSING.len());
                        debug!("Client {} sent QUIT, closing connection", self.client_addr);
                        break;
                    }

                    // Check if command should be rejected or if we need to switch modes in hybrid routing
                    match CommandHandler::handle_command(&command) {
                        CommandAction::InterceptAuth(auth_action) => {
                            // Handle authentication locally
                            let response = match auth_action {
                                AuthAction::RequestPassword => AuthHandler::user_response(),
                                AuthAction::AcceptAuth => AuthHandler::pass_response(),
                            };
                            client_write.write_all(response).await?;
                            backend_to_client_bytes.add(response.len());
                        }
                        CommandAction::Reject(reason) => {
                            // In hybrid mode, stateful commands trigger a switch to stateful connection
                            if self.routing_mode == RoutingMode::Hybrid
                                && NntpCommand::classify(&command).is_stateful()
                            {
                                info!(
                                    "Client {} switching to stateful mode (command: {})",
                                    self.client_addr, trimmed
                                );

                                // Switch to stateful mode - acquire a dedicated backend connection
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

                            // In non-hybrid per-command mode, reject stateful commands
                            warn!(
                                "Rejecting command from client {}: {} ({})",
                                self.client_addr, trimmed, reason
                            );
                            client_write
                                .write_all(COMMAND_NOT_SUPPORTED_STATELESS)
                                .await?;
                            backend_to_client_bytes.add(COMMAND_NOT_SUPPORTED_STATELESS.len());
                        }
                        CommandAction::ForwardStateless => {
                            // Route this command to a backend
                            match self
                                .route_and_execute_command(
                                    router,
                                    &command,
                                    &mut client_write,
                                    &mut client_to_backend_bytes,
                                    &mut backend_to_client_bytes,
                                )
                                .await
                            {
                                Ok(_backend_id) => {}
                                Err(e) => {
                                    use crate::session::error_classification::ErrorClassifier;

                                    // Classify error and handle appropriately
                                    if ErrorClassifier::should_skip_client_error_response(&e) {
                                        // Client disconnected - this is normal for downloads that complete
                                        debug!(
                                            "Client {} disconnected after '{}' | ↑{} ↓{}",
                                            self.client_addr,
                                            trimmed,
                                            crate::formatting::format_bytes(
                                                client_to_backend_bytes.as_u64()
                                            ),
                                            crate::formatting::format_bytes(
                                                backend_to_client_bytes.as_u64()
                                            )
                                        );
                                    } else {
                                        // Log based on error classification
                                        let log_level = ErrorClassifier::log_level(&e);
                                        let message = format!(
                                            "Client {} error routing '{}': {} | ↑{} ↓{}",
                                            self.client_addr,
                                            trimmed,
                                            e,
                                            crate::formatting::format_bytes(
                                                client_to_backend_bytes.as_u64()
                                            ),
                                            crate::formatting::format_bytes(
                                                backend_to_client_bytes.as_u64()
                                            )
                                        );

                                        match log_level {
                                            tracing::Level::ERROR => error!("{}", message),
                                            tracing::Level::WARN => warn!("{}", message),
                                            tracing::Level::INFO => info!("{}", message),
                                            tracing::Level::DEBUG => debug!("{}", message),
                                            _ => debug!("{}", message), // Future-proof for TRACE or other levels
                                        }

                                        // Try to send error response
                                        let _ = client_write.write_all(BACKEND_ERROR).await;
                                        backend_to_client_bytes.add(BACKEND_ERROR.len());
                                    }

                                    // For debugging test connections and small transfers, log detailed info
                                    if (client_to_backend_bytes + backend_to_client_bytes).as_u64()
                                        < SMALL_TRANSFER_THRESHOLD
                                    {
                                        debug!(
                                            "ERROR SUMMARY for small transfer - Client {}: \
                                             Command '{}' failed with {}. \
                                             Total session: {} bytes to backend, {} bytes from backend. \
                                             This appears to be a short session (test connection?). \
                                             Check debug logs above for full command/response hex dumps.",
                                            self.client_addr,
                                            trimmed,
                                            e,
                                            client_to_backend_bytes,
                                            backend_to_client_bytes
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
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
            }
        }

        // Log session summary for debugging, especially useful for test connections
        if (client_to_backend_bytes + backend_to_client_bytes).as_u64() < SMALL_TRANSFER_THRESHOLD {
            debug!(
                "Session summary {} | ↑{} ↓{} | Short session (likely test connection)",
                self.client_addr,
                crate::formatting::format_bytes(client_to_backend_bytes.as_u64()),
                crate::formatting::format_bytes(backend_to_client_bytes.as_u64())
            );
        }

        Ok((
            client_to_backend_bytes.as_u64(),
            backend_to_client_bytes.as_u64(),
        ))
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

        // Route the command to get a backend (lock-free!)
        let backend_id = router.route_command_sync(self.client_id, command)?;

        debug!(
            "Client {} routed command to backend {:?}: {}",
            self.client_addr,
            backend_id,
            command.trim()
        );

        // Get a connection from the router's backend pool
        let provider = router
            .get_backend_provider(backend_id)
            .ok_or_else(|| anyhow::anyhow!("Backend {:?} not found", backend_id))?;

        debug!(
            "Client {} getting pooled connection for backend {:?}",
            self.client_addr, backend_id
        );

        let mut pooled_conn = provider.get_pooled_connection().await?;

        debug!(
            "Client {} got pooled connection for backend {:?}",
            self.client_addr, backend_id
        );

        // Execute the command - returns (result, got_backend_data)
        // If got_backend_data is true, we successfully communicated with backend
        let (result, got_backend_data) = self
            .execute_command_on_backend(
                &mut pooled_conn,
                command,
                client_write,
                backend_id,
                client_to_backend_bytes,
                backend_to_client_bytes,
            )
            .await;

        // Only remove backend connection if error occurred AND we didn't get data from backend
        // If we got data from backend, then any error is from writing to client
        if let Err(ref e) = result {
            if !got_backend_data && is_connection_error(e) {
                warn!(
                    "Backend connection error for client {}, backend {:?}: {} - removing connection from pool",
                    self.client_addr, backend_id, e
                );
                remove_from_pool(pooled_conn);
                router.complete_command_sync(backend_id);
                return result.map(|_| backend_id);
            } else if got_backend_data && is_connection_error(e) {
                debug!(
                    "Client {} disconnected while receiving data from backend {:?} - backend connection is healthy",
                    self.client_addr, backend_id
                );
            }
        }

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
    /// # Return Value
    ///
    /// Returns `(Result<()>, got_backend_data)` where:
    /// - `got_backend_data = true` means we successfully read from backend before any error
    /// - This distinguishes backend failures (remove from pool) from client disconnects (keep backend)
    ///
    /// This function is `pub(super)` and is intended for use by `hybrid.rs` for stateful mode command execution.
    pub(super) async fn execute_command_on_backend(
        &self,
        pooled_conn: &mut deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        command: &str,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_id: crate::types::BackendId,
        client_to_backend_bytes: &mut BytesTransferred,
        backend_to_client_bytes: &mut BytesTransferred,
    ) -> (Result<()>, bool) {
        let mut got_backend_data = false;

        // Send command and read first chunk
        let (chunk, n, _response_code, is_multiline) =
            match backend::send_command_and_read_first_chunk(
                &mut **pooled_conn,
                command,
                backend_id,
                self.client_addr,
            )
            .await
            {
                Ok(result) => {
                    got_backend_data = true;
                    result
                }
                Err(e) => return (Err(e), got_backend_data),
            };

        client_to_backend_bytes.add(command.len());

        // Extract message-ID from command if present (for correlation with SABnzbd errors)
        let msgid = extract_message_id(command);

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
                &chunk,
                n,
                self.client_addr,
                backend_id,
            )
            .await
            {
                Ok(bytes) => bytes,
                Err(e) => return (Err(e), got_backend_data),
            }
        } else {
            // Single-line response - just write the first chunk
            let log_msg = if let Some(id) = msgid {
                // 430 (No such article) and other 4xx errors are expected single-line responses
                if let Some(code) = _response_code.status_code() {
                    if (400..500).contains(&code) {
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
                            &chunk[..n.min(50)]
                        )
                    }
                } else {
                    format!(
                        "Client {} ARTICLE {} → UNUSUAL single-line ({:?}), writing {}: {:02x?}",
                        self.client_addr,
                        id,
                        _response_code.status_code(),
                        crate::formatting::format_bytes(n as u64),
                        &chunk[..n.min(50)]
                    )
                }
            } else {
                format!(
                    "Client {} '{}' → single-line ({:?}), writing {}: {:02x?}",
                    self.client_addr,
                    command.trim(),
                    _response_code.status_code(),
                    crate::formatting::format_bytes(n as u64),
                    &chunk[..n.min(50)]
                )
            };

            // Only warn if it's truly unusual (not a 4xx/5xx error response)
            if let Some(code) = _response_code.status_code() {
                if code >= 400 {
                    debug!("{}", log_msg); // Errors are expected, just debug
                } else if msgid.is_some() {
                    warn!("{}", log_msg); // ARTICLE with 2xx/3xx single-line is unusual
                } else {
                    debug!("{}", log_msg);
                }
            } else {
                debug!("{}", log_msg);
            }

            match client_write.write_all(&chunk[..n]).await {
                Ok(_) => n as u64,
                Err(e) => return (Err(e.into()), got_backend_data),
            }
        };

        backend_to_client_bytes.add(bytes_written as usize);

        if let Some(id) = msgid {
            debug!(
                "Client {} ARTICLE {} completed: wrote {} bytes to client",
                self.client_addr, id, bytes_written
            );
        }

        (Ok(()), got_backend_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_message_id_valid() {
        let command = "BODY <test@example.com>";
        let msgid = extract_message_id(command);
        assert_eq!(msgid, Some("<test@example.com>"));
    }

    #[test]
    fn test_extract_message_id_article_command() {
        let command = "ARTICLE <1234@news.server>";
        let msgid = extract_message_id(command);
        assert_eq!(msgid, Some("<1234@news.server>"));
    }

    #[test]
    fn test_extract_message_id_head_command() {
        let command = "HEAD <article@host.domain>";
        let msgid = extract_message_id(command);
        assert_eq!(msgid, Some("<article@host.domain>"));
    }

    #[test]
    fn test_extract_message_id_stat_command() {
        let command = "STAT <msg@example.org>";
        let msgid = extract_message_id(command);
        assert_eq!(msgid, Some("<msg@example.org>"));
    }

    #[test]
    fn test_extract_message_id_no_brackets() {
        let command = "GROUP comp.lang.rust";
        let msgid = extract_message_id(command);
        assert_eq!(msgid, None);
    }

    #[test]
    fn test_extract_message_id_malformed() {
        let command = "BODY <incomplete";
        let msgid = extract_message_id(command);
        assert_eq!(msgid, None);
    }

    #[test]
    fn test_extract_message_id_with_extra_text() {
        let command = "BODY <msg@host> extra stuff";
        let msgid = extract_message_id(command);
        assert_eq!(msgid, Some("<msg@host>"));
    }

    #[test]
    fn test_extract_message_id_empty_brackets() {
        let command = "BODY <>";
        let msgid = extract_message_id(command);
        assert_eq!(msgid, Some("<>"));
    }

    #[test]
    fn test_extract_message_id_lowercase_command() {
        let command = "body <test@example.com>";
        let msgid = extract_message_id(command);
        assert_eq!(msgid, Some("<test@example.com>"));
    }

    #[test]
    fn test_extract_message_id_mixed_case() {
        let command = "BoDy <TeSt@ExAmPlE.cOm>";
        let msgid = extract_message_id(command);
        assert_eq!(msgid, Some("<TeSt@ExAmPlE.cOm>"));
    }
}
