//! Session loop state management
//!
//! This module provides the `SessionLoopState` struct which encapsulates
//! all mutable state needed during a session command loop.

use std::collections::VecDeque;

use crate::protocol::{RequestKind, StatusCode, request_kind_expects_multiline};
use crate::session::streaming::tail_buffer::{TailBuffer, TerminatorStatus};
use crate::types::{BackendToClientBytes, ClientToBackendBytes, TransferMetrics};

struct PendingBackendResponse {
    kind: RequestKind,
    state: PendingBackendResponseState,
}

pub enum DeferredStatefulAction {
    LocalReply(Vec<u8>),
    BackendRequest { kind: RequestKind, wire: Vec<u8> },
}

enum PendingBackendResponseState {
    AwaitingStatusLine(Vec<u8>),
    ReadingMultiline { tail: TailBuffer },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingChunkProgress {
    consumed: usize,
    completed: bool,
}

impl PendingBackendResponse {
    fn new(kind: RequestKind) -> Self {
        Self {
            kind,
            state: PendingBackendResponseState::AwaitingStatusLine(Vec::new()),
        }
    }

    fn consume(&mut self, chunk: &[u8]) -> PendingChunkProgress {
        match &mut self.state {
            PendingBackendResponseState::AwaitingStatusLine(status_line) => {
                let Some(pos) = memchr::memchr(b'\n', chunk) else {
                    status_line.extend_from_slice(chunk);
                    return PendingChunkProgress {
                        consumed: chunk.len(),
                        completed: false,
                    };
                };

                let end = pos + 1;
                status_line.extend_from_slice(&chunk[..end]);
                let is_multiline = StatusCode::parse(status_line)
                    .is_some_and(|status| request_kind_expects_multiline(self.kind, status));

                if !is_multiline {
                    return PendingChunkProgress {
                        consumed: end,
                        completed: true,
                    };
                }

                let body = &chunk[end..];
                let mut tail = TailBuffer::default();
                tail.update(status_line);
                match tail.detect_terminator(body) {
                    TerminatorStatus::FoundAt(pos) => PendingChunkProgress {
                        consumed: end + pos,
                        completed: true,
                    },
                    TerminatorStatus::NotFound => {
                        tail.update(body);
                        self.state = PendingBackendResponseState::ReadingMultiline { tail };
                        PendingChunkProgress {
                            consumed: chunk.len(),
                            completed: false,
                        }
                    }
                }
            }
            PendingBackendResponseState::ReadingMultiline { tail } => {
                let status = tail.detect_terminator(chunk);
                match status {
                    TerminatorStatus::FoundAt(pos) => PendingChunkProgress {
                        consumed: pos,
                        completed: true,
                    },
                    TerminatorStatus::NotFound => {
                        tail.update(chunk);
                        PendingChunkProgress {
                            consumed: chunk.len(),
                            completed: false,
                        }
                    }
                }
            }
        }
    }
}

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
    /// Forwarded backend replies that must complete before deferred local replies can flush.
    pending_backend_replies: VecDeque<PendingBackendResponse>,
    /// Ordered local/backend actions deferred until earlier backend output has been sent first.
    deferred_actions: VecDeque<DeferredStatefulAction>,
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
    pub const fn new(auth_enabled: bool) -> Self {
        Self {
            client_to_backend: ClientToBackendBytes::zero(),
            backend_to_client: BackendToClientBytes::zero(),
            last_reported_c2b: ClientToBackendBytes::zero(),
            last_reported_b2c: BackendToClientBytes::zero(),
            iteration_count: 0,
            auth_username: None,
            skip_auth_check: !auth_enabled,
            pending_backend_replies: VecDeque::new(),
            deferred_actions: VecDeque::new(),
        }
    }

    /// Create session loop state with initial byte counts
    ///
    /// Used by hybrid mode when switching from per-command to stateful,
    /// to carry forward the bytes already transferred.
    #[must_use]
    pub const fn from_initial_bytes(
        client_to_backend: u64,
        backend_to_client: u64,
        auth_enabled: bool,
    ) -> Self {
        Self::new(auth_enabled).with_initial_bytes(client_to_backend, backend_to_client)
    }

