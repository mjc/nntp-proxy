//! Hybrid mode switching handler
//!
//! This module implements the transition from per-command routing to stateful
//! routing when a stateful command is encountered in hybrid mode.
//!
//! # Flow
//!
//! ```text
//! Per-Command Mode ──[stateful command]──> switch_to_stateful_mode()
//!                                                    │
//!                                          ┌─────────▼─────────┐
//!                                          │ 1. Acquire backend │
//!                                          │ 2. Send command    │
//!                                          │ 3. Hand off loop   │
//!                                          └─────────┬─────────┘
//!                                                    │
//!                                          run_stateful_proxy_loop()
//! ```

use crate::session::ClientSession;
use crate::types::TransferMetrics;
use anyhow::{Context, Result};
use tokio::io::BufReader;
use tracing::info;

/// Error context for hybrid mode operations
mod error {
    pub const ROUTER_REQUIRED: &str = "Hybrid mode requires a router";
    pub const BACKEND_NOT_FOUND: &str = "Backend not found";
}

/// The dedicated backend resources held from hybrid handoff through loop exit.
///
/// Keeping the connection, selected backend, and pending-command accounting
/// together prevents a partial handoff from accidentally returning only part
/// of its resources to the pool.
struct StatefulBackendLease {
    connection: crate::pool::ConnectionGuard,
    backend_id: crate::types::BackendId,
    _pending_command: crate::router::CommandGuard,
}

impl StatefulBackendLease {
    const fn new(
        connection: crate::pool::ConnectionGuard,
        backend_id: crate::types::BackendId,
        pending_command: crate::router::CommandGuard,
    ) -> Self {
        Self {
            connection,
            backend_id,
            _pending_command: pending_command,
        }
    }

    fn connection_mut(&mut self) -> &mut crate::pool::ConnectionGuard {
        &mut self.connection
    }

    const fn backend_id(&self) -> crate::types::BackendId {
        self.backend_id
    }

    fn complete_success(self, completion: crate::session::backend::BackendResponseComplete) {
        let _ = self.connection.complete_success(completion);
    }

    fn finalize(
        self,
        disposition: crate::session::handlers::stateful::StatefulConnectionDisposition,
    ) {
        match disposition {
            crate::session::handlers::stateful::StatefulConnectionDisposition::Reusable => {
                self.complete_success(
                    crate::session::backend::BackendResponseComplete::stateful_session(),
                );
            }
            crate::session::handlers::stateful::StatefulConnectionDisposition::RetireClient => {
                self.connection.fail_client();
            }
            crate::session::handlers::stateful::StatefulConnectionDisposition::RetireBackend => {
                self.connection.fail_backend();
            }
        }
    }
}

/// Backend resources and loop state after the triggering request is registered.
///
/// This bundle is the boundary between handoff setup and bidirectional proxying:
/// its existence means the dedicated connection has accepted the request and
/// the response ordering state already expects its reply.
struct PreparedStatefulLoop {
    backend: StatefulBackendLease,
    state: crate::session::state::SessionLoopState,
}

impl PreparedStatefulLoop {
    const fn new(
        backend: StatefulBackendLease,
        state: crate::session::state::SessionLoopState,
    ) -> Self {
        Self { backend, state }
    }

    fn into_parts(
        self,
    ) -> (
        StatefulBackendLease,
        crate::session::state::SessionLoopState,
    ) {
        (self.backend, self.state)
    }
}

/// RAII guard for stateful session metrics
///
/// Automatically calls `stateful_session_ended()` on drop.
/// Follows the same pattern as `CommandGuard` from `src/router/mod.rs`.
struct StatefulSessionGuard<'a> {
    metrics: &'a crate::metrics::MetricsCollector,
    ended: bool,
}

impl<'a> StatefulSessionGuard<'a> {
    /// Start a stateful session (calls `stateful_session_started`)
    fn start(metrics: &'a crate::metrics::MetricsCollector) -> Self {
        metrics.stateful_session_started();
        Self {
            metrics,
            ended: false,
        }
    }
}

