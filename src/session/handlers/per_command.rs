//! Per-command routing mode handler and command dispatch
//!
//! This module implements the command loop for per-command routing where each
//! command can be routed to a different backend. The actual routing, backend
//! execution, and cache logic are split into sub-modules:
//!
//! - [`article_retry`]: Availability-aware backend selection and retry logic
//! - [`command_execution`]: Single-backend command execution and response writing
//! - [`cache_operations`]: Cache lookups, upserts, and tier helpers

use super::BackendLease;

use crate::protocol::{
    AUTH_REQUIRED_FOR_COMMAND, RequestContext, RequestKind, RequestResponseMetadata, StatusCode,
    codes,
};
use crate::session::common;
use crate::session::{ClientAuthState, ClientSession, connection};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tracing::{debug, warn};

use crate::command::{AuthAction, AuthenticationAccess, CommandHandler, CommandPlan};
use crate::constants::buffer::READER_CAPACITY;
use crate::router::BackendSelector;
use crate::session::SessionError;
use crate::types::{BackendToClientBytes, ClientToBackendBytes, TransferMetrics};

fn safe_command_log_label(request: &RequestContext) -> &str {
    std::str::from_utf8(request.verb()).unwrap_or("<non-utf8-command>")
}

/// Result of executing a routing decision
enum CommandResult {
    /// Continue processing commands
    Continue,
}

/// Result of processing a single command
enum SingleCommandResult {
    /// Continue processing commands
    Continue,
    /// Client sent QUIT command (bytes already added to `backend_to_client_bytes`)
    Quit,
}

/// A classified command plan whose request-borrowing evidence has been reduced
/// to the execution data needed after the request is mutably borrowed.
///
/// Authentication arguments are copied because `CommandPlan` borrows them from
/// the request. The classification itself still happens exactly once.
enum ExecutableCommandPlan {
    InterceptAuth(OwnedAuthAction),
    Reject(crate::command::RejectResponse),
    Forward,
    RequireAuth,
    SwitchToStateful,
    InterceptCapabilities,
}

enum OwnedAuthAction {
    RequestPassword(String),
    ValidateAndRespond { password: String },
    UnknownSubcommand,
}

impl From<CommandPlan<'_>> for ExecutableCommandPlan {
    fn from(plan: CommandPlan<'_>) -> Self {
        match plan {
            CommandPlan::InterceptAuth(AuthAction::RequestPassword(username)) => {
                Self::InterceptAuth(OwnedAuthAction::RequestPassword(username.to_owned()))
            }
            CommandPlan::InterceptAuth(AuthAction::ValidateAndRespond { password }) => {
                Self::InterceptAuth(OwnedAuthAction::ValidateAndRespond {
                    password: password.to_owned(),
                })
            }
            CommandPlan::InterceptAuth(AuthAction::UnknownSubcommand) => {
                Self::InterceptAuth(OwnedAuthAction::UnknownSubcommand)
            }
            CommandPlan::Reject(response) => Self::Reject(response),
            CommandPlan::Forward => Self::Forward,
            CommandPlan::RequireAuth => Self::RequireAuth,
            CommandPlan::SwitchToStateful(_) => Self::SwitchToStateful,
            CommandPlan::InterceptCapabilities => Self::InterceptCapabilities,
        }
    }
}

impl OwnedAuthAction {
    fn as_borrowed(&self) -> AuthAction<'_> {
        match self {
            Self::RequestPassword(username) => AuthAction::RequestPassword(username),
            Self::ValidateAndRespond { password } => AuthAction::ValidateAndRespond { password },
            Self::UnknownSubcommand => AuthAction::UnknownSubcommand,
        }
    }
}

#[expect(
    clippy::large_enum_variant,
    reason = "the owned stateful handoff avoids a heap allocation on this rare transition"
)]
enum BatchLoopAction {
    Continue,
    Break,
    SwitchToStateful(crate::command::StatefulHandoff),
}

/// Shared parameters for command execution and single-command processing
struct CommandExecutionParams<'a> {
    request: &'a mut RequestContext,
    auth_access: AuthenticationAccess,
    router: &'a Arc<BackendSelector>,
    client_writer: &'a crate::session::SharedClientWriter,
    backend_connection: &'a mut Option<BackendLease>,
    auth_username: &'a mut ClientAuthState,
    client_to_backend_bytes: ClientToBackendBytes,
    backend_to_client_bytes: &'a mut BackendToClientBytes,
}

#[derive(Default)]
struct BatchBackendConnection {
    conn: Option<BackendLease>,
}

impl BatchBackendConnection {
    fn slot(&mut self) -> &mut Option<BackendLease> {
        &mut self.conn
    }

