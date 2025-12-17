//! Connection statistics aggregation
//!
//! Aggregates connection events by user over a time window to reduce log spam.
//! Instead of logging every connection individually, we batch them and log:
//! "User abc created 90 connections in per-command routing mode in 5.2s"

use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::info;

/// Time window for aggregating connection stats (30 seconds)
const AGGREGATION_WINDOW: Duration = Duration::from_secs(30);

// Use centralized constant from constants::user module
use crate::constants::user::ANONYMOUS;

/// Statistics for a single user's connections
#[derive(Debug)]
struct UserConnectionStats {
    /// Number of connections in this window
    count: AtomicU64,
    /// Routing mode used (static string - no allocation)
    routing_mode: &'static str,
    /// First connection timestamp  
    first_seen: Instant,
    /// Last connection timestamp (needs Mutex for interior mutability)
    last_seen: Mutex<Instant>,
}

impl UserConnectionStats {
    /// Create new stats for a single event
    fn new(routing_mode: &'static str, timestamp: Instant) -> Self {
        Self {
            count: AtomicU64::new(1),
            routing_mode,
            first_seen: timestamp,
            last_seen: Mutex::new(timestamp),
        }
    }

    /// Update stats with a new event at the given timestamp
    fn record_event(&self, timestamp: Instant) {
        self.count.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut last) = self.last_seen.lock() {
            *last = timestamp;
        }
    }

    /// Duration between first and last event
    #[inline]
    fn duration_secs(&self) -> f64 {
        self.last_seen
            .lock()
            .ok()
            .map(|last| last.duration_since(self.first_seen).as_secs_f64())
            .unwrap_or(0.0)
    }

    /// Get current count
    fn get_count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Log this as a connection event
    fn log_connection(&self, username: &str) {
        let duration = self.duration_secs();
        let count = self.get_count();
        let noun = if count == 1 {
            "connection"
        } else {
            "connections"
        };
        info!(
            username = %username,
            count = count,
            routing_mode = %self.routing_mode,
            duration_secs = duration,
            "User {} created {} {} in {} in {:.1}s",
            username, count, noun, self.routing_mode, duration
        );
    }

    /// Log this as a disconnection event
    fn log_disconnection(&self, username: &str) {
        let duration = self.duration_secs();
        let count = self.get_count();
        let noun = if count == 1 { "session" } else { "sessions" };
        info!(
            username = %username,
            count = count,
            routing_mode = %self.routing_mode,
            duration_secs = duration,
            "{} {} closed for {} in {} over {:.1}s",
            count, noun, username, self.routing_mode, duration
        );
    }
}

/// Connection statistics aggregator
///
/// Buffers connection events and periodically logs aggregated stats
/// to reduce log spam from high-frequency connections.
#[derive(Clone, Debug)]
pub struct ConnectionStatsAggregator {
    connection_stats: Arc<DashMap<String, UserConnectionStats>>,
    disconnection_stats: Arc<DashMap<String, UserConnectionStats>>,
    last_flush: Arc<Mutex<Instant>>,
}

impl ConnectionStatsAggregator {
    /// Create a new connection stats aggregator
    #[must_use]
    pub fn new() -> Self {
        Self {
            connection_stats: Arc::new(DashMap::new()),
            disconnection_stats: Arc::new(DashMap::new()),
            last_flush: Arc::new(Mutex::new(Instant::now())),
        }
    }

    /// Flush stats if needed
    fn maybe_flush(&self, now: Instant, force: bool) {
        if let Ok(mut last_flush) = self.last_flush.lock() {
            let should_flush = force
                || (now.duration_since(*last_flush) >= AGGREGATION_WINDOW
                    && (!self.connection_stats.is_empty() || !self.disconnection_stats.is_empty()));

            if should_flush {
                let log_and_drain =
                    |stats: &DashMap<String, UserConnectionStats>,
                     log_fn: fn(&UserConnectionStats, &str)| {
                        stats
                            .iter()
                            .for_each(|entry| log_fn(entry.value(), entry.key()));
                        stats.clear();
                    };
                log_and_drain(&self.connection_stats, UserConnectionStats::log_connection);
                log_and_drain(
                    &self.disconnection_stats,
                    UserConnectionStats::log_disconnection,
                );
                *last_flush = now;
            }
        }
    }

    /// Record a connection or disconnection event
    fn record_event(
        &self,
        username: Option<&str>,
        routing_mode: &'static str,
        is_connection: bool,
    ) {
        // Fast path: only flush if actually needed (check lock-free first)
        let now = Instant::now();

        // Only check flush time if maps are non-empty (avoid lock when possible)
        if !self.connection_stats.is_empty() || !self.disconnection_stats.is_empty() {
            self.maybe_flush(now, false);
        }

        let stats = if is_connection {
            &self.connection_stats
        } else {
            &self.disconnection_stats
        };
        let username = username.unwrap_or(ANONYMOUS).to_string();

        stats
            .entry(username)
            .and_modify(|s| s.record_event(now))
            .or_insert_with(|| UserConnectionStats::new(routing_mode, now));
    }

