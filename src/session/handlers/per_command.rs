//! Per-command routing mode handler and command dispatch
//!
//! This module implements the command loop for per-command routing where each
//! command can be routed to a different backend. The actual routing, backend
//! execution, and cache logic are split into sub-modules:
//!
//! - [`article_retry`]: Availability-aware backend selection and retry logic
//! - [`command_execution`]: Single-backend command execution and response streaming
//! - [`cache_operations`]: Cache lookups, upserts, and tier helpers

use crate::protocol::{
    AUTH_REQUIRED_FOR_COMMAND, RequestContext, RequestResponseMetadata, StatusCode, codes,
};
use crate::session::common;
use crate::session::routing::{CommandRoutingDecision, decide_request_routing};
use crate::session::{ClientSession, connection};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader};
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

enum BatchLoopAction {
    Continue,
    Break,
    SwitchToStateful(BatchSwitchTarget),
}

enum BatchSwitchTarget {
    Context(usize),
    Trailing,
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

struct PerCommandLoopState {
    client_to_backend_bytes: ClientToBackendBytes,
    backend_to_client_bytes: BackendToClientBytes,
    auth_username: Option<String>,
    skip_auth_check: bool,
}

impl PerCommandLoopState {
    const fn new(skip_auth_check: bool) -> Self {
        Self {
            client_to_backend_bytes: ClientToBackendBytes::zero(),
            backend_to_client_bytes: BackendToClientBytes::zero(),
            auth_username: None,
            skip_auth_check,
        }
    }

    const fn transfer_metrics(&self) -> TransferMetrics {
        TransferMetrics {
            client_to_backend: self.client_to_backend_bytes,
            backend_to_client: self.backend_to_client_bytes,
        }
    }
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
        let request = params.request;
        let skip_auth_check = params.skip_auth_check;
        let CommandExecutionParams {
            router,
            client_write,
            auth_username,
            client_to_backend_bytes,
            backend_to_client_bytes,
            ..
        } = params;

        let decision = decide_request_routing(
            request,
            skip_auth_check,
            self.auth_handler.is_enabled(),
            self.mode_state.routing_mode(),
        );

        match decision {
            CommandRoutingDecision::InterceptAuth => {
                self.handle_intercept_auth(
                    request,
                    client_write,
                    auth_username,
                    backend_to_client_bytes,
                )
                .await
            }
            CommandRoutingDecision::Forward => {
                self.handle_forward_decision(
                    request,
                    router,
                    client_write,
                    client_to_backend_bytes,
                    backend_to_client_bytes,
                )
                .await
            }
            CommandRoutingDecision::RequireAuth => {
                self.handle_require_auth(request, client_write, backend_to_client_bytes)
                    .await
            }
            CommandRoutingDecision::SwitchToStateful => {
                Ok(self.handle_stateful_switch_decision(request))
            }
            CommandRoutingDecision::Reject => {
                self.handle_rejected_request(request, client_write, backend_to_client_bytes)
                    .await
            }
            CommandRoutingDecision::InterceptCapabilities => {
                self.handle_capabilities_request(
                    request,
                    skip_auth_check,
                    client_write,
                    backend_to_client_bytes,
                )
                .await
            }
        }
    }

