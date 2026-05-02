//! Per-command routing mode handler and command dispatch
//!
//! This module implements the command loop for per-command routing where each
//! command can be routed to a different backend. The actual routing, backend
//! execution, and cache logic are split into sub-modules:
//!
//! - [`article_retry`]: Availability-aware backend selection and retry logic
//! - [`command_execution`]: Single-backend command execution and response streaming
//! - [`cache_operations`]: Cache lookups, upserts, and tier helpers

use crate::protocol::{RequestContext, RequestResponseMetadata, StatusCode, codes};
use crate::session::common;
use crate::session::routing::{CommandRoutingDecision, decide_request_routing};
use crate::session::{ClientSession, connection};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::command::{CommandAction, CommandHandler};
use crate::constants::buffer::{COMMAND, READER_CAPACITY};
use crate::router::BackendSelector;
use crate::session::SessionError;
use crate::types::{BackendToClientBytes, ClientToBackendBytes, TransferMetrics};

/// Result of executing a routing decision
enum CommandResult {
    /// Continue processing commands
    Continue { auth_succeeded: bool },
    /// Switch to stateful mode (early return from loop)
    SwitchToStateful,
}

/// Result of processing a single command
enum SingleCommandResult {
    /// Continue processing commands
    Continue { auth_succeeded: bool },
    /// Client sent QUIT command (bytes already added to `backend_to_client_bytes`)
    Quit,
    /// Switch to stateful mode (early return from loop)
    SwitchToStateful,
}

/// Parameters for executing a command decision
struct CommandExecutionParams<'a, 'b> {
    request: &'a mut RequestContext,
    skip_auth_check: bool,
    router: &'a Arc<BackendSelector>,
    client_write: &'a mut tokio::net::tcp::WriteHalf<'b>,
    auth_username: &'a mut Option<String>,
    client_to_backend_bytes: ClientToBackendBytes,
    backend_to_client_bytes: &'a mut BackendToClientBytes,
}

/// Parameters for processing a single command (full flow including QUIT handling)
struct ProcessCommandParams<'a, 'b> {
    request: &'a mut RequestContext,
    skip_auth_check: bool,
    router: &'a Arc<BackendSelector>,
    client_write: &'a mut tokio::net::tcp::WriteHalf<'b>,
    auth_username: &'a mut Option<String>,
    client_to_backend_bytes: ClientToBackendBytes,
    backend_to_client_bytes: &'a mut BackendToClientBytes,
}

fn record_local_response(request: &mut RequestContext, status: u16, response: &[u8]) {
    request.record_local_response(RequestResponseMetadata::new(
        StatusCode::new(status),
        response.len().into(),
    ));
}