    /// Builder method: set initial byte counts
    #[must_use]
    pub const fn with_initial_bytes(mut self, c2b: u64, b2c: u64) -> Self {
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
    pub const fn check_and_maybe_flush_metrics(&mut self) -> bool {
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
    pub const fn add_client_to_backend(&mut self, bytes: usize) {
        self.client_to_backend = self.client_to_backend.add(bytes);
    }

    /// Add bytes to backend-to-client counter
    #[inline]
    pub const fn add_backend_to_client(&mut self, bytes: u64) {
        self.backend_to_client = self.backend_to_client.add_u64(bytes);
    }

    /// Flush accumulated byte deltas to the metrics collector.
    ///
    /// Reports the difference since the last flush and updates the last-reported watermarks.
    /// Used by both the periodic in-loop flush and the final flush on disconnect.
    pub fn flush_byte_deltas(
        &mut self,
        metrics: &crate::metrics::MetricsCollector,
        backend_id: crate::types::BackendId,
        username: Option<&str>,
    ) {
        let delta_c2b = self
            .client_to_backend
            .as_u64()
            .saturating_sub(self.last_reported_c2b.as_u64());
        let delta_b2c = self
            .backend_to_client
            .as_u64()
            .saturating_sub(self.last_reported_b2c.as_u64());

        if delta_c2b > 0 {
            metrics.record_client_to_backend_bytes_for(backend_id, delta_c2b);
            metrics.user_bytes_sent(username, delta_c2b);
        }
        if delta_b2c > 0 {
            metrics.record_backend_to_client_bytes_for(backend_id, delta_b2c);
            metrics.user_bytes_received(username, delta_b2c);
        }

        self.last_reported_c2b = self.client_to_backend;
        self.last_reported_b2c = self.backend_to_client;
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

    /// Mark that a backend request was forwarded and its reply must be ordered first.
    #[inline]
    pub fn mark_backend_request_sent(&mut self, kind: RequestKind) {
        self.pending_backend_replies
            .push_back(PendingBackendResponse::new(kind));
    }

    /// Whether earlier forwarded backend replies are still ahead of any deferred local replies.
    #[inline]
    #[must_use]
    pub fn has_pending_backend_replies(&self) -> bool {
        !self.pending_backend_replies.is_empty()
    }

    /// Advance pending reply framing using newly forwarded backend bytes.
    pub fn observe_backend_bytes(&mut self, chunk: &[u8]) {
        let mut offset = 0;

        while offset < chunk.len() {
            let Some(front) = self.pending_backend_replies.front_mut() else {
                break;
            };

            let progress = front.consume(&chunk[offset..]);
            if progress.consumed == 0 {
                break;
            }

            offset += progress.consumed;
            if progress.completed {
                self.pending_backend_replies.pop_front();
            } else {
                break;
            }
        }
    }

    /// Queue a local reply until earlier backend output has been sent.
    pub fn push_deferred_reply(&mut self, reply: impl Into<Vec<u8>>) {
        self.deferred_actions
            .push_back(DeferredStatefulAction::LocalReply(reply.into()));
    }

    /// Queue a backend request behind earlier deferred actions.
    pub fn push_deferred_backend_request(&mut self, kind: RequestKind, wire: Vec<u8>) {
        self.deferred_actions
            .push_back(DeferredStatefulAction::BackendRequest { kind, wire });
    }

    /// Whether ordered deferred actions remain queued.
    #[inline]
    #[must_use]
    pub fn has_deferred_actions(&self) -> bool {
        !self.deferred_actions.is_empty()
    }

    /// Pop the next deferred action in FIFO order.
    #[must_use]
    pub fn pop_deferred_action(&mut self) -> Option<DeferredStatefulAction> {
        self.deferred_actions.pop_front()
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
        assert!(!state.has_pending_backend_replies());
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
    fn test_session_loop_state_deferred_replies() {
        let mut state = SessionLoopState::new(false);

        state.mark_backend_request_sent(RequestKind::Date);
        assert!(state.has_pending_backend_replies());

        state.push_deferred_reply(b"205 Goodbye\r\n".to_vec());
        assert!(state.has_deferred_actions());
        match state.pop_deferred_action() {
            Some(DeferredStatefulAction::LocalReply(reply)) => {
                assert_eq!(reply, b"205 Goodbye\r\n".to_vec());
            }
            Some(DeferredStatefulAction::BackendRequest { .. }) | None => {
                panic!("expected deferred local reply")
            }
        }

        state.observe_backend_bytes(b"111 20260505120000\r\n");
        assert!(!state.has_pending_backend_replies());
    }

    #[test]
    fn test_session_loop_state_deferred_backend_requests_preserve_kind_and_wire() {
        let mut state = SessionLoopState::new(false);

        state.push_deferred_backend_request(RequestKind::Date, b"DATE\r\n".to_vec());
        match state.pop_deferred_action() {
            Some(DeferredStatefulAction::BackendRequest { kind, wire }) => {
                assert_eq!(kind, RequestKind::Date);
                assert_eq!(wire, b"DATE\r\n".to_vec());
            }
            Some(DeferredStatefulAction::LocalReply(_)) | None => {
                panic!("expected deferred backend request")
            }
        }
    }

    #[test]
    fn test_pending_backend_replies_wait_for_multiline_terminator() {
        let mut state = SessionLoopState::new(false);

        state.mark_backend_request_sent(RequestKind::Help);
        state.observe_backend_bytes(b"100 Help follows\r\nline one\r\n");
        assert!(state.has_pending_backend_replies());

        state.observe_backend_bytes(b".\r\n");
        assert!(!state.has_pending_backend_replies());
    }

    #[test]
    fn test_pending_backend_replies_handle_empty_multiline_body() {
        let mut state = SessionLoopState::new(false);

        state.mark_backend_request_sent(RequestKind::Help);
        state.observe_backend_bytes(b"100 Help follows\r\n.\r\n");
        assert!(
            !state.has_pending_backend_replies(),
            "empty multiline replies should complete at the immediate terminator"
        );
    }

    #[test]
    fn test_pending_backend_replies_track_pipelined_responses_fifo() {
        let mut state = SessionLoopState::new(false);

        state.mark_backend_request_sent(RequestKind::Date);
        state.mark_backend_request_sent(RequestKind::Help);
        state.observe_backend_bytes(b"111 20260505120000\r\n100 Help follows\r\nline one\r\n");
        assert!(
            state.has_pending_backend_replies(),
            "HELP should remain pending until the multiline terminator arrives"
        );

        state.observe_backend_bytes(b".\r\n");
        assert!(!state.has_pending_backend_replies());
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
