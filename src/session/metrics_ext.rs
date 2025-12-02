//! Extension trait for optional metrics collection
//!
//! Provides a clean API for recording metrics when MetricsCollector is available,
//! with no-op behavior when it's not. This eliminates the need for repeated
//! `if let Some(ref m) = self.metrics` patterns throughout the codebase.

use crate::metrics::MetricsCollector;
use crate::types::BackendId;

/// Extension trait for `Option<MetricsCollector>` to provide no-op metrics recording
///
/// This trait encapsulates the "Option check + forward" pattern, making client code
/// cleaner and more functional. All methods are no-ops when metrics is None.
///
/// # Examples
///
/// ```
/// use nntp_proxy::session::metrics_ext::MetricsRecorder;
/// use nntp_proxy::metrics::MetricsCollector;
/// use nntp_proxy::types::BackendId;
///
/// let metrics: Option<MetricsCollector> = Some(MetricsCollector::new(1));
/// metrics.record_command(BackendId::from_index(0)); // Records
///
/// let no_metrics: Option<MetricsCollector> = None;
/// no_metrics.record_command(BackendId::from_index(0)); // No-op
/// ```
pub trait MetricsRecorder {
    /// Record a command execution for a specific backend
    fn record_command(&self, backend_id: BackendId);

    /// Record a user command (for user-level statistics)
    fn user_command(&self, username: Option<&str>);

    /// Record the start of a stateful session
    fn stateful_session_started(&self);

    /// Record the end of a stateful session
    fn stateful_session_ended(&self);

    /// Record bytes sent by a user
    fn user_bytes_sent(&self, username: Option<&str>, bytes: u64);

    /// Record bytes received by a user
    fn user_bytes_received(&self, username: Option<&str>, bytes: u64);

    /// Record client-to-backend bytes for a specific backend
    fn record_client_to_backend_bytes_for(&self, backend_id: BackendId, bytes: u64);

    /// Record backend-to-client bytes for a specific backend
    fn record_backend_to_client_bytes_for(&self, backend_id: BackendId, bytes: u64);

    /// Record that a user connection has closed
    fn user_connection_closed(&self, username: Option<&str>);
}

impl MetricsRecorder for Option<MetricsCollector> {
    #[inline]
    fn record_command(&self, backend_id: BackendId) {
        if let Some(m) = self {
            m.record_command(backend_id);
        }
    }

    #[inline]
    fn user_command(&self, username: Option<&str>) {
        if let Some(m) = self {
            m.user_command(username);
        }
    }

    #[inline]
    fn stateful_session_started(&self) {
        if let Some(m) = self {
            m.stateful_session_started();
        }
    }

    #[inline]
    fn stateful_session_ended(&self) {
        if let Some(m) = self {
            m.stateful_session_ended();
        }
    }

    #[inline]
    fn user_bytes_sent(&self, username: Option<&str>, bytes: u64) {
        if let Some(m) = self {
            m.user_bytes_sent(username, bytes);
        }
    }

    #[inline]
    fn user_bytes_received(&self, username: Option<&str>, bytes: u64) {
        if let Some(m) = self {
            m.user_bytes_received(username, bytes);
        }
    }

    #[inline]
    fn record_client_to_backend_bytes_for(&self, backend_id: BackendId, bytes: u64) {
        if let Some(m) = self {
            m.record_client_to_backend_bytes_for(backend_id, bytes);
        }
    }

    #[inline]
    fn record_backend_to_client_bytes_for(&self, backend_id: BackendId, bytes: u64) {
        if let Some(m) = self {
            m.record_backend_to_client_bytes_for(backend_id, bytes);
        }
    }

    #[inline]
    fn user_connection_closed(&self, username: Option<&str>) {
        if let Some(m) = self {
            m.user_connection_closed(username);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_metrics_recorder_with_none() {
        let metrics: Option<MetricsCollector> = None;

        // All should be no-ops and not panic
        metrics.record_command(BackendId::from_index(0));
        metrics.user_command(Some("testuser"));
        metrics.stateful_session_started();
        metrics.stateful_session_ended();
        metrics.user_bytes_sent(Some("testuser"), 1024);
        metrics.user_bytes_received(Some("testuser"), 2048);
        metrics.record_client_to_backend_bytes_for(BackendId::from_index(0), 512);
        metrics.record_backend_to_client_bytes_for(BackendId::from_index(0), 1024);
        metrics.user_connection_closed(Some("testuser"));
    }

    #[test]
    fn test_metrics_recorder_with_collector() {
        let collector = MetricsCollector::new(1);
        let metrics: Option<MetricsCollector> = Some(collector.clone());

        metrics.record_command(BackendId::from_index(0));
        metrics.user_command(Some("testuser"));
        metrics.user_bytes_sent(Some("testuser"), 1024);
        metrics.user_bytes_received(Some("testuser"), 2048);

        let snapshot = collector.snapshot(None);
        assert_eq!(snapshot.backend_stats[0].total_commands.get(), 1);

        let user_stats = snapshot
            .user_stats
            .iter()
            .find(|s| s.username == "testuser");
        assert!(user_stats.is_some());

        let stats = user_stats.unwrap();
        assert_eq!(stats.total_commands.get(), 1);
        assert_eq!(stats.bytes_sent.as_u64(), 1024);
        assert_eq!(stats.bytes_received.as_u64(), 2048);
    }
}
