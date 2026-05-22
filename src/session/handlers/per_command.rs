//! Per-command routing mode handler and command dispatch
//!
//! This module implements the command loop for per-command routing where each
//! command can be routed to a different backend. The actual routing, backend
//! execution, and cache logic are split into sub-modules:
//!
//! - [`article_retry`]: Availability-aware backend selection and retry logic
//! - [`command_execution`]: Single-backend command execution and response writing
//! - [`cache_operations`]: Cache lookups, upserts, and tier helpers

use crate::protocol::{
    AUTH_REQUIRED_FOR_COMMAND, RequestContext, RequestKind, RequestResponseMetadata, StatusCode,
    codes,
};
use crate::session::common;
use crate::session::routing::{CommandRoutingDecision, decide_request_routing};
use crate::session::{ClientSession, connection};
use anyhow::Result;
use futures::{StreamExt, stream::FuturesUnordered};
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::command::{CommandAction, CommandHandler};
use crate::constants::buffer::READER_CAPACITY;
use crate::router::BackendSelector;
use crate::session::SessionError;
use crate::session::handlers::article_retry::OrderedPipelineGate;
use crate::types::{BackendId, BackendToClientBytes, ClientToBackendBytes, TransferMetrics};

fn safe_command_log_label(request: &RequestContext) -> String {
    String::from_utf8_lossy(request.verb()).into_owned()
}

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
struct CommandExecutionParams<'a> {
    request: &'a mut RequestContext,
    skip_auth_check: bool,
    router: &'a Arc<BackendSelector>,
    client_writer: &'a crate::session::SharedClientWriter,
    backend_connection: &'a mut Option<(crate::types::BackendId, crate::pool::ConnectionGuard)>,
    auth_username: &'a mut Option<String>,
    client_to_backend_bytes: ClientToBackendBytes,
    backend_to_client_bytes: &'a mut BackendToClientBytes,
}

/// Parameters for processing a single command (full flow including QUIT handling)
struct ProcessCommandParams<'a> {
    request: &'a mut RequestContext,
    skip_auth_check: bool,
    router: &'a Arc<BackendSelector>,
    client_writer: &'a crate::session::SharedClientWriter,
    backend_connection: &'a mut Option<(crate::types::BackendId, crate::pool::ConnectionGuard)>,
    auth_username: &'a mut Option<String>,
    client_to_backend_bytes: ClientToBackendBytes,
    backend_to_client_bytes: &'a mut BackendToClientBytes,
}

#[derive(Default)]
struct BatchBackendConnection {
    conn: Option<(BackendId, crate::pool::ConnectionGuard)>,
}

impl BatchBackendConnection {
    fn slot(&mut self) -> &mut Option<(BackendId, crate::pool::ConnectionGuard)> {
        &mut self.conn
    }

    fn release(&mut self) {
        if let Some((_backend_id, conn)) = self.conn.take() {
            let _ = conn.release();
        }
    }
}