impl ClientSession {
    /// Execute a command routing decision
    ///
    /// Handles all routing decision types: auth, forwarding, rejection, etc.
    async fn execute_command_decision(
        &self,
        params: CommandExecutionParams<'_, '_>,
    ) -> Result<CommandResult> {
        let CommandExecutionParams {
            request,
            skip_auth_check,
            router,
            client_write,
            auth_username,
            client_to_backend_bytes,
            backend_to_client_bytes,
        } = params;

        let decision = decide_request_routing(
            request,
            skip_auth_check,
            self.auth_handler.is_enabled(),
            self.mode_state.routing_mode(),
        );

        match decision {
            CommandRoutingDecision::InterceptAuth => {
                debug!("Client {} decision: InterceptAuth", self.client_addr);
                let action = CommandHandler::classify_request(request);
                let CommandAction::InterceptAuth(auth_action) = action else {
                    unreachable!("InterceptAuth decision must come from InterceptAuth action")
                };

                let result = common::handle_auth_command(
                    &self.auth_handler,
                    auth_action,
                    client_write,
                    auth_username,
                    &self.auth_state,
                )
                .await?;
                let auth_succeeded = match result {
                    common::AuthResult::Authenticated { .. } => {
                        common::on_authentication_success(
                            self.client_addr,
                            auth_username.clone(),
                            self.mode_state.routing_mode(),
                            &self.metrics,
                            self.connection_stats(),
                            |username| self.set_username(username),
                        );
                        request.record_local_response(result.response_metadata());
                        *backend_to_client_bytes =
                            backend_to_client_bytes.add_u64(result.bytes_written().as_u64());
                        true
                    }
                    common::AuthResult::NotAuthenticated { .. } => {
                        request.record_local_response(result.response_metadata());
                        *backend_to_client_bytes =
                            backend_to_client_bytes.add_u64(result.bytes_written().as_u64());
                        false
                    }
                };

                Ok(CommandResult::Continue { auth_succeeded })
            }

            CommandRoutingDecision::Forward => {
                debug!(
                    "Client {} decision: Forward kind={:?}, verb={:?}",
                    self.client_addr,
                    request.kind(),
                    request.verb()
                );
                let mut c2b_mutable = client_to_backend_bytes;
                self.route_and_execute_request(
                    router.clone(),
                    request,
                    client_write,
                    &mut c2b_mutable,
                    backend_to_client_bytes,
                )
                .await?;
                Ok(CommandResult::Continue {
                    auth_succeeded: false,
                })
            }

            CommandRoutingDecision::RequireAuth => {
                debug!("Client {} decision: RequireAuth", self.client_addr);
                use crate::protocol::AUTH_REQUIRED_FOR_COMMAND;
                client_write.write_all(AUTH_REQUIRED_FOR_COMMAND).await?;
                record_local_response(request, codes::AUTH_REQUIRED, AUTH_REQUIRED_FOR_COMMAND);
                *backend_to_client_bytes =
                    backend_to_client_bytes.add(AUTH_REQUIRED_FOR_COMMAND.len());
                Ok(CommandResult::Continue {
                    auth_succeeded: false,
                })
            }

            CommandRoutingDecision::SwitchToStateful => {
                debug!(
                    "Client {} decision: SwitchToStateful kind={:?}, verb={:?}",
                    self.client_addr,
                    request.kind(),
                    request.verb()
                );
                info!(
                    "Client {} switching to stateful mode (kind={:?}, verb={:?})",
                    self.client_addr,
                    request.kind(),
                    request.verb()
                );
                Ok(CommandResult::SwitchToStateful)
            }

            CommandRoutingDecision::Reject => {
                debug!("Client {} decision: Reject", self.client_addr);
                let action = CommandHandler::classify_request(request);
                let CommandAction::Reject(response) = action else {
                    unreachable!("Reject decision must come from Reject action")
                };
                client_write.write_all(response.as_bytes()).await?;
                request.record_local_response(response.metadata());
                *backend_to_client_bytes = backend_to_client_bytes.add(response.len());
                Ok(CommandResult::Continue {
                    auth_succeeded: false,
                })
            }

            CommandRoutingDecision::InterceptCapabilities => {
                debug!(
                    "Client {} decision: InterceptCapabilities",
                    self.client_addr
                );
                // Build a synthetic capability list that reflects the proxy's own capabilities.
                // Per RFC 4643 §3.2: include AUTHINFO only when auth is required and not yet
                // authenticated; omit it once the client is authenticated.
                let capabilities = if skip_auth_check {
                    // skip_auth_check == true means auth disabled OR already authenticated
                    crate::protocol::CAPABILITIES_WITHOUT_AUTHINFO
                } else {
                    // skip_auth_check == false means auth enabled AND not yet authenticated
                    crate::protocol::CAPABILITIES_WITH_AUTHINFO
                };
                client_write.write_all(capabilities).await?;
                record_local_response(request, codes::CAPABILITY_LIST, capabilities);
                *backend_to_client_bytes = backend_to_client_bytes.add(capabilities.len());
                Ok(CommandResult::Continue {
                    auth_succeeded: false,
                })
            }
        }
    }

    /// Process a single command (handles QUIT, auth, routing decision)
    ///
    /// Returns `SingleCommandResult` indicating whether to continue, quit, or switch to stateful mode.
    async fn process_single_command(
        &self,
        params: ProcessCommandParams<'_, '_>,
    ) -> Result<SingleCommandResult> {
        let ProcessCommandParams {
            request,
            skip_auth_check,
            router,
            client_write,
            auth_username,
            client_to_backend_bytes,
            backend_to_client_bytes,
        } = params;

        // Handle QUIT locally
        if let common::QuitStatus::Quit(bytes) =
            common::handle_quit_command(request, client_write).await?
        {
            record_local_response(
                request,
                codes::CONNECTION_CLOSING,
                crate::protocol::CONNECTION_CLOSING,
            );
            *backend_to_client_bytes = backend_to_client_bytes.add_u64(bytes.into());
            return Ok(SingleCommandResult::Quit);
        }

        // Execute command decision
        match self
            .execute_command_decision(CommandExecutionParams {
                request,
                skip_auth_check,
                router,
                client_write,
                auth_username,
                client_to_backend_bytes,
                backend_to_client_bytes,
            })
            .await?
        {
            CommandResult::Continue { auth_succeeded } => {
                Ok(SingleCommandResult::Continue { auth_succeeded })
            }
            CommandResult::SwitchToStateful => Ok(SingleCommandResult::SwitchToStateful),
        }
    }