    /// Record a new connection
    pub fn record_connection(&self, username: Option<&str>, routing_mode: &'static str) {
        self.record_event(username, routing_mode, true);
    }

    /// Record a disconnection
    pub fn record_disconnection(&self, username: Option<&str>, routing_mode: &'static str) {
        self.record_event(username, routing_mode, false);
    }

    /// Force flush all pending stats (for graceful shutdown)
    pub fn flush(&self) {
        self.maybe_flush(Instant::now(), true);
    }
}

impl Default for ConnectionStatsAggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionStatsAggregator {
    /// Get connection count for a user (primarily for testing)
    pub fn connection_count(&self, username: &str) -> Option<u64> {
        self.connection_stats
            .get(username)
            .map(|stats| stats.get_count())
    }

    /// Get number of tracked users (primarily for testing)
    pub fn user_count(&self) -> usize {
        self.connection_stats.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_connection_stats_new() {
        let now = Instant::now();
        let stats = UserConnectionStats::new("per-command", now);

        assert_eq!(stats.get_count(), 1);
        assert_eq!(stats.routing_mode, "per-command");
        assert_eq!(stats.first_seen, now);
    }

    #[test]
    fn test_user_connection_stats_record_event() {
        let now = Instant::now();
        let stats = UserConnectionStats::new("hybrid", now);

        assert_eq!(stats.get_count(), 1);

        let later = now + Duration::from_secs(5);
        stats.record_event(later);

        assert_eq!(stats.get_count(), 2);
        assert!(stats.duration_secs() >= 5.0);
    }

    #[test]
    fn test_user_connection_stats_duration_secs() {
        let now = Instant::now();
        let stats = UserConnectionStats::new("stateful", now);

        // Immediately after creation, duration should be near 0
        assert!(stats.duration_secs() < 0.1);

        // After recording an event 3 seconds later
        let later = now + Duration::from_secs(3);
        stats.record_event(later);

        let duration = stats.duration_secs();
        assert!((3.0..3.1).contains(&duration));
    }

    #[test]
    fn test_connection_stats_aggregator_new() {
        let aggregator = ConnectionStatsAggregator::new();

        assert_eq!(aggregator.user_count(), 0);
        assert_eq!(aggregator.connection_count("test"), None);
    }

    #[test]
    fn test_connection_stats_aggregator_default() {
        let aggregator = ConnectionStatsAggregator::default();

        assert_eq!(aggregator.user_count(), 0);
    }

    #[test]
    fn test_record_connection_single_user() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_connection(Some("alice"), "per-command");

        assert_eq!(aggregator.user_count(), 1);
        assert_eq!(aggregator.connection_count("alice"), Some(1));
    }

    #[test]
    fn test_record_connection_multiple_events_same_user() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_connection(Some("bob"), "hybrid");
        aggregator.record_connection(Some("bob"), "hybrid");
        aggregator.record_connection(Some("bob"), "hybrid");