impl Drop for BatchBackendConnection {
    fn drop(&mut self) {
        self.release();
    }
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
        params: CommandExecutionParams<'_>,
    ) -> Result<CommandResult> {
        let request = params.request;
        let skip_auth_check = params.skip_auth_check;
        let CommandExecutionParams {
            router,
            client_writer,
            backend_connection,
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
                    client_writer,
                    auth_username,
                    backend_to_client_bytes,
                )
                .await
            }
            CommandRoutingDecision::Forward => {
                self.handle_forward_decision(
                    request,
                    router,
                    client_writer,
                    backend_connection,
                    client_to_backend_bytes,
                    backend_to_client_bytes,
                )
                .await
            }
            CommandRoutingDecision::RequireAuth => {
                self.handle_require_auth(request, client_writer, backend_to_client_bytes)
                    .await
            }
            CommandRoutingDecision::SwitchToStateful => {
                Ok(self.handle_stateful_switch_decision(request))
            }
            CommandRoutingDecision::Reject => {
                self.handle_rejected_request(request, client_writer, backend_to_client_bytes)
                    .await
            }
            CommandRoutingDecision::InterceptCapabilities => {
                self.handle_capabilities_request(
                    request,
                    skip_auth_check,
                    client_writer,
                    backend_to_client_bytes,
                )
                .await
            }
        }
    }

    async fn handle_intercept_auth(
        &self,
        request: &mut RequestContext,
        client_writer: &crate::session::SharedClientWriter,
        auth_username: &mut Option<String>,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<CommandResult> {
        debug!("Client {} decision: InterceptAuth", self.client_addr);
        let action = CommandHandler::classify_request(request);
        let CommandAction::InterceptAuth(auth_action) = action else {
            unreachable!("InterceptAuth decision must come from InterceptAuth action")
        };

        let mut client_write = client_writer.lock().await;
        let result = common::handle_auth_command(
            &self.auth_handler,
            auth_action,
            &mut *client_write,
            auth_username,
            &self.auth_state,
        )
        .await?;
        let auth_succeeded = matches!(result, common::AuthResult::Authenticated { .. });
        if auth_succeeded {
            common::on_authentication_success(
                self.client_id(),
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
        client_writer: &crate::session::SharedClientWriter,
        backend_connection: &mut Option<(crate::types::BackendId, crate::pool::ConnectionGuard)>,
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
            client_writer,
            backend_connection,
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
        client_writer: &crate::session::SharedClientWriter,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<CommandResult> {
        debug!("Client {} decision: RequireAuth", self.client_addr);
        let mut client_write = client_writer.lock().await;
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
        client_writer: &crate::session::SharedClientWriter,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<CommandResult> {
        debug!("Client {} decision: Reject", self.client_addr);
        let action = CommandHandler::classify_request(request);
        let CommandAction::Reject(response) = action else {
            unreachable!("Reject decision must come from Reject action")
        };
        let mut client_write = client_writer.lock().await;
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
        client_writer: &crate::session::SharedClientWriter,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<CommandResult> {
        debug!(
            "Client {} decision: InterceptCapabilities",
            self.client_addr
        );
        let capabilities = crate::session::backend::capabilities_response(!skip_auth_check);
        let mut client_write = client_writer.lock().await;
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
        params: ProcessCommandParams<'_>,
    ) -> Result<SingleCommandResult> {
        let ProcessCommandParams {
            request,
            skip_auth_check,
            router,
            client_writer,
            backend_connection,
            auth_username,
            client_to_backend_bytes,
            backend_to_client_bytes,
        } = params;

        // Handle QUIT locally
        let quit_status = {
            let mut client_write = client_writer.lock().await;
            common::handle_quit_command(request, &mut *client_write).await?
        };
        if let common::QuitStatus::Quit(bytes) = quit_status {
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
                client_writer,
                backend_connection,
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
        client_stream: TcpStream,
    ) -> Result<TransferMetrics, SessionError> {
        let Some(router) = self.router.as_ref() else {
            return Err(SessionError::Backend(anyhow::anyhow!(
                "Per-command routing mode requires a router"
            )));
        };

        let (client_read, client_write) = client_stream.into_split();
        self.run_per_command_loop(
            router,
            BufReader::with_capacity(READER_CAPACITY, client_read),
            crate::session::SharedClientWriter::new(client_write),
        )
        .await
    }

    async fn run_per_command_loop(
        &self,
        router: &Arc<BackendSelector>,
        mut client_reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
        client_writer: crate::session::SharedClientWriter,
    ) -> Result<TransferMetrics, SessionError> {
        debug!("Client {} entering command loop", self.client_addr);
        let mut command_buf = [0u8; crate::protocol::MAX_COMMAND_LINE_OCTETS];
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
                .handle_command_batch(router, &client_writer, &mut state, &mut batch)
                .await?
            {
                BatchLoopAction::Continue => {}
                BatchLoopAction::Break => break,
                BatchLoopAction::SwitchToStateful(target) => {
                    return self
                        .switch_batch_to_stateful(
                            client_reader,
                            client_writer,
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
        client_reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
        command_buf: &mut [u8; crate::protocol::MAX_COMMAND_LINE_OCTETS],
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
        client_writer: &crate::session::SharedClientWriter,
        state: &mut PerCommandLoopState,
        batch: &mut crate::session::handlers::pipeline::RequestBatch,
    ) -> Result<BatchLoopAction, SessionError> {
        let mut backend_connection = BatchBackendConnection::default();
        if let Some(action) = self.handle_batch_rejections(batch, client_writer).await? {
            return Ok(action);
        }

        match self
            .process_pipelineable_batch(
                router,
                client_writer,
                state,
                batch,
                &mut backend_connection,
            )
            .await?
        {
            BatchLoopAction::Continue => {
                self.handle_trailing_command(
                    router,
                    client_writer,
                    state,
                    batch,
                    &mut backend_connection,
                )
                .await
            }
            action => Ok(action),
        }
    }

    async fn handle_batch_rejections(
        &self,
        batch: &crate::session::handlers::pipeline::RequestBatch,
        client_writer: &crate::session::SharedClientWriter,
    ) -> Result<Option<BatchLoopAction>, SessionError> {
        if batch.is_first_oversized() {
            warn!(
                "Client {} sent oversized first command, rejecting with 501",
                self.client_addr
            );
            let mut client_write = client_writer.lock().await;
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
            let mut client_write = client_writer.lock().await;
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
        client_writer: &crate::session::SharedClientWriter,
        _state: &mut PerCommandLoopState,
        batch: &mut crate::session::handlers::pipeline::RequestBatch,
        backend_connection: &mut BatchBackendConnection,
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

        let action = self
            .process_pipelineable_commands(router, client_writer, _state, batch, backend_connection)
            .await?;
        if !matches!(action, BatchLoopAction::Continue) {
            return Ok(action);
        }
        if batch_size > 1 {
            self.metrics.record_pipeline_batch(batch_size as u64);
        }
        Ok(BatchLoopAction::Continue)
    }

    async fn process_pipelineable_commands(
        &self,
        router: &Arc<BackendSelector>,
        client_writer: &crate::session::SharedClientWriter,
        state: &mut PerCommandLoopState,
        batch: &mut crate::session::handlers::pipeline::RequestBatch,
        backend_connection: &mut BatchBackendConnection,
    ) -> Result<BatchLoopAction, SessionError> {
        let batch_size = batch.len();

        if self.can_process_ordered_large_transfer_batch(router, batch, state) {
            return self
                .process_ordered_large_transfer_batch(router, client_writer, state, batch)
                .await;
        }

        for i in 0..batch_size {
            let request = batch.context(i);
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

            let request = batch.context_mut(i);
            match self
                .process_single_command(ProcessCommandParams {
                    request,
                    skip_auth_check: state.skip_auth_check,
                    router,
                    client_writer,
                    backend_connection: backend_connection.slot(),
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

    fn can_process_ordered_large_transfer_batch(
        &self,
        router: &Arc<BackendSelector>,
        batch: &crate::session::handlers::pipeline::RequestBatch,
        state: &PerCommandLoopState,
    ) -> bool {
        if router.backend_count() <= 1
            || batch.len() <= 1
            || self.cache_articles
            || self.adaptive_precheck
        {
            return false;
        }

        let skip_auth_check = self.is_authenticated_cached(state.skip_auth_check);
        (0..batch.len()).all(|i| {
            let request = batch.context(i);
            request.is_large_transfer()
                && decide_request_routing(
                    request,
                    skip_auth_check,
                    self.auth_handler.is_enabled(),
                    self.mode_state.routing_mode(),
                ) == CommandRoutingDecision::Forward
        })
    }

    async fn process_ordered_large_transfer_batch(
        &self,
        router: &Arc<BackendSelector>,
        client_writer: &crate::session::SharedClientWriter,
        state: &mut PerCommandLoopState,
        batch: &crate::session::handlers::pipeline::RequestBatch,
    ) -> Result<BatchLoopAction, SessionError> {
        let batch_size = batch.len();
        let order = Arc::new(OrderedPipelineGate::new());
        let mut futures = FuturesUnordered::new();

        state.skip_auth_check = self.is_authenticated_cached(state.skip_auth_check);
        for i in 0..batch_size {
            let request = batch.context(i);
            debug!(
                "Client {} dispatching ordered pipeline request {} of {}: kind={:?}, verb={:?}",
                self.client_addr,
                i + 1,
                batch_size,
                request.kind(),
                request.verb()
            );
            state.client_to_backend_bytes = state
                .client_to_backend_bytes
                .add(request.request_wire_len().get());

            futures.push(self.execute_ordered_large_transfer_request(
                router.clone(),
                request,
                client_writer.clone(),
                order.clone(),
                i,
            ));
        }

        let mut first_error = None;
        while let Some(result) = futures.next().await {
            match result {
                Ok(result) => {
                    state.client_to_backend_bytes = state
                        .client_to_backend_bytes
                        .add_u64(result.additional_client_to_backend_bytes.as_u64());
                    state.backend_to_client_bytes = state
                        .backend_to_client_bytes
                        .add_u64(result.backend_to_client_bytes.as_u64());
                }
                Err(err) => {
                    if first_error.is_none() {
                        first_error = Some(err);
                    }
                }
            }
        }

        if let Some(err) = first_error {
            return Err(err);
        }

        Ok(BatchLoopAction::Continue)
    }

    async fn handle_trailing_command(
        &self,
        router: &Arc<BackendSelector>,
        client_writer: &crate::session::SharedClientWriter,
        state: &mut PerCommandLoopState,
        batch: &mut crate::session::handlers::pipeline::RequestBatch,
        backend_connection: &mut BatchBackendConnection,
    ) -> Result<BatchLoopAction, SessionError> {
        if batch.is_trailing_oversized() {
            warn!(
                "Client {} sent oversized command ({} bytes), rejecting",
                self.client_addr,
                batch.trailing_wire_len()
            );
            let mut client_write = client_writer.lock().await;
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
            let mut client_write = client_writer.lock().await;
            client_write
                .write_all(crate::protocol::COMMAND_SYNTAX_ERROR_RESPONSE)
                .await
                .map_err(|e| SessionError::from(anyhow::Error::from(e)))?;
            return Ok(BatchLoopAction::Continue);
        }

        let Some(trailing_context) = batch.trailing_context() else {
            return Ok(BatchLoopAction::Continue);
        };
        if !matches!(trailing_context.kind(), RequestKind::AuthInfo) {
            debug!(
                "Client {} trailing non-pipelineable {}",
                self.client_addr,
                safe_command_log_label(trailing_context)
            );
        }
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
                client_writer,
                backend_connection: backend_connection.slot(),
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
        client_reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
        client_writer: crate::session::SharedClientWriter,
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

        let client_write = client_writer.try_into_inner().map_err(|_| {
            SessionError::Backend(anyhow::anyhow!(
                "client writer still shared while switching to stateful mode"
            ))
        })?;

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
    fn safe_command_log_label_formats_plain_verb() {
        let request = RequestContext::parse(b"GROUP alt.test\r\n").expect("valid request");

        assert_eq!(super::safe_command_log_label(&request), "GROUP");
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