impl Drop for StatefulSessionGuard<'_> {
    fn drop(&mut self) {
        if !self.ended {
            self.metrics.stateful_session_ended();
        }
    }
}

fn stateful_initial_client_bytes(
    carried_client_to_backend_bytes: u64,
    initial_request: &crate::command::StatefulHandoff,
) -> u64 {
    carried_client_to_backend_bytes + initial_request.request().request_wire_len().as_u64()
}

impl ClientSession {
    /// Switch from per-command routing to stateful mode
    ///
    /// This is a one-way transition that:
    /// 1. Acquires a dedicated backend connection
    /// 2. Forwards the initial stateful command
    /// 3. Delegates to the stateful proxy loop for the remainder of the session
    ///
    /// # Arguments
    /// * `client_reader` - Buffered reader for client commands
    /// * `client_write` - Write half for sending responses to client
    /// * `initial_request` - The typed stateful request that triggered the switch
    /// * `client_to_backend_bytes` - Bytes already transferred client→backend
    /// * `backend_to_client_bytes` - Bytes already transferred backend→client
    ///
    /// # Errors
    /// Returns error if router unavailable, backend unreachable, or connection fails
    pub(super) async fn switch_to_stateful_mode<R, W>(
        &mut self,
        client_reader: BufReader<R>,
        client_write: W,
        initial_request: crate::command::StatefulHandoff,
        client_to_backend_bytes: u64,
        backend_to_client_bytes: u64,
    ) -> Result<TransferMetrics, crate::session::SessionError>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut backend = self
            .acquire_stateful_backend()
            .await
            .context("Failed to acquire backend for stateful mode")?;

        // Start stateful session metrics tracking
        let _session_guard = StatefulSessionGuard::start(&self.metrics);

        info!(
            client = %self.client_addr,
            backend = ?backend.backend_id(),
            "Switched to stateful mode"
        );

        // Forward the triggering request (response handled by proxy loop)
        if let Err(error) = initial_request
            .request()
            .write_wire_to(backend.connection_mut().stream_mut())
            .await
            .context("Failed to send initial request to backend")
        {
            backend.finalize(
                crate::session::handlers::stateful::StatefulConnectionDisposition::RetireBackend,
            );
            return Err(crate::session::SessionError::from(error));
        }

        // Build initial state with carried-over byte counts
        let initial_bytes =
            stateful_initial_client_bytes(client_to_backend_bytes, &initial_request);
        let mut state = crate::session::state::SessionLoopState::from_initial_bytes(
            initial_bytes,
            backend_to_client_bytes,
            self.auth_handler.is_enabled(),
        );
        state.mark_backend_request_sent(initial_request.request().kind());

        match self.mode_state.switch_to_stateful() {
            crate::session::ModeTransition::Switched => {}
            transition => {
                backend.finalize(
                    crate::session::handlers::stateful::StatefulConnectionDisposition::RetireBackend,
                );
                return Err(crate::session::SessionError::Backend(anyhow::anyhow!(
                    "stateful handoff entered from invalid mode: {transition:?}"
                )));
            }
        }

        let prepared = PreparedStatefulLoop::new(backend, state);

        let (mut backend, state) = prepared.into_parts();
        let backend_id = backend.backend_id();
        let (backend_read, backend_write) = tokio::io::split(backend.connection_mut().stream_mut());

        // Delegate to stateful loop (handles all remaining commands + responses)
        let result = self
            .run_stateful_proxy_loop(
                client_reader,
                client_write,
                backend_read,
                backend_write,
                state,
                backend_id,
            )
            .await;

        // pending_guard automatically calls complete_command via Drop

