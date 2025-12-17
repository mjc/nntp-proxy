//! Session loop state management
//!
//! This module provides the `SessionLoopState` struct which encapsulates
//! all mutable state needed during a session command loop.

use crate::types::{BackendToClientBytes, ClientToBackendBytes, TransferMetrics};

/// Session loop state for tracking bytes, auth, and metrics
///
/// This struct encapsulates all mutable state needed during a session loop,
/// making it easy to pass around and test in isolation.
///
/// # Example
/// ```ignore
/// let state = SessionLoopState::new(auth_enabled)
///     .with_initial_bytes(1000, 500);
/// ```
#[derive(Debug, Clone)]
pub struct SessionLoopState {
    /// Bytes sent from client to backend
    pub client_to_backend: ClientToBackendBytes,
    /// Bytes sent from backend to client
    pub backend_to_client: BackendToClientBytes,
    /// Last reported client-to-backend bytes (for incremental metrics)
    pub last_reported_c2b: ClientToBackendBytes,
    /// Last reported backend-to-client bytes (for incremental metrics)
    pub last_reported_b2c: BackendToClientBytes,
    /// Iteration counter for metrics flush timing
    iteration_count: u32,
    /// Username from AUTHINFO USER command (if any)
    pub auth_username: Option<String>,
    /// Whether to skip auth checking (optimization after first auth)
    pub skip_auth_check: bool,
}

impl Default for SessionLoopState {
    fn default() -> Self {
        Self::new(false)
    }
}

impl SessionLoopState {
    /// Create new session loop state
    ///
    /// # Arguments
    /// * `auth_enabled` - If true, auth checking starts enabled; if false, it's skipped
    #[must_use]
    pub fn new(auth_enabled: bool) -> Self {
        Self {
            client_to_backend: ClientToBackendBytes::zero(),
            backend_to_client: BackendToClientBytes::zero(),
            last_reported_c2b: ClientToBackendBytes::zero(),
            last_reported_b2c: BackendToClientBytes::zero(),
            iteration_count: 0,
            auth_username: None,
            skip_auth_check: !auth_enabled,
        }
    }

    /// Create session loop state with initial byte counts
    ///
    /// Used by hybrid mode when switching from per-command to stateful,
    /// to carry forward the bytes already transferred.
    #[must_use]
    pub fn from_initial_bytes(
        client_to_backend: u64,
        backend_to_client: u64,
        auth_enabled: bool,
    ) -> Self {
        Self::new(auth_enabled).with_initial_bytes(client_to_backend, backend_to_client)
    }

    /// Builder method: set initial byte counts
    #[must_use]
    pub fn with_initial_bytes(mut self, c2b: u64, b2c: u64) -> Self {
        self.client_to_backend = ClientToBackendBytes::new(c2b);
        self.backend_to_client = BackendToClientBytes::new(b2c);
        self.last_reported_c2b = self.client_to_backend;
        self.last_reported_b2c = self.backend_to_client;
        self
    }

    /// Check if metrics should be flushed and reset counter if so
    ///
    /// Returns `true` every `METRICS_FLUSH_INTERVAL` iterations.
    #[inline]
    pub fn check_and_maybe_flush_metrics(&mut self) -> bool {
        self.iteration_count += 1;
        if self.iteration_count >= crate::constants::session::METRICS_FLUSH_INTERVAL {
            self.iteration_count = 0;
            true
        } else {
            false
        }
    }

    /// Add bytes to client-to-backend counter
    #[inline]
    pub fn add_client_to_backend(&mut self, bytes: usize) {
        self.client_to_backend = self.client_to_backend.add(bytes);
    }

    /// Add bytes to backend-to-client counter
    #[inline]
    pub fn add_backend_to_client(&mut self, bytes: u64) {
        self.backend_to_client = self.backend_to_client.add_u64(bytes);
    }

    /// Convert to final transfer metrics
    #[must_use]
    pub fn into_metrics(self) -> TransferMetrics {
        TransferMetrics {
            client_to_backend: self.client_to_backend,
            backend_to_client: self.backend_to_client,
        }
    }

    /// Mark authentication as complete (skip future checks)
    #[inline]
    pub fn mark_authenticated(&mut self) {
        self.skip_auth_check = true;
    }