    /// Handle a client connection with per-command routing
    /// Each command is routed independently to potentially different backends
    pub async fn handle_per_command_routing(
        &self,
        mut client_stream: TcpStream,
    ) -> Result<TransferMetrics, SessionError> {
        use tokio::io::BufReader;

        let Some(router) = self.router.as_ref() else {
            return Err(SessionError::Backend(anyhow::anyhow!(
                "Per-command routing mode requires a router"
            )));
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
        let mut command_buf = Vec::with_capacity(COMMAND);

        // PERFORMANCE: Cache authenticated state to avoid atomic loads after auth succeeds
        // If auth is disabled, skip checks from the start
        let mut skip_auth_check = !self.auth_handler.is_enabled();

        // Process commands in batches (single commands fall through with zero overhead)
        'command_batch_loop: loop {
            let mut batch = match self
                .read_command_batch(&mut client_reader, &mut command_buf)
                .await
            {
                Ok(batch) => batch,
                Err(e) => {
                    // Handle read errors like the old sequential path: log and break
                    if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                        connection::log_client_error(
                            self.client_addr,
                            self.username(),
                            io_err,
                            TransferMetrics {
                                client_to_backend: client_to_backend_bytes,
                                backend_to_client: backend_to_client_bytes,
                            },
                        );
                    } else {
                        debug!("Client {} read error: {}", self.client_addr, e);
                    }
                    break;
                }
            };

            // RFC 3977 §3.2.1: oversized first command → 501 Syntax Error, keep session alive
            if batch.is_first_oversized() {
                warn!(
                    "Client {} sent oversized first command, rejecting with 501",
                    self.client_addr
                );
                client_write
                    .write_all(crate::protocol::COMMAND_TOO_LONG)
                    .await
                    .map_err(|e| SessionError::from(anyhow::Error::from(e)))?;
                continue;
            }

            if batch.is_empty() {
                debug!("Client {} disconnected", self.client_addr);
                break;
            }

            let batch_size = batch.len();

            // --- Process pipelineable commands ---
            if batch_size > 0 {
                if batch_size > 1 {
                    debug!(
                        "Client {} pipeline batch: {} pipelineable commands",
                        self.client_addr, batch_size
                    );
                }

                // Detect if entire batch is large-transfer commands (ARTICLE/BODY by message-ID)
                // that benefit from TCP pipelining on a single backend connection.
                let all_large_transfer =
                    batch_size > 1 && (0..batch_size).all(|i| batch.context(i).is_large_transfer());

                // Try batch pipelining for all-ARTICLE/BODY batches;
                // fall through to individual processing on failure.
                let batch_handled = if all_large_transfer {
                    match self
                        .batch_execute_articles(
                            router,
                            &batch,
                            &mut client_write,
                            crate::session::handlers::article_retry::BatchPipelineState {
                                client_to_backend_bytes: &mut client_to_backend_bytes,
                                backend_to_client_bytes: &mut backend_to_client_bytes,
                            },
                        )
                        .await
                    {
                        Ok(()) => true,
                        Err(e @ SessionError::ClientDisconnect(_)) => {
                            return Err(e);
                        }
                        Err(e) => {
                            debug!(
                                "Client {} batch execution failed, falling through to individual: {}",
                                self.client_addr, e
                            );
                            false
                        }
                    }
                } else {
                    false
                };

                if !batch_handled {
                    // Sequential processing for mixed, single-command, or failed-batch commands
                    for i in 0..batch_size {
                        let request = batch.context_mut(i);
                        debug!(
                            "Client {} received {} request bytes: kind={:?}, verb={:?}",
                            self.client_addr,
                            request.request_wire_len().get(),
                            request.kind(),
                            request.verb()
                        );

                        client_to_backend_bytes =
                            client_to_backend_bytes.add(request.request_wire_len().get());
                        skip_auth_check = self.is_authenticated_cached(skip_auth_check);

                        match self
                            .process_single_command(ProcessCommandParams {
                                request,
                                skip_auth_check,
                                router,
                                client_write: &mut client_write,
                                auth_username: &mut auth_username,
                                client_to_backend_bytes,
                                backend_to_client_bytes: &mut backend_to_client_bytes,
                            })
                            .await?
                        {
                            SingleCommandResult::Continue { auth_succeeded } => {
                                skip_auth_check |= auth_succeeded;
                            }
                            SingleCommandResult::Quit => {
                                break 'command_batch_loop;
                            }
                            SingleCommandResult::SwitchToStateful => {
                                return self
                                    .switch_to_stateful_mode(
                                        client_reader,
                                        client_write,
                                        request,
                                        client_to_backend_bytes.into(),
                                        backend_to_client_bytes.into(),
                                    )
                                    .await;
                            }
                        }
                    }
                }

                // Record pipeline metrics for batches > 1
                if batch_size > 1 {
                    self.metrics.record_pipeline_batch(batch_size as u64);
                }
            }

            // --- Handle trailing non-pipelineable command (auth, QUIT, stateful, etc.) ---
            if batch.is_trailing_oversized() {
                warn!(
                    "Client {} sent oversized command ({} bytes), rejecting",
                    self.client_addr,
                    batch.trailing_wire_len()
                );
                client_write
                    .write_all(crate::protocol::COMMAND_TOO_LONG)
                    .await
                    .map_err(|e| SessionError::from(anyhow::Error::from(e)))?;
                continue;
            }

            if let Some(trailing_context) = batch.trailing_context() {
                let trailing_wire_len = trailing_context.request_wire_len();
                debug!(
                    "Client {} trailing non-pipelineable {:?}: {:?}",
                    self.client_addr,
                    trailing_context.kind(),
                    String::from_utf8_lossy(trailing_context.verb())
                );

                client_to_backend_bytes = client_to_backend_bytes.add(trailing_wire_len.get());
                skip_auth_check = self.is_authenticated_cached(skip_auth_check);

                let trailing_context = batch
                    .trailing_context_mut()
                    .expect("valid trailing command has context");
                match self
                    .process_single_command(ProcessCommandParams {
                        request: trailing_context,
                        skip_auth_check,
                        router,
                        client_write: &mut client_write,
                        auth_username: &mut auth_username,
                        client_to_backend_bytes,
                        backend_to_client_bytes: &mut backend_to_client_bytes,
                    })
                    .await?
                {
                    SingleCommandResult::Continue { auth_succeeded } => {
                        if auth_succeeded {
                            skip_auth_check = true;
                        }
                    }
                    SingleCommandResult::Quit => {
                        break;
                    }
                    SingleCommandResult::SwitchToStateful => {
                        return self
                            .switch_to_stateful_mode(
                                client_reader,
                                client_write,
                                trailing_context,
                                client_to_backend_bytes.into(),
                                backend_to_client_bytes.into(),
                            )
                            .await;
                    }
                }
            }
        }

