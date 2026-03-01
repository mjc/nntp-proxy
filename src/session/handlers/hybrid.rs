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
//!                                          handle_stateful_proxy_loop()
//! ```

use crate::session::ClientSession;
use crate::types::TransferMetrics;
use anyhow::{Context, Result};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::tcp::{ReadHalf, WriteHalf};
use tracing::info;

/// Error context for hybrid mode operations
mod error {
    pub const ROUTER_REQUIRED: &str = "Hybrid mode requires a router";
    pub const BACKEND_NOT_FOUND: &str = "Backend not found";
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
    /// Start a stateful session (calls stateful_session_started)
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
    /// * `initial_command` - The stateful command that triggered the switch
    /// * `client_to_backend_bytes` - Bytes already transferred client→backend
    /// * `backend_to_client_bytes` - Bytes already transferred backend→client
    ///
    /// # Errors
    /// Returns error if router unavailable, backend unreachable, or connection fails
    pub(super) async fn switch_to_stateful_mode(
        &self,
        client_reader: BufReader<ReadHalf<'_>>,
        client_write: WriteHalf<'_>,
        initial_command: &str,
        client_to_backend_bytes: u64,
        backend_to_client_bytes: u64,
    ) -> Result<TransferMetrics, crate::session::SessionError> {
        // One-way transition: PerCommand → Stateful
        self.mode_state.switch_to_stateful();

        // Acquire backend connection (returns CommandGuard to track pending_count)
        let (pooled_conn, backend_id, _pending_guard, provider) = self
            .acquire_stateful_backend(initial_command)
            .await
            .context("Failed to acquire backend for stateful mode")?;

        // Wrap connection in guard — removes from pool on any error
        let mut conn_guard = crate::pool::ConnectionGuard::new(pooled_conn, provider);

        // Start stateful session metrics tracking
        let _session_guard = StatefulSessionGuard::start(&self.metrics);

        info!(
            client = %self.client_addr,
            backend = ?backend_id,
            "Switched to stateful mode"
        );

        // Forward the triggering command (response handled by proxy loop)
        conn_guard
            .write_all(initial_command.as_bytes())
            .await
            .context("Failed to send initial command to backend")?;

        // Build initial state with carried-over byte counts
        let initial_bytes = client_to_backend_bytes + initial_command.len() as u64;
        let state = crate::session::state::SessionLoopState::from_initial_bytes(
            initial_bytes,
            backend_to_client_bytes,
            self.auth_handler.is_enabled(),
        );

        // Split backend for bidirectional proxy
        let (backend_read, backend_write) = tokio::io::split(&mut **conn_guard);

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

        // H1: Only return connection to pool on success
        match result {
            Ok(_) => {
                let _conn = conn_guard.release();
            }
            Err(_) => { /* guard drops → removes broken connection from pool */ }
        }

        // Metrics guard automatically ends session via Drop
        result.map_err(crate::session::SessionError::from)
    }

    /// Acquire a dedicated backend connection for stateful mode
    ///
    /// Routes the command to select a backend, then gets a pooled connection.
    /// Returns the connection, backend ID, and a `CommandGuard` that decrements
    /// `pending_count` on drop. Creating the guard here (immediately after
    /// `route_command`) ensures the count is decremented even if
    /// `get_pooled_connection` fails.
    async fn acquire_stateful_backend(
        &self,
        command: &str,
    ) -> Result<(
        deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        crate::types::BackendId,
        crate::router::CommandGuard,
        crate::pool::DeadpoolConnectionProvider,
    )> {
        let router = self
            .router
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!(error::ROUTER_REQUIRED))?;

        let backend_id = router.route_command(self.client_id, command)?;

        // Guard pending_count immediately — if get_pooled_connection fails,
        // the guard drops and decrements automatically
        let pending_guard = crate::router::CommandGuard::new(router.clone(), backend_id);

        let provider = router
            .backend_provider(backend_id)
            .ok_or_else(|| anyhow::anyhow!("{}: {:?}", error::BACKEND_NOT_FOUND, backend_id))?;

        let provider = provider.clone();
        let conn = provider.get_pooled_connection().await?;

        Ok((conn, backend_id, pending_guard, provider))
    }
}

#[cfg(test)]
mod tests {
    // Unit tests for pure functions would go here
    // The async methods require integration tests with mock servers

    #[test]
    fn test_error_messages_are_descriptive() {
        use super::error::*;
        assert!(ROUTER_REQUIRED.contains("router"));
        assert!(BACKEND_NOT_FOUND.contains("Backend"));
    }
}
