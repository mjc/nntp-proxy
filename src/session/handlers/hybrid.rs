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

use crate::session::metrics_ext::MetricsRecorder;
use crate::session::{ClientSession, common};
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
    ) -> Result<TransferMetrics> {
        // One-way transition: PerCommand → Stateful
        self.mode_state.switch_to_stateful();

        // Acquire backend connection
        let (mut pooled_conn, backend_id) = self
            .acquire_stateful_backend(initial_command)
            .await
            .context("Failed to acquire backend for stateful mode")?;

        info!(
            client = %self.client_addr,
            backend = ?backend_id,
            "Switched to stateful mode"
        );

        // Track session lifecycle
        self.metrics.stateful_session_started();

        // Forward the triggering command (response handled by proxy loop)
        pooled_conn
            .write_all(initial_command.as_bytes())
            .await
            .context("Failed to send initial command to backend")?;

        // Build initial state with carried-over byte counts
        let initial_bytes = client_to_backend_bytes + initial_command.len() as u64;
        let state = common::SessionLoopState::from_initial_bytes(
            initial_bytes,
            backend_to_client_bytes,
            self.auth_handler.is_enabled(),
        );

        // Split backend for bidirectional proxy
        let (backend_read, backend_write) = tokio::io::split(&mut *pooled_conn);

        // Delegate to stateful loop (handles all remaining commands + responses)
        let metrics = self
            .run_stateful_proxy_loop(
                client_reader,
                client_write,
                backend_read,
                backend_write,
                state,
                backend_id,
            )
            .await?;

        self.metrics.stateful_session_ended();
        Ok(metrics)
    }

    /// Acquire a dedicated backend connection for stateful mode
    ///
    /// Routes the command to select a backend, then gets a pooled connection.
    /// Returns both the connection and the backend ID for metrics tracking.
    async fn acquire_stateful_backend(
        &self,
        command: &str,
    ) -> Result<(
        deadpool::managed::Object<crate::pool::deadpool_connection::TcpManager>,
        crate::types::BackendId,
    )> {
        let router = self
            .router
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!(error::ROUTER_REQUIRED))?;

        let backend_id = router.route_command(self.client_id, command)?;

        let provider = router
            .backend_provider(backend_id)
            .ok_or_else(|| anyhow::anyhow!("{}: {:?}", error::BACKEND_NOT_FOUND, backend_id))?;

        let conn = provider.get_pooled_connection().await?;

        Ok((conn, backend_id))
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