    async fn handle_intercept_auth(
        &self,
        request: &mut RequestContext,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        auth_username: &mut Option<String>,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<CommandResult> {
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
        let auth_succeeded = matches!(result, common::AuthResult::Authenticated { .. });
        if auth_succeeded {
            common::on_authentication_success(
                self.client_addr,
                auth_username.clone(),
                self.mode_state.routing_mode(),
                &self.metrics,
                self.connection_stats(),
                |username| self.set_username(username),
            );
        }
        request.record_local_response(result.response_metadata());
        *backend_to_client_bytes = backend_to_client_bytes.add_u64(result.bytes_written().as_u64());
        Ok(CommandResult::Continue { auth_succeeded })
    }

    async fn handle_forward_decision(
        &self,
        request: &mut RequestContext,
        router: &Arc<BackendSelector>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        client_to_backend_bytes: ClientToBackendBytes,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<CommandResult> {
        debug!(
            "Client {} decision: Forward kind={:?}, verb={:?}",
            self.client_addr,
            request.kind(),
            request.verb()
        );
        let mut client_to_backend_bytes = client_to_backend_bytes;
        self.route_and_execute_request(
            router.clone(),
            request,
            client_write,
            &mut client_to_backend_bytes,
            backend_to_client_bytes,
        )
        .await?;
        Ok(CommandResult::Continue {
            auth_succeeded: false,
        })
    }

    async fn handle_require_auth(
        &self,
        request: &mut RequestContext,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<CommandResult> {
        debug!("Client {} decision: RequireAuth", self.client_addr);
        client_write.write_all(AUTH_REQUIRED_FOR_COMMAND).await?;
        record_local_response(request, codes::AUTH_REQUIRED, AUTH_REQUIRED_FOR_COMMAND);
        *backend_to_client_bytes = backend_to_client_bytes.add(AUTH_REQUIRED_FOR_COMMAND.len());
        Ok(CommandResult::Continue {
            auth_succeeded: false,
        })
    }

    fn handle_stateful_switch_decision(&self, request: &RequestContext) -> CommandResult {
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
        CommandResult::SwitchToStateful
    }

    async fn handle_rejected_request(
        &self,
        request: &mut RequestContext,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<CommandResult> {
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

    async fn handle_capabilities_request(
        &self,
        request: &mut RequestContext,
        skip_auth_check: bool,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<CommandResult> {
        debug!(
            "Client {} decision: InterceptCapabilities",
            self.client_addr
        );
        let capabilities = if skip_auth_check {
            crate::protocol::CAPABILITIES_WITHOUT_AUTHINFO
        } else {
            crate::protocol::CAPABILITIES_WITH_AUTHINFO
        };
        client_write.write_all(capabilities).await?;
        record_local_response(request, codes::CAPABILITY_LIST, capabilities);
        *backend_to_client_bytes = backend_to_client_bytes.add(capabilities.len());
        Ok(CommandResult::Continue {
            auth_succeeded: false,
        })
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

    /// Handle a client connection with per-command routing.
    ///
    /// Each command is routed independently to potentially different backends.
    ///
    /// # Errors
    /// Returns an error if the router is unavailable, a client write fails, or
    /// switching into stateful mode fails.
    pub async fn handle_per_command_routing(
        &self,
        mut client_stream: TcpStream,
    ) -> Result<TransferMetrics, SessionError> {
        let Some(router) = self.router.as_ref() else {
            return Err(SessionError::Backend(anyhow::anyhow!(
                "Per-command routing mode requires a router"
            )));
        };

        let (client_read, client_write) = client_stream.split();
        Box::pin(self.run_per_command_loop(
            router,
            BufReader::with_capacity(READER_CAPACITY, client_read),
            client_write,
        ))
        .await
    }

    async fn run_per_command_loop(
        &self,
        router: &Arc<BackendSelector>,
        mut client_reader: BufReader<tokio::net::tcp::ReadHalf<'_>>,
        mut client_write: tokio::net::tcp::WriteHalf<'_>,
    ) -> Result<TransferMetrics, SessionError> {
        debug!("Client {} entering command loop", self.client_addr);
        let mut command_buf = Vec::with_capacity(COMMAND);
        let mut state = PerCommandLoopState::new(!self.auth_handler.is_enabled());

        loop {
            let Some(mut batch) = self
                .read_next_batch(
                    &mut client_reader,
                    &mut command_buf,
                    state.transfer_metrics(),
                )
                .await
            else {
                break;
            };

            match self
                .handle_command_batch(router, &mut client_write, &mut state, &mut batch)
                .await?
            {
                BatchLoopAction::Continue => {}
                BatchLoopAction::Break => break,
                BatchLoopAction::SwitchToStateful(target) => {
                    return self
                        .switch_batch_to_stateful(
                            client_reader,
                            client_write,
                            &mut batch,
                            target,
                            &state,
                        )
                        .await;
                }
            }
        }

        self.metrics.user_connection_closed(self.username());
        Ok(state.transfer_metrics())
    }

    async fn read_next_batch(
        &self,
        client_reader: &mut BufReader<tokio::net::tcp::ReadHalf<'_>>,
        command_buf: &mut Vec<u8>,
        metrics: TransferMetrics,
    ) -> Option<crate::session::handlers::pipeline::RequestBatch> {
        match self.read_command_batch(client_reader, command_buf).await {
            Ok(batch) => Some(batch),
            Err(e) => {
                if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                    connection::log_client_error(
                        self.client_addr,
                        self.username(),
                        io_err,
                        metrics,
                    );
                } else {
                    debug!("Client {} read error: {}", self.client_addr, e);
                }
                None
            }
        }
    }

    async fn handle_command_batch(
        &self,
        router: &Arc<BackendSelector>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        state: &mut PerCommandLoopState,
        batch: &mut crate::session::handlers::pipeline::RequestBatch,
    ) -> Result<BatchLoopAction, SessionError> {
        if let Some(action) = self.handle_batch_rejections(batch, client_write).await? {
            return Ok(action);
        }

        match self
            .process_pipelineable_batch(router, client_write, state, batch)
            .await?
        {
            BatchLoopAction::Continue => {
                self.handle_trailing_command(router, client_write, state, batch)
                    .await
            }
            action => Ok(action),
        }
    }

    async fn handle_batch_rejections(
        &self,
        batch: &crate::session::handlers::pipeline::RequestBatch,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
    ) -> Result<Option<BatchLoopAction>, SessionError> {
        if batch.is_first_oversized() {
            warn!(
                "Client {} sent oversized first command, rejecting with 501",
                self.client_addr
            );
            client_write
                .write_all(crate::protocol::COMMAND_TOO_LONG)
                .await
                .map_err(|e| SessionError::from(anyhow::Error::from(e)))?;
            return Ok(Some(BatchLoopAction::Continue));
        }
        if batch.is_first_invalid() {
            warn!(
                "Client {} sent invalid first command, rejecting with 501",
                self.client_addr
            );
            client_write
                .write_all(crate::protocol::COMMAND_SYNTAX_ERROR_RESPONSE)
                .await
                .map_err(|e| SessionError::from(anyhow::Error::from(e)))?;
            return Ok(Some(BatchLoopAction::Continue));
        }
        if batch.is_empty() {
            debug!("Client {} disconnected", self.client_addr);
            return Ok(Some(BatchLoopAction::Break));
        }

        Ok(None)
    }

    async fn process_pipelineable_batch(
        &self,
        router: &Arc<BackendSelector>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        state: &mut PerCommandLoopState,
        batch: &mut crate::session::handlers::pipeline::RequestBatch,
    ) -> Result<BatchLoopAction, SessionError> {
        let batch_size = batch.len();
        if batch_size == 0 {
            return Ok(BatchLoopAction::Continue);
        }
        if batch_size > 1 {
            debug!(
                "Client {} pipeline batch: {} pipelineable commands",
                self.client_addr, batch_size
            );
        }

        let all_large_transfer =
            batch_size > 1 && (0..batch_size).all(|i| batch.context(i).is_large_transfer());
        let batch_handled = self
            .try_execute_article_batch(router, batch, client_write, state, all_large_transfer)
            .await?;
        if !batch_handled {
            let action = self
                .process_pipelineable_commands(router, client_write, state, batch)
                .await?;
            if !matches!(action, BatchLoopAction::Continue) {
                return Ok(action);
            }
        }
        if batch_size > 1 {
            self.metrics.record_pipeline_batch(batch_size as u64);
        }
        Ok(BatchLoopAction::Continue)
    }

    async fn try_execute_article_batch(
        &self,
        router: &Arc<BackendSelector>,
        batch: &crate::session::handlers::pipeline::RequestBatch,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        state: &mut PerCommandLoopState,
        all_large_transfer: bool,
    ) -> Result<bool, SessionError> {
        if !all_large_transfer {
            return Ok(false);
        }

        match self
            .batch_execute_articles(
                router,
                batch,
                client_write,
                crate::session::handlers::article_retry::BatchPipelineState {
                    client_to_backend_bytes: &mut state.client_to_backend_bytes,
                    backend_to_client_bytes: &mut state.backend_to_client_bytes,
                },
            )
            .await
        {
            Ok(()) => Ok(true),
            Err(e @ SessionError::ClientDisconnect(_)) => Err(e),
            Err(e) => {
                debug!(
                    "Client {} batch execution failed, falling through to individual: {}",
                    self.client_addr, e
                );
                Ok(false)
            }
        }
    }

    async fn process_pipelineable_commands(
        &self,
        router: &Arc<BackendSelector>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        state: &mut PerCommandLoopState,
        batch: &mut crate::session::handlers::pipeline::RequestBatch,
    ) -> Result<BatchLoopAction, SessionError> {
        for i in 0..batch.len() {
            let request = batch.context_mut(i);
            debug!(
                "Client {} received {} request bytes: kind={:?}, verb={:?}",
                self.client_addr,
                request.request_wire_len().get(),
                request.kind(),
                request.verb()
            );

            state.client_to_backend_bytes = state
                .client_to_backend_bytes
                .add(request.request_wire_len().get());
            state.skip_auth_check = self.is_authenticated_cached(state.skip_auth_check);

            match self
                .process_single_command(ProcessCommandParams {
                    request,
                    skip_auth_check: state.skip_auth_check,
                    router,
                    client_write,
                    auth_username: &mut state.auth_username,
                    client_to_backend_bytes: state.client_to_backend_bytes,
                    backend_to_client_bytes: &mut state.backend_to_client_bytes,
                })
                .await?
            {
                SingleCommandResult::Continue { auth_succeeded } => {
                    state.skip_auth_check |= auth_succeeded;
                }
                SingleCommandResult::Quit => return Ok(BatchLoopAction::Break),
                SingleCommandResult::SwitchToStateful => {
                    return Ok(BatchLoopAction::SwitchToStateful(
                        BatchSwitchTarget::Context(i),
                    ));
                }
            }
        }

        Ok(BatchLoopAction::Continue)
    }

    async fn handle_trailing_command(
        &self,
        router: &Arc<BackendSelector>,
        client_write: &mut tokio::net::tcp::WriteHalf<'_>,
        state: &mut PerCommandLoopState,
        batch: &mut crate::session::handlers::pipeline::RequestBatch,
    ) -> Result<BatchLoopAction, SessionError> {
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
            return Ok(BatchLoopAction::Continue);
        }
        if batch.is_trailing_invalid() {
            warn!(
                "Client {} sent invalid trailing command, rejecting",
                self.client_addr
            );
            client_write
                .write_all(crate::protocol::COMMAND_SYNTAX_ERROR_RESPONSE)
                .await
                .map_err(|e| SessionError::from(anyhow::Error::from(e)))?;
            return Ok(BatchLoopAction::Continue);
        }

        let Some(trailing_context) = batch.trailing_context() else {
            return Ok(BatchLoopAction::Continue);
        };
        debug!(
            "Client {} trailing non-pipelineable {:?}: {:?}",
            self.client_addr,
            trailing_context.kind(),
            trailing_context.verb()
        );
        state.client_to_backend_bytes = state
            .client_to_backend_bytes
            .add(trailing_context.request_wire_len().get());
        state.skip_auth_check = self.is_authenticated_cached(state.skip_auth_check);

        let Some(trailing_context) = batch.trailing_context_mut() else {
            return Ok(BatchLoopAction::Continue);
        };
        match self
            .process_single_command(ProcessCommandParams {
                request: trailing_context,
                skip_auth_check: state.skip_auth_check,
                router,
                client_write,
                auth_username: &mut state.auth_username,
                client_to_backend_bytes: state.client_to_backend_bytes,
                backend_to_client_bytes: &mut state.backend_to_client_bytes,
            })
            .await?
        {
            SingleCommandResult::Continue { auth_succeeded } => {
                state.skip_auth_check |= auth_succeeded;
                Ok(BatchLoopAction::Continue)
            }
            SingleCommandResult::Quit => Ok(BatchLoopAction::Break),
            SingleCommandResult::SwitchToStateful => Ok(BatchLoopAction::SwitchToStateful(
                BatchSwitchTarget::Trailing,
            )),
        }
    }

    async fn switch_batch_to_stateful(
        &self,
        client_reader: BufReader<tokio::net::tcp::ReadHalf<'_>>,
        client_write: tokio::net::tcp::WriteHalf<'_>,
        batch: &mut crate::session::handlers::pipeline::RequestBatch,
        target: BatchSwitchTarget,
        state: &PerCommandLoopState,
    ) -> Result<TransferMetrics, SessionError> {
        let request = match target {
            BatchSwitchTarget::Context(i) if i < batch.len() => batch.context_mut(i),
            BatchSwitchTarget::Trailing => match batch.trailing_context_mut() {
                Some(request) => request,
                None => return Ok(state.transfer_metrics()),
            },
            BatchSwitchTarget::Context(_) => return Ok(state.transfer_metrics()),
        };

        self.switch_to_stateful_mode(
            client_reader,
            client_write,
            request,
            state.client_to_backend_bytes.into(),
            state.backend_to_client_bytes.into(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use crate::protocol::{RequestContext, ResponseWireLen, StatusCode};

    #[test]
    fn local_response_records_typed_status_and_wire_len() {
        let mut request = RequestContext::parse(b"QUIT\r\n").expect("valid request line");

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