        // Log session summary and close user connection
        self.metrics.user_connection_closed(self.username());

        Ok(TransferMetrics {
            client_to_backend: client_to_backend_bytes,
            backend_to_client: backend_to_client_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::protocol::{RequestContext, RequestLine, ResponseWireLen, StatusCode};

    #[test]
    fn local_response_records_typed_status_and_wire_len() {
        let mut request = RequestContext::from_request_line(RequestLine::parse(b"QUIT\r\n"));

        super::record_local_response(
            &mut request,
            crate::protocol::codes::CONNECTION_CLOSING,
            crate::protocol::CONNECTION_CLOSING,
        );

        assert_eq!(request.backend_id(), None);
        assert_eq!(request.response_status(), Some(StatusCode::new(205)));
        assert_eq!(
            request.response_wire_len(),
            Some(ResponseWireLen::new(
                crate::protocol::CONNECTION_CLOSING.len()
            ))
        );
    }

    #[test]
    fn test_client_disconnect_is_detected() {
        use std::io::ErrorKind;

        // Broken pipe should be detected as client disconnect
        let broken_pipe = std::io::Error::new(ErrorKind::BrokenPipe, "broken pipe");
        let err: anyhow::Error = broken_pipe.into();
        assert!(
            matches!(
                crate::session::SessionError::from(err),
                crate::session::SessionError::ClientDisconnect(_)
            ),
            "BrokenPipe should be classified as client disconnect"
        );

        // Timeout is not a client disconnect
        let timeout = std::io::Error::new(ErrorKind::TimedOut, "timed out");
        let err: anyhow::Error = timeout.into();
        assert!(
            matches!(
                crate::session::SessionError::from(err),
                crate::session::SessionError::Backend(_)
            ),
            "TimedOut should NOT be classified as client disconnect"
        );

        // Other errors are not client disconnects
        let other = std::io::Error::other("other error");
        let err: anyhow::Error = other.into();
        assert!(
            matches!(
                crate::session::SessionError::from(err),
                crate::session::SessionError::Backend(_)
            ),
            "Other errors should NOT be classified as client disconnect"
        );
    }
}
