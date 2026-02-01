//! Per-command routing mode handler and command dispatch
//!
//! This module implements the command loop for per-command routing where each
//! command can be routed to a different backend. The actual routing, backend
//! execution, and cache logic are split into sub-modules:
//!
//! - [`article_retry`]: Availability-aware backend selection and retry logic
//! - [`command_execution`]: Single-backend command execution and response streaming
//! - [`cache_operations`]: Cache lookups, upserts, and tier helpers

use crate::session::common;
use crate::session::routing::{CommandRoutingDecision, decide_command_routing};
use crate::session::{ClientSession, connection};
use anyhow::Result;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tracing::{debug, info};

use crate::command::classifier::{ARTICLE_CASES, BODY_CASES, matches_any};
use crate::command::{CommandAction, CommandHandler};
use crate::constants::buffer::{COMMAND, READER_CAPACITY};
use crate::router::BackendSelector;
use crate::types::{BackendToClientBytes, ClientToBackendBytes, TransferMetrics};

/// Result of executing a routing decision
enum CommandResult {
    /// Continue processing commands
    Continue { auth_succeeded: bool },
    /// Switch to stateful mode (early return from loop)
    SwitchToStateful,
}

/// Parameters for executing a command decision
struct CommandExecutionParams<'a, 'b> {
    command: &'a str,
    skip_auth_check: bool,
    router: &'a Arc<BackendSelector>,
    client_reader: &'a mut tokio::io::BufReader<tokio::net::tcp::ReadHalf<'b>>,
    client_write: &'a mut tokio::net::tcp::WriteHalf<'b>,
    auth_username: &'a mut Option<String>,
    client_to_backend_bytes: ClientToBackendBytes,
    backend_to_client_bytes: &'a mut BackendToClientBytes,
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
            command,
            skip_auth_check,
            router,
            client_reader: _client_reader,
            client_write,
            auth_username,
            client_to_backend_bytes,
            backend_to_client_bytes,
        } = params;

        let decision = decide_command_routing(
            command,
            skip_auth_check,
            self.auth_handler.is_enabled(),
            self.mode_state.routing_mode(),
        );

        let trimmed = command.trim();
        match decision {
            CommandRoutingDecision::InterceptAuth => {
                debug!("Client {} decision: InterceptAuth", self.client_addr);
                let action = CommandHandler::classify(command);
                let auth_action = match action {
                    CommandAction::InterceptAuth(a) => a,
                    _ => unreachable!("InterceptAuth decision must come from InterceptAuth action"),
                };

                let auth_succeeded = match common::handle_auth_command(
                    &self.auth_handler,
                    auth_action,
                    client_write,
                    auth_username,
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
                        *backend_to_client_bytes = backend_to_client_bytes.add_u64(bytes.as_u64());
                        true
                    }
                    common::AuthResult::NotAuthenticated(bytes) => {
                        *backend_to_client_bytes = backend_to_client_bytes.add_u64(bytes.as_u64());
                        false
                    }
                };

                Ok(CommandResult::Continue { auth_succeeded })
            }

            CommandRoutingDecision::Forward => {
                debug!(
                    "Client {} decision: Forward ({})",
                    self.client_addr, trimmed
                );
                let mut c2b_mutable = client_to_backend_bytes;
                self.route_and_execute_command(
                    router.clone(),
                    command,
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
                *backend_to_client_bytes =
                    backend_to_client_bytes.add(AUTH_REQUIRED_FOR_COMMAND.len());
                Ok(CommandResult::Continue {
                    auth_succeeded: false,
                })
            }

            CommandRoutingDecision::SwitchToStateful => {
                debug!(
                    "Client {} decision: SwitchToStateful ({})",
                    self.client_addr, trimmed
                );
                info!(
                    "Client {} switching to stateful mode (command: {})",
                    self.client_addr, trimmed
                );
                Ok(CommandResult::SwitchToStateful)
            }

            CommandRoutingDecision::Reject => {
                debug!("Client {} decision: Reject", self.client_addr);
                let action = CommandHandler::classify(command);
                let response = match action {
                    CommandAction::Reject(r) => r,
                    _ => unreachable!("Reject decision must come from Reject action"),
                };
                client_write.write_all(response.as_bytes()).await?;
                *backend_to_client_bytes = backend_to_client_bytes.add(response.len());
                Ok(CommandResult::Continue {
                    auth_succeeded: false,
                })
            }
        }
    }

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
        let mut command_buf = String::with_capacity(COMMAND);

        // PERFORMANCE: Cache authenticated state to avoid atomic loads after auth succeeds
        // If auth is disabled, skip checks from the start
        let mut skip_auth_check = !self.auth_handler.is_enabled();

        // Track last command for switch-to-stateful (needs to be outside loop for borrow checker)
        let mut last_command = String::new();

        // Process commands in batches (single commands fall through with zero overhead)
        loop {
            let batch = match self
                .read_command_batch(&mut client_reader, &mut command_buf)
                .await
            {
                Ok(batch) => batch,
                Err(e) => {
                    // Handle read errors like the old sequential path: log and break
                    if let Some(io_err) = e.downcast_ref::<std::io::Error>() {
                        connection::log_client_error(
                            self.client_addr,
                            self.username().as_deref(),
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

            if batch.is_empty() {
                debug!("Client {} disconnected", self.client_addr);
                break;
            }

            let batch_size = batch.commands.len();

            // --- Process pipelineable commands ---
            if !batch.commands.is_empty() {
                if batch_size > 1 {
                    debug!(
                        "Client {} pipeline batch: {} pipelineable commands",
                        self.client_addr, batch_size
                    );
                }

                // Detect if entire batch is large-transfer commands (ARTICLE/BODY by message-ID)
                // that benefit from TCP pipelining on a single backend connection.
                let all_large_transfer = batch.commands.len() > 1
                    && batch.commands.iter().all(|cmd| {
                        let bytes = cmd.as_bytes();
                        let end = memchr::memchr(b' ', bytes).unwrap_or(bytes.len());
                        end >= 4
                            && (matches_any(&bytes[..end], ARTICLE_CASES)
                                || matches_any(&bytes[..end], BODY_CASES))
                    });

                // Try batch pipelining for all-ARTICLE/BODY batches;
                // fall through to individual processing on failure.
                let mut batch_handled = false;
                if all_large_transfer {
                    match self
                        .batch_execute_articles(
                            router.clone(),
                            &batch.commands,
                            &mut client_write,
                            &mut client_to_backend_bytes,
                            &mut backend_to_client_bytes,
                        )
                        .await
                    {
                        Ok(()) => {
                            batch_handled = true;
                        }
                        Err(e)
                            if crate::session::error_classification::ErrorClassifier::is_client_disconnect(&e) =>
                        {
                            return Err(e);
                        }
                        Err(e) => {
                            debug!(
                                "Client {} batch execution failed, falling through to individual: {}",
                                self.client_addr, e
                            );
                        }
                    }
                }

                if !batch_handled {
                    // Sequential processing for mixed, single-command, or failed-batch commands
                    let mut should_break = false;
                    for command in &batch.commands {
                        debug!(
                            "Client {} received {} bytes: {:?}",
                            self.client_addr,
                            command.len(),
                            command.trim()
                        );

                        client_to_backend_bytes = client_to_backend_bytes.add(command.len());

                        // Handle QUIT locally
                        if let common::QuitStatus::Quit(bytes) =
                            common::handle_quit_command(command, &mut client_write).await?
                        {
                            backend_to_client_bytes = backend_to_client_bytes.add_u64(bytes.into());
                            should_break = true;
                            break;
                        }

                        skip_auth_check = self.is_authenticated_cached(skip_auth_check);

                        last_command.clear();
                        last_command.push_str(command);

                        match self
                            .execute_command_decision(CommandExecutionParams {
                                command: &last_command,
                                skip_auth_check,
                                router,
                                client_reader: &mut client_reader,
                                client_write: &mut client_write,
                                auth_username: &mut auth_username,
                                client_to_backend_bytes,
                                backend_to_client_bytes: &mut backend_to_client_bytes,
                            })
                            .await?
                        {
                            CommandResult::Continue { auth_succeeded } => {
                                if auth_succeeded {
                                    skip_auth_check = true;
                                }
                            }
                            CommandResult::SwitchToStateful => {
                                return self
                                    .switch_to_stateful_mode(
                                        client_reader,
                                        client_write,
                                        &last_command,
                                        client_to_backend_bytes.into(),
                                        backend_to_client_bytes.into(),
                                    )
                                    .await;
                            }
                        }
                    }

                    if should_break {
                        break;
                    }
                }

                // Record pipeline metrics for batches > 1
                if batch_size > 1 {
                    self.metrics.record_pipeline_batch(batch_size as u64);
                }
            }

            // --- Handle trailing non-pipelineable command (auth, QUIT, stateful, etc.) ---
            if let Some(ref trailing_cmd) = batch.trailing_non_pipelineable {
                debug!(
                    "Client {} trailing non-pipelineable: {:?}",
                    self.client_addr,
                    trailing_cmd.trim()
                );

                client_to_backend_bytes = client_to_backend_bytes.add(trailing_cmd.len());

                // Handle QUIT locally
                if let common::QuitStatus::Quit(bytes) =
                    common::handle_quit_command(trailing_cmd, &mut client_write).await?
                {
                    backend_to_client_bytes = backend_to_client_bytes.add_u64(bytes.into());
                    break;
                }

                skip_auth_check = self.is_authenticated_cached(skip_auth_check);

                last_command.clear();
                last_command.push_str(trailing_cmd);

                match self
                    .execute_command_decision(CommandExecutionParams {
                        command: &last_command,
                        skip_auth_check,
                        router,
                        client_reader: &mut client_reader,
                        client_write: &mut client_write,
                        auth_username: &mut auth_username,
                        client_to_backend_bytes,
                        backend_to_client_bytes: &mut backend_to_client_bytes,
                    })
                    .await?
                {
                    CommandResult::Continue { auth_succeeded } => {
                        if auth_succeeded {
                            skip_auth_check = true;
                        }
                    }
                    CommandResult::SwitchToStateful => {
                        return self
                            .switch_to_stateful_mode(
                                client_reader,
                                client_write,
                                &last_command,
                                client_to_backend_bytes.into(),
                                backend_to_client_bytes.into(),
                            )
                            .await;
                    }
                }
            }
        }

        // Log session summary and close user connection
        self.metrics
            .user_connection_closed(self.username().as_deref());

        Ok(TransferMetrics {
            client_to_backend: client_to_backend_bytes,
            backend_to_client: backend_to_client_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_client_disconnect_is_detected() {
        use std::io::ErrorKind;

        // Broken pipe should be detected as client disconnect
        let broken_pipe = std::io::Error::new(ErrorKind::BrokenPipe, "broken pipe");
        let err: anyhow::Error = broken_pipe.into();
        assert!(
            crate::session::error_classification::ErrorClassifier::is_client_disconnect(&err),
            "BrokenPipe should be classified as client disconnect"
        );

        // Timeout is not a client disconnect
        let timeout = std::io::Error::new(ErrorKind::TimedOut, "timed out");
        let err: anyhow::Error = timeout.into();
        assert!(
            !crate::session::error_classification::ErrorClassifier::is_client_disconnect(&err),
            "TimedOut should NOT be classified as client disconnect"
        );

        // Other errors are not client disconnects
        let other = std::io::Error::other("other error");
        let err: anyhow::Error = other.into();
        assert!(
            !crate::session::error_classification::ErrorClassifier::is_client_disconnect(&err),
            "Other errors should NOT be classified as client disconnect"
        );
    }
}