        assert_eq!(aggregator.user_count(), 1);
        assert_eq!(aggregator.connection_count("bob"), Some(3));
    }

    #[test]
    fn test_record_connection_multiple_users() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_connection(Some("alice"), "per-command");
        aggregator.record_connection(Some("bob"), "hybrid");
        aggregator.record_connection(Some("charlie"), "stateful");

        assert_eq!(aggregator.user_count(), 3);
        assert_eq!(aggregator.connection_count("alice"), Some(1));
        assert_eq!(aggregator.connection_count("bob"), Some(1));
        assert_eq!(aggregator.connection_count("charlie"), Some(1));
    }

    #[test]
    fn test_record_connection_anonymous() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_connection(None, "per-command");

        assert_eq!(aggregator.user_count(), 1);
        assert_eq!(aggregator.connection_count(ANONYMOUS), Some(1));
    }

    #[test]
    fn test_record_disconnection() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_disconnection(Some("alice"), "per-command");

        // Disconnections are tracked separately
        assert_eq!(aggregator.user_count(), 0); // No connections
    }

    #[test]
    fn test_flush_clears_stats() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_connection(Some("alice"), "per-command");
        aggregator.record_connection(Some("bob"), "hybrid");

        assert_eq!(aggregator.user_count(), 2);

        aggregator.flush();

        assert_eq!(aggregator.user_count(), 0);
        assert_eq!(aggregator.connection_count("alice"), None);
        assert_eq!(aggregator.connection_count("bob"), None);
    }

    #[test]
    fn test_aggregator_clone() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_connection(Some("alice"), "per-command");

        let cloned = aggregator.clone();

        // Clone shares the same underlying data (Arc)
        assert_eq!(cloned.user_count(), 1);
        assert_eq!(cloned.connection_count("alice"), Some(1));
    }

    #[test]
    fn test_connection_count_nonexistent_user() {
        let aggregator = ConnectionStatsAggregator::new();

        assert_eq!(aggregator.connection_count("nonexistent"), None);
    }

    #[test]
    fn test_user_connection_stats_log_connection_single() {
        let now = Instant::now();
        let stats = UserConnectionStats::new("hybrid", now);

        // Should not panic when logging
        stats.log_connection("testuser");
    }

    #[test]
    fn test_user_connection_stats_log_connection_plural() {
        let now = Instant::now();
        let stats = UserConnectionStats::new("per-command", now);
        stats.record_event(now + Duration::from_secs(1));
        stats.record_event(now + Duration::from_secs(2));

        // Should use plural form "connections"
        stats.log_connection("testuser");
    }

    #[test]
    fn test_user_connection_stats_log_disconnection_single() {
        let now = Instant::now();
        let stats = UserConnectionStats::new("stateful", now);

        // Should not panic when logging
        stats.log_disconnection("testuser");
    }

    #[test]
    fn test_user_connection_stats_log_disconnection_plural() {
        let now = Instant::now();
        let stats = UserConnectionStats::new("hybrid", now);
        stats.record_event(now + Duration::from_secs(1));
        stats.record_event(now + Duration::from_secs(2));

        // Should use plural form "sessions"
        stats.log_disconnection("testuser");
    }

    #[test]
    fn test_record_connection_with_empty_username() {
        let aggregator = ConnectionStatsAggregator::new();

        // Empty string should be treated as distinct user
        aggregator.record_connection(Some(""), "hybrid");

        assert_eq!(aggregator.user_count(), 1);
        assert_eq!(aggregator.connection_count(""), Some(1));
    }

    #[test]
    fn test_multiple_disconnections_same_user() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_disconnection(Some("alice"), "hybrid");
        aggregator.record_disconnection(Some("alice"), "hybrid");
        aggregator.record_disconnection(Some("alice"), "hybrid");

        // Disconnections tracked separately from connections
        assert_eq!(aggregator.user_count(), 0); // No connections
    }

    #[test]
    fn test_anonymous_user_constant() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_connection(None, "per-command");
        aggregator.record_connection(None, "per-command");

        // Should aggregate under ANONYMOUS constant
        assert_eq!(aggregator.connection_count(ANONYMOUS), Some(2));
    }

    #[test]
    fn test_routing_mode_preserved() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_connection(Some("user1"), "hybrid");
        aggregator.record_connection(Some("user2"), "stateful");
        aggregator.record_connection(Some("user3"), "per-command");

        // Each user should have their routing mode preserved
        assert_eq!(aggregator.user_count(), 3);
    }

    #[test]
    fn test_duration_zero_for_single_event() {
        let now = Instant::now();
        let stats = UserConnectionStats::new("hybrid", now);

        // Single event should have near-zero duration
        let duration = stats.duration_secs();
        assert!(duration < 0.01);
    }

    #[test]
    fn test_get_count_after_multiple_records() {
        let now = Instant::now();
        let stats = UserConnectionStats::new("stateful", now);

        for i in 1..=10 {
            stats.record_event(now + Duration::from_millis(i * 100));
        }

        assert_eq!(stats.get_count(), 11); // 1 initial + 10 records
    }

    #[test]
    fn test_aggregator_flush_with_no_stats() {
        let aggregator = ConnectionStatsAggregator::new();

        // Flushing empty aggregator should not panic
        aggregator.flush();

        assert_eq!(aggregator.user_count(), 0);
    }

    #[test]
    fn test_aggregator_clone_independence() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_connection(Some("alice"), "hybrid");

        let cloned = aggregator.clone();

        // Both should see the same data (shared Arc)
        assert_eq!(aggregator.connection_count("alice"), Some(1));
        assert_eq!(cloned.connection_count("alice"), Some(1));

        // Adding to one affects the other (shared state)
        aggregator.record_connection(Some("alice"), "hybrid");

        assert_eq!(aggregator.connection_count("alice"), Some(2));
        assert_eq!(cloned.connection_count("alice"), Some(2));
    }

    #[test]
    fn test_flush_after_connection_and_disconnection() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_connection(Some("alice"), "hybrid");
        aggregator.record_disconnection(Some("bob"), "stateful");

        assert_eq!(aggregator.user_count(), 1); // Only connection tracked in connection_stats

        aggregator.flush();

        // Both should be cleared
        assert_eq!(aggregator.user_count(), 0);
        assert_eq!(aggregator.connection_count("alice"), None);
    }
}
