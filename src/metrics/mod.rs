//! Real-time metrics collection for the NNTP proxy
//!
//! This module provides lock-free, thread-safe metrics tracking using atomic operations.
//! Metrics are designed to be updated frequently from hot paths with minimal overhead.
//!
//! # Architecture
//!
//! - **collector**: Lock-free data accumulation using atomic operations (internal types here)
//! - **connection_stats**: Connection lifecycle tracking with aggregation
//! - **snapshot**: MetricsSnapshot with aggregation/query methods
//! - **backend_stats**: BackendStats with calculation methods
//! - **user_stats**: UserStats with helper methods
//! - **types**: Type-safe newtypes for all metric values (including BackendHealthStatus)
//!
//! ## Rate Calculations
//!
//! Snapshots contain cumulative counters only. The TUI calculates rates by:
//! 1. Getting snapshots at regular intervals (e.g., 250ms)
//! 2. Calculating: rate = (new_value - old_value) / time_delta
//! 3. This keeps the collector simple (just accumulation) and calculator logic in one place (TUI)

mod backend_stats;
mod collector;
mod connection_stats;
mod snapshot;
pub mod store;
pub mod types;
mod user_stats;

pub use backend_stats::BackendStats;
pub use collector::MetricsCollector;
pub use connection_stats::ConnectionStatsAggregator;
pub use snapshot::MetricsSnapshot;
pub use store::{BackendStore, MetricsStore, UserMetrics};
pub use types::*;
pub use user_stats::UserStats;

#[cfg(test)]
mod persistence_tests {
    use super::*;
    use crate::types::{
        ArticleBytesTotal, BackendId, BytesReceived, BytesSent, TimingMeasurementCount,
    };

    /// Test that BackendStats can be serialized and deserialized
    /// Ephemeral fields (active_connections, health_status) should default to zero/Healthy
    #[test]
    fn test_backend_stats_serde_roundtrip() {
        let stats = BackendStats {
            backend_id: BackendId::from_index(2),
            active_connections: ActiveConnections::new(5), // Should be skipped
            total_commands: CommandCount::new(1000),
            bytes_sent: BytesSent::new(50000),
            bytes_received: BytesReceived::new(100_000),
            errors: ErrorCount::new(10),
            errors_4xx: ErrorCount::new(5),
            errors_5xx: ErrorCount::new(5),
            article_bytes_total: ArticleBytesTotal::new(90000),
            article_count: ArticleCount::new(100),
            ttfb_micros_total: TtfbMicros::new(5_000_000),
            ttfb_count: TimingMeasurementCount::new(100),
            send_micros_total: SendMicros::new(1_000_000),
            recv_micros_total: RecvMicros::new(3_000_000),
            connection_failures: FailureCount::new(2),
            health_status: BackendHealthStatus::Degraded, // Should be skipped
        };

        // Serialize to JSON
        let json = serde_json::to_string(&stats).expect("Failed to serialize BackendStats");

        // Verify ephemeral fields are not in JSON
        assert!(!json.contains("active_connections"));
        assert!(!json.contains("health_status"));

        // Deserialize back
        let deserialized: BackendStats =
            serde_json::from_str(&json).expect("Failed to deserialize BackendStats");

        // Check that persisted fields match
        assert_eq!(deserialized.backend_id, stats.backend_id);
        assert_eq!(deserialized.total_commands, stats.total_commands);
        assert_eq!(deserialized.bytes_sent, stats.bytes_sent);
        assert_eq!(deserialized.bytes_received, stats.bytes_received);
        assert_eq!(deserialized.errors, stats.errors);
        assert_eq!(deserialized.errors_4xx, stats.errors_4xx);
        assert_eq!(deserialized.errors_5xx, stats.errors_5xx);
        assert_eq!(deserialized.article_bytes_total, stats.article_bytes_total);
        assert_eq!(deserialized.article_count, stats.article_count);
        assert_eq!(deserialized.ttfb_micros_total, stats.ttfb_micros_total);
        assert_eq!(deserialized.ttfb_count, stats.ttfb_count);
        assert_eq!(deserialized.send_micros_total, stats.send_micros_total);
        assert_eq!(deserialized.recv_micros_total, stats.recv_micros_total);
        assert_eq!(deserialized.connection_failures, stats.connection_failures);

        // Check that ephemeral fields have defaults
        assert_eq!(deserialized.active_connections.get(), 0);
        assert_eq!(deserialized.health_status, BackendHealthStatus::Healthy);
    }

    // MetricsStore tests are in src/metrics/store.rs
    // We import them here to ensure they're run with the metrics test suite
    // The detailed tests are in store.rs for better organization
}