    fn complete_success(&mut self) {
        if let Some(lease) = self.conn.take() {
            lease.complete_success();
        }
    }
}

impl Drop for BatchBackendConnection {
    fn drop(&mut self) {
        if let Some(lease) = self.conn.take() {
            lease.fail_backend();
        }
    }
}

struct PerCommandLoopState {
    client_to_backend_bytes: ClientToBackendBytes,
    backend_to_client_bytes: BackendToClientBytes,
    auth_username: ClientAuthState,
    auth_access: AuthenticationAccess,
}

impl PerCommandLoopState {
    const fn new(auth_access: AuthenticationAccess) -> Self {
        Self {
            client_to_backend_bytes: ClientToBackendBytes::zero(),
            backend_to_client_bytes: BackendToClientBytes::zero(),
            auth_username: ClientAuthState::anonymous(),
            auth_access,
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
    async fn handle_intercept_auth(
        &self,
        auth_action: AuthAction<'_>,
        client_writer: &crate::session::SharedClientWriter,
        auth_username: &mut ClientAuthState,
    ) -> Result<common::AuthResult> {
        debug!("Client {} decision: InterceptAuth", self.client_addr);
        let mut client_write = client_writer.lock().await;
        common::handle_auth_command(
            &self.auth_handler,
            auth_action,
            &mut *client_write,
            auth_username,
            &self.auth_state,
        )
        .await
    }

    async fn handle_forward_decision(
        &self,
        request: &mut RequestContext,
        router: &Arc<BackendSelector>,
        client_writer: &crate::session::SharedClientWriter,
        backend_connection: &mut Option<BackendLease>,
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
        Ok(CommandResult::Continue)
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
        Ok(CommandResult::Continue)
    }

    async fn handle_rejected_request(
        &self,
        request: &mut RequestContext,
        response: crate::command::RejectResponse,
        client_writer: &crate::session::SharedClientWriter,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<CommandResult> {
        debug!("Client {} decision: Reject", self.client_addr);
        let mut client_write = client_writer.lock().await;
        client_write.write_all(response.as_bytes()).await?;
        request.record_local_response(response.metadata());
        *backend_to_client_bytes = backend_to_client_bytes.add(response.len());
        Ok(CommandResult::Continue)
    }

    async fn handle_capabilities_request(
        &self,
        request: &mut RequestContext,
        auth_access: AuthenticationAccess,
        client_writer: &crate::session::SharedClientWriter,
        backend_to_client_bytes: &mut BackendToClientBytes,
    ) -> Result<CommandResult> {
        debug!(
            "Client {} decision: InterceptCapabilities",
            self.client_addr
        );
        let capabilities =
            crate::session::backend::capabilities_response(!auth_access.can_access_backend());
        let mut client_write = client_writer.lock().await;
        client_write.write_all(capabilities).await?;
        record_local_response(request, codes::CAPABILITY_LIST, capabilities);
        *backend_to_client_bytes = backend_to_client_bytes.add(capabilities.len());
        Ok(CommandResult::Continue)
    }

    /// Process a single command (handles QUIT, auth, routing decision)
    ///
    /// Returns `SingleCommandResult` indicating whether to continue, quit, or switch to stateful mode.
    async fn process_single_command(
        &self,
        params: CommandExecutionParams<'_>,
    ) -> Result<SingleCommandResult> {
        let CommandExecutionParams {
            request,
            auth_access,
            router,
            client_writer,
            backend_connection,
            auth_username,
            client_to_backend_bytes,
            backend_to_client_bytes,
        } = params;
        let plan = ExecutableCommandPlan::from(CommandHandler::classify_request(
            request,
            auth_access,
            self.mode_state.routing_mode(),
        ));
        self.process_single_command_with_plan(
            CommandExecutionParams {
                request,
                auth_access,
                router,
                client_writer,
                backend_connection,
                auth_username,
                client_to_backend_bytes,
                backend_to_client_bytes,
            },
            plan,
        )
        .await
    }

    async fn process_single_command_with_plan(
        &self,
        params: CommandExecutionParams<'_>,
        plan: ExecutableCommandPlan,
    ) -> Result<SingleCommandResult> {
        let CommandExecutionParams {
            request,
            auth_access,
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

        match plan {
            ExecutableCommandPlan::InterceptAuth(auth_action) => {
                let result = self
                    .handle_intercept_auth(auth_action.as_borrowed(), client_writer, auth_username)
                    .await?;
                if matches!(result, common::AuthResult::Authenticated { .. }) {
                    common::on_authentication_success(
                        self.client_id(),
                        self.client_addr,
                        auth_username.username().map(str::to_owned),
                        self.mode_state.routing_mode(),
                        self.connection_stats(),
                        |username| self.set_username(username),
                    );
                }
                request.record_local_response(result.response_metadata());
                *backend_to_client_bytes =
                    backend_to_client_bytes.add_u64(result.bytes_written().as_u64());
                Ok(SingleCommandResult::Continue)
            }
            ExecutableCommandPlan::Forward => self
                .handle_forward_decision(
                    request,
                    router,
                    client_writer,
                    backend_connection,
                    client_to_backend_bytes,
                    backend_to_client_bytes,
                )
                .await
                .map(|CommandResult::Continue| SingleCommandResult::Continue),
            ExecutableCommandPlan::RequireAuth => self
                .handle_require_auth(request, client_writer, backend_to_client_bytes)
                .await
                .map(|CommandResult::Continue| SingleCommandResult::Continue),
            ExecutableCommandPlan::SwitchToStateful => {
                anyhow::bail!("stateful command reached per-command execution after classification")
            }
            ExecutableCommandPlan::Reject(response) => self
                .handle_rejected_request(request, response, client_writer, backend_to_client_bytes)
                .await
                .map(|CommandResult::Continue| SingleCommandResult::Continue),
            ExecutableCommandPlan::InterceptCapabilities => self
                .handle_capabilities_request(
                    request,
                    auth_access,
                    client_writer,
                    backend_to_client_bytes,
                )
                .await
                .map(|CommandResult::Continue| SingleCommandResult::Continue),
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
        &mut self,
        client_stream: TcpStream,
    ) -> Result<TransferMetrics, SessionError> {
        let Some(router) = self.router.clone() else {
            return Err(SessionError::Backend(anyhow::anyhow!(
                "Per-command routing mode requires a router"
            )));
        };

        let (client_read, client_write) = client_stream.into_split();
        self.run_per_command_loop(
            &router,
            BufReader::with_capacity(READER_CAPACITY, client_read),
            crate::session::SharedClientWriter::new(client_write),
        )
        .await
    }

    async fn run_per_command_loop(
        &mut self,
        router: &Arc<BackendSelector>,
        mut client_reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
        client_writer: crate::session::SharedClientWriter,
    ) -> Result<TransferMetrics, SessionError> {
        debug!("Client {} entering command loop", self.client_addr);
        let mut command_buf = [0u8; crate::protocol::MAX_COMMAND_LINE_OCTETS];
        let mut state = PerCommandLoopState::new(AuthenticationAccess::from_auth_enabled(
            self.auth_handler.is_enabled(),
        ));

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
                BatchLoopAction::SwitchToStateful(initial_request) => {
                    let client_write = client_writer.try_into_inner().map_err(|_| {
                        SessionError::Backend(anyhow::anyhow!(
                            "client writer still shared while switching to stateful mode"
                        ))
                    })?;
                    return self
                        .switch_to_stateful_mode(
                            client_reader,
                            client_write,
                            initial_request,
                            state.client_to_backend_bytes.into(),
                            state.backend_to_client_bytes.into(),
                        )
                        .await;
                }
            }
        }

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

        self.process_pipelineable_batch(
            router,
            client_writer,
            state,
            batch,
            &mut backend_connection,
        )
        .await?;
        let action = self
            .handle_trailing_command(router, client_writer, state, batch, &mut backend_connection)
            .await?;
        backend_connection.complete_success();
        Ok(action)
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
    ) -> Result<(), SessionError> {
        let batch_size = batch.len();
        if batch_size == 0 {
            return Ok(());
        }
        if batch_size > 1 {
            debug!(
                "Client {} pipeline batch: {} pipelineable commands",
                self.client_addr, batch_size
            );
        }

        self.process_pipelineable_commands(
            router,
            client_writer,
            _state,
            batch,
            backend_connection,
        )
        .await?;
        if batch_size > 1 {
            self.metrics.record_pipeline_batch(batch_size as u64);
        }
        Ok(())
    }

    async fn process_pipelineable_commands(
        &self,
        router: &Arc<BackendSelector>,
        client_writer: &crate::session::SharedClientWriter,
        state: &mut PerCommandLoopState,
        batch: &mut crate::session::handlers::pipeline::RequestBatch,
        backend_connection: &mut BatchBackendConnection,
    ) -> Result<(), SessionError> {
        let batch_size = batch.len();
        for i in 0..batch_size {
            let request = batch.context(i).request();
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
            state.auth_access = self.authentication_access(state.auth_access);

            let request = batch.context_mut(i).request_mut();
            match self
                .process_single_command(CommandExecutionParams {
                    request,
                    auth_access: state.auth_access,
                    router,
                    client_writer,
                    backend_connection: backend_connection.slot(),
                    auth_username: &mut state.auth_username,
                    client_to_backend_bytes: state.client_to_backend_bytes,
                    backend_to_client_bytes: &mut state.backend_to_client_bytes,
                })
                .await?
            {
                SingleCommandResult::Continue => {}
                SingleCommandResult::Quit => {
                    return Err(SessionError::Backend(anyhow::anyhow!(
                        "pipelineable request unexpectedly terminated the session"
                    )));
                }
            }
        }

        Ok(())
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

        let Some(trailing_context) = batch.take_trailing_context() else {
            return Ok(BatchLoopAction::Continue);
        };
        if !matches!(trailing_context.kind(), RequestKind::AuthInfo) {
            debug!(
                "Client {} trailing non-pipelineable {}",
                self.client_addr,
                safe_command_log_label(&trailing_context)
            );
        }
        state.client_to_backend_bytes = state
            .client_to_backend_bytes
            .add(trailing_context.request_wire_len().get());
        state.auth_access = self.authentication_access(state.auth_access);

        let trailing_plan = ExecutableCommandPlan::from(CommandHandler::classify_request(
            &trailing_context,
            state.auth_access,
            self.mode_state.routing_mode(),
        ));
        if matches!(trailing_plan, ExecutableCommandPlan::SwitchToStateful) {
            return Ok(BatchLoopAction::SwitchToStateful(
                crate::command::StatefulHandoff::new(trailing_context),
            ));
        }
        let mut trailing_context = trailing_context;
        match self
            .process_single_command_with_plan(
                CommandExecutionParams {
                    request: &mut trailing_context,
                    auth_access: state.auth_access,
                    router,
                    client_writer,
                    backend_connection: backend_connection.slot(),
                    auth_username: &mut state.auth_username,
                    client_to_backend_bytes: state.client_to_backend_bytes,
                    backend_to_client_bytes: &mut state.backend_to_client_bytes,
                },
                trailing_plan,
            )
            .await?
        {
            SingleCommandResult::Continue => Ok(BatchLoopAction::Continue),
            SingleCommandResult::Quit => Ok(BatchLoopAction::Break),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::pool::DeadpoolConnectionProvider;
    use crate::protocol::{RequestContext, ResponseWireLen, StatusCode};
    use crate::types::BackendId;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;
    use tokio::time::Duration;

    async fn spawn_greeting_server() -> (u16, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accept_count = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&accept_count);

        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let (read_half, mut write_half) = stream.into_split();
                    let mut reader = BufReader::new(read_half);

                    if write_half.write_all(b"200 Ready\r\n").await.is_err() {
                        return;
                    }

                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                let cmd = line.trim().to_ascii_uppercase();
                                if cmd == "COMPRESS DEFLATE" {
                                    let _ = write_half.write_all(b"500 Not supported\r\n").await;
                                } else if cmd.starts_with("MODE") {
                                    let _ = write_half.write_all(b"200 Posting allowed\r\n").await;
                                } else if cmd.starts_with("QUIT") {
                                    let _ = write_half.write_all(b"205 Goodbye\r\n").await;
                                    break;
                                } else if cmd.starts_with("DATE") {
                                    let _ = write_half.write_all(b"111 20240101000000\r\n").await;
                                } else {
                                    let _ = write_half.write_all(b"200 OK\r\n").await;
                                }
                            }
                        }
                    }
                });
            }
        });

        (port, accept_count)
    }

    fn make_provider(port: u16) -> DeadpoolConnectionProvider {
        DeadpoolConnectionProvider::builder("127.0.0.1", port)
            .max_connections(5)
            .build()
            .unwrap()
    }

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

    #[tokio::test]
    async fn batch_connection_cancel_retirees_connection_on_drop() {
        let (port, accept_count) = spawn_greeting_server().await;
        let provider = make_provider(port);
        let conn = provider.checkout_connection_guard().await.unwrap();
        assert_eq!(accept_count.load(Ordering::SeqCst), 1);

        let handle = tokio::spawn(async move {
            let _batch = super::BatchBackendConnection {
                conn: Some(super::BackendLease::new(BackendId::from_index(0), conn)),
            };
            tokio::time::sleep(Duration::from_secs(1)).await;
            drop(_batch);
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        handle.abort();
        let _ = handle.await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        let next = provider.checkout_connection_guard().await.unwrap();
        drop(next.complete_success());
        assert_eq!(
            accept_count.load(Ordering::SeqCst),
            2,
            "timeout/cancel cleanup must retire the batch connection instead of returning it to the pool"
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