    /// Update state based on auth handler result
    ///
    /// Returns the bytes written for convenience in chaining.
    pub fn apply_auth_result(&mut self, result: &super::common::AuthHandlerResult) -> u64 {
        let bytes = result.bytes_written();
        self.add_backend_to_client(bytes);
        if result.should_skip_further_checks() {
            self.mark_authenticated();
        }
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::common::AuthHandlerResult;

    #[test]
    fn test_session_loop_state_new() {
        let state = SessionLoopState::new(true);
        assert_eq!(state.client_to_backend.as_u64(), 0);
        assert_eq!(state.backend_to_client.as_u64(), 0);
        assert!(!state.skip_auth_check); // Auth enabled = don't skip
        assert!(state.auth_username.is_none());

        let state2 = SessionLoopState::new(false);
        assert!(state2.skip_auth_check); // Auth disabled = skip
    }

    #[test]
    fn test_session_loop_state_default() {
        let state = SessionLoopState::default();
        assert_eq!(state.client_to_backend.as_u64(), 0);
        assert!(state.skip_auth_check); // Default = auth disabled
    }

    #[test]
    fn test_session_loop_state_builder_pattern() {
        let state = SessionLoopState::new(false).with_initial_bytes(1000, 500);

        assert_eq!(state.client_to_backend.as_u64(), 1000);
        assert_eq!(state.backend_to_client.as_u64(), 500);
    }

    #[test]
    fn test_session_loop_state_from_initial_bytes() {
        let state = SessionLoopState::from_initial_bytes(100, 200, true);
        assert_eq!(state.client_to_backend.as_u64(), 100);
        assert_eq!(state.backend_to_client.as_u64(), 200);
        assert_eq!(state.last_reported_c2b.as_u64(), 100);
        assert_eq!(state.last_reported_b2c.as_u64(), 200);
        assert!(!state.skip_auth_check);
    }

    #[test]
    fn test_session_loop_state_add_bytes() {
        let mut state = SessionLoopState::new(false);

        state.add_client_to_backend(100);
        assert_eq!(state.client_to_backend.as_u64(), 100);

        state.add_backend_to_client(200);
        assert_eq!(state.backend_to_client.as_u64(), 200);

        // Cumulative
        state.add_client_to_backend(50);
        state.add_backend_to_client(50);
        assert_eq!(state.client_to_backend.as_u64(), 150);
        assert_eq!(state.backend_to_client.as_u64(), 250);
    }

    #[test]
    fn test_session_loop_state_mark_authenticated() {
        let mut state = SessionLoopState::new(true);
        assert!(!state.skip_auth_check);

        state.mark_authenticated();
        assert!(state.skip_auth_check);
    }

    #[test]
    fn test_session_loop_state_apply_auth_result() {
        let mut state = SessionLoopState::new(true);
        assert!(!state.skip_auth_check);
        assert_eq!(state.backend_to_client.as_u64(), 0);

        // Authenticated result should update bytes and skip flag
        let result = AuthHandlerResult::Authenticated {
            bytes_written: 100,
            skip_further_checks: true,
        };
        let bytes = state.apply_auth_result(&result);

        assert_eq!(bytes, 100);
        assert_eq!(state.backend_to_client.as_u64(), 100);
        assert!(state.skip_auth_check);
    }

    #[test]
    fn test_session_loop_state_apply_auth_result_not_authenticated() {
        let mut state = SessionLoopState::new(true);

        let result = AuthHandlerResult::NotAuthenticated { bytes_written: 50 };
        state.apply_auth_result(&result);

        assert_eq!(state.backend_to_client.as_u64(), 50);
        assert!(!state.skip_auth_check); // Still need to check
    }

    #[test]
    fn test_session_loop_state_into_metrics() {
        let state = SessionLoopState::new(false).with_initial_bytes(1000, 2000);

        let metrics = state.into_metrics();
        assert_eq!(metrics.client_to_backend.as_u64(), 1000);
        assert_eq!(metrics.backend_to_client.as_u64(), 2000);
    }

    #[test]
    fn test_session_loop_state_metrics_flush_interval() {
        use crate::constants::session::METRICS_FLUSH_INTERVAL;

        let mut state = SessionLoopState::new(false);

        // Should return false until we hit the interval
        for _ in 0..(METRICS_FLUSH_INTERVAL - 1) {
            assert!(!state.check_and_maybe_flush_metrics());
        }

        // Should return true at the interval
        assert!(state.check_and_maybe_flush_metrics());

        // Counter should reset, so next METRICS_FLUSH_INTERVAL-1 should be false
        assert!(!state.check_and_maybe_flush_metrics());
    }
}
