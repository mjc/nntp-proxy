//! Connection statistics aggregation
//!
//! Aggregates connection events by user over a time window to reduce log spam.
//! Instead of logging every connection individually, we batch them and log:
//! "User abc created 90 connections in per-command routing mode in 5.2s"

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::info;

/// Time window for aggregating connection stats (30 seconds)
const AGGREGATION_WINDOW: Duration = Duration::from_secs(30);

/// Placeholder for anonymous/unauthenticated connections
const ANONYMOUS: &str = "<anonymous>";

/// Statistics for a single user's connections
#[derive(Debug, Clone)]
struct UserConnectionStats {
    /// Number of connections in this window
    count: u64,
    /// Routing mode used
    routing_mode: String,
    /// First connection timestamp
    first_seen: Instant,
    /// Last connection timestamp
    last_seen: Instant,
}

/// Aggregated connection statistics
#[derive(Debug)]
struct AggregationState {
    /// Stats per username
    user_stats: HashMap<String, UserConnectionStats>,
    /// Last time we flushed stats
    last_flush: Instant,
}

/// Connection statistics aggregator
///
/// Buffers connection events and periodically logs aggregated stats
/// to reduce log spam from high-frequency connections.
#[derive(Clone, Debug)]
pub struct ConnectionStatsAggregator {
    stats: Arc<Mutex<AggregationState>>,
}

impl ConnectionStatsAggregator {
    /// Create a new connection stats aggregator
    #[must_use]
    pub fn new() -> Self {
        Self {
            stats: Arc::new(Mutex::new(AggregationState {
                user_stats: HashMap::new(),
                last_flush: Instant::now(),
            })),
        }
    }

    /// Record a new connection
    pub fn record_connection(&self, username: Option<&str>, routing_mode: &str) {
        let username = username.unwrap_or(ANONYMOUS).to_string();

        if let Ok(mut state) = self.stats.lock() {
            let now = Instant::now();

            // Check if we should flush (30 second window elapsed)
            if now.duration_since(state.last_flush) >= AGGREGATION_WINDOW {
                Self::flush_stats(&mut state);
            }

            // Update or insert stats for this user
            state
                .user_stats
                .entry(username.clone())
                .and_modify(|stats| {
                    stats.count += 1;
                    stats.last_seen = now;
                })
                .or_insert(UserConnectionStats {
                    count: 1,
                    routing_mode: routing_mode.to_string(),
                    first_seen: now,
                    last_seen: now,
                });
        }
    }

    /// Flush accumulated stats and log them
    fn flush_stats(state: &mut AggregationState) {
        if state.user_stats.is_empty() {
            state.last_flush = Instant::now();
            return;
        }

        // Log aggregated stats for each user
        for (username, stats) in &state.user_stats {
            let duration = stats.last_seen.duration_since(stats.first_seen);

            if stats.count == 1 {
                // Single connection - log normally
                info!(
                    username = %username,
                    routing_mode = %stats.routing_mode,
                    "Client connected"
                );
            } else {
                // Multiple connections - log aggregated
                info!(
                    username = %username,
                    count = stats.count,
                    routing_mode = %stats.routing_mode,
                    duration_secs = duration.as_secs_f64(),
                    "User created {} connections in {} routing mode in {:.1}s",
                    stats.count,
                    stats.routing_mode,
                    duration.as_secs_f64()
                );
            }
        }

        // Clear stats and reset timer
        state.user_stats.clear();
        state.last_flush = Instant::now();
    }

    /// Force flush all pending stats (for graceful shutdown)
    pub fn flush(&self) {
        if let Ok(mut state) = self.stats.lock() {
            Self::flush_stats(&mut state);
        }
    }
}

impl Default for ConnectionStatsAggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ConnectionStatsAggregator {
    fn drop(&mut self) {
        // Flush any remaining stats on drop
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_connection() {
        let aggregator = ConnectionStatsAggregator::new();
        aggregator.record_connection(Some("testuser"), "hybrid");

        let state = aggregator.stats.lock().unwrap();
        assert_eq!(state.user_stats.len(), 1);
        assert_eq!(state.user_stats.get("testuser").unwrap().count, 1);
    }

    #[test]
    fn test_multiple_connections_same_user() {
        let aggregator = ConnectionStatsAggregator::new();

        for _ in 0..5 {
            aggregator.record_connection(Some("testuser"), "per-command");
        }

        let state = aggregator.stats.lock().unwrap();
        assert_eq!(state.user_stats.len(), 1);
        assert_eq!(state.user_stats.get("testuser").unwrap().count, 5);
    }

    #[test]
    fn test_multiple_users() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_connection(Some("user1"), "hybrid");
        aggregator.record_connection(Some("user2"), "hybrid");
        aggregator.record_connection(Some("user1"), "hybrid");

        let state = aggregator.stats.lock().unwrap();
        assert_eq!(state.user_stats.len(), 2);
        assert_eq!(state.user_stats.get("user1").unwrap().count, 2);
        assert_eq!(state.user_stats.get("user2").unwrap().count, 1);
    }

    #[test]
    fn test_anonymous_connections() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_connection(None, "standard");
        aggregator.record_connection(None, "standard");

        let state = aggregator.stats.lock().unwrap();
        assert_eq!(state.user_stats.len(), 1);
        assert_eq!(state.user_stats.get(ANONYMOUS).unwrap().count, 2);
    }
}