        // Metrics guard automatically ends session via Drop
        match result {
            Ok(outcome) => {
                let disposition = outcome.disposition();
                let metrics = outcome.into_metrics();
                backend.finalize(disposition);
                Ok(metrics)
            }
            Err(error) => {
                let disposition = error.disposition();
                let source = error.into_source();
                backend.finalize(disposition);
                Err(crate::session::SessionError::from(source))
            }
        }
    }

    /// Acquire a dedicated backend connection for stateful mode
    ///
    /// Routes the client to a backend, then gets a pooled connection.
    /// Returns a lease owning the connection, backend ID, and `CommandGuard`
    /// that decrements `pending_count` on drop. Creating the guard immediately after
    /// routing ensures the count is decremented even if `get_pooled_connection`
    /// fails.
    async fn acquire_stateful_backend(&self) -> Result<StatefulBackendLease> {
        let router = self
            .router
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!(error::ROUTER_REQUIRED))?;

        let backend_id = router.route(crate::router::RouteRequest::new(self.client_id))?;

        // Guard pending_count immediately — if get_pooled_connection fails,
        // the guard drops and decrements automatically
        let pending_guard =
            crate::router::BackendSelector::guard_for_routed_backend(router.clone(), backend_id);

        let provider = router
            .backend_provider(backend_id)
            .ok_or_else(|| anyhow::anyhow!("{}: {:?}", error::BACKEND_NOT_FOUND, backend_id))?;

        let provider = provider.clone();
        let conn_guard = provider.checkout_connection_guard().await?;

        Ok(StatefulBackendLease::new(
            conn_guard,
            backend_id,
            pending_guard,
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::auth::AuthHandler;
    use crate::command::CommandHandler;
    use crate::config::RoutingMode;
    use crate::metrics::MetricsCollector;
    use crate::pool::BufferPool;
    use crate::protocol::RequestContext;
    use crate::router::BackendSelector;
    use crate::session::{ClientSession, SessionMode};
    use crate::types::{BufferSize, ClientAddress};
    use std::sync::Arc;
    use tokio::io::BufReader;

    #[test]
    fn test_error_messages_are_descriptive() {
        use super::error::*;
        assert!(ROUTER_REQUIRED.contains("router"));
        assert!(BACKEND_NOT_FOUND.contains("Backend"));
    }

    #[test]
    fn stateful_initial_client_bytes_uses_typed_wire_len() {
        let request = RequestContext::parse(b"group alt.test\r\n").expect("valid request line");
        let handoff = CommandHandler::prepare_stateful_handoff(
            request,
            crate::command::AuthenticationAccess::Unrestricted,
            RoutingMode::Hybrid,
        )
        .expect("GROUP must prepare a stateful handoff");

        assert_eq!(
            super::stateful_initial_client_bytes(10, &handoff),
            10 + "group alt.test\r\n".len() as u64
        );
    }

    #[tokio::test]
    async fn failed_hybrid_handoff_does_not_commit_stateful_mode() {
        let addr: std::net::SocketAddr = "127.0.0.1:119".parse().expect("valid client address");
        let mut session = ClientSession::new_with_router(
            ClientAddress::from(addr),
            BufferPool::new(BufferSize::try_new(1024).expect("valid buffer size"), 1),
            Arc::new(BackendSelector::new()),
            RoutingMode::Hybrid,
            Arc::new(AuthHandler::new(None, None).expect("valid auth handler")),
            MetricsCollector::new(1),
        );
        let request = RequestContext::parse(b"GROUP alt.test\r\n").expect("valid request");
        let stateful_request = CommandHandler::prepare_stateful_handoff(
            request,
            crate::command::AuthenticationAccess::Unrestricted,
            RoutingMode::Hybrid,
        )
        .expect("GROUP starts a hybrid stateful handoff");
        let (client_write, client_read) = tokio::io::duplex(64);

        let result = session
            .switch_to_stateful_mode(
                BufReader::new(client_read),
                client_write,
                stateful_request,
                0,
                0,
            )
            .await;

        assert!(result.is_err());
        assert_eq!(session.mode(), SessionMode::PerCommand);
    }
}
