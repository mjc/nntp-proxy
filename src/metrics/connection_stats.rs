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
    /// Stats per username for connections
    connection_stats: HashMap<String, UserConnectionStats>,
    /// Stats per username for disconnections
    disconnection_stats: HashMap<String, UserConnectionStats>,
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
                connection_stats: HashMap::new(),
                disconnection_stats: HashMap::new(),
                last_flush: Instant::now(),
            })),
        }
    }

    /// Record a new connection
    pub fn record_connection(&self, username: Option<&str>, routing_mode: &str) {
        let username = username.unwrap_or(ANONYMOUS).to_string();

        if let Ok(mut state) = self.stats.lock() {
            let now = Instant::now();

            // Check if we should flush (30 second window elapsed AND we have stats to flush)
            if now.duration_since(state.last_flush) >= AGGREGATION_WINDOW
                && (!state.connection_stats.is_empty() || !state.disconnection_stats.is_empty())
            {
                Self::flush_stats(&mut state);
            }

            // Update or insert stats for this user
            state
                .connection_stats
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

    /// Record a disconnection
    pub fn record_disconnection(&self, username: Option<&str>, routing_mode: &str) {
        let username = username.unwrap_or(ANONYMOUS).to_string();

        if let Ok(mut state) = self.stats.lock() {
            let now = Instant::now();

            // Check if we should flush (30 second window elapsed AND we have stats to flush)
            if now.duration_since(state.last_flush) >= AGGREGATION_WINDOW
                && (!state.connection_stats.is_empty() || !state.disconnection_stats.is_empty())
            {
                Self::flush_stats(&mut state);
            }

            // Update or insert stats for this user
            state
                .disconnection_stats
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
        let has_connections = !state.connection_stats.is_empty();
        let has_disconnections = !state.disconnection_stats.is_empty();

        if !has_connections && !has_disconnections {
            state.last_flush = Instant::now();
            return;
        }

        // Log aggregated connection stats for each user
        for (username, stats) in &state.connection_stats {
            let duration = stats.last_seen.duration_since(stats.first_seen);

            info!(
                username = %username,
                count = stats.count,
                routing_mode = %stats.routing_mode,
                duration_secs = duration.as_secs_f64(),
                "User created {} connection{} in {} routing mode in {:.1}s",
                stats.count,
                if stats.count == 1 { "" } else { "s" },
                stats.routing_mode,
                duration.as_secs_f64()
            );
        }

        // Log aggregated disconnection stats for each user
        for (username, stats) in &state.disconnection_stats {
            let duration = stats.last_seen.duration_since(stats.first_seen);

            info!(
                username = %username,
                count = stats.count,
                routing_mode = %stats.routing_mode,
                duration_secs = duration.as_secs_f64(),
                "{} session{} closed for user in {} routing mode over {:.1}s",
                stats.count,
                if stats.count == 1 { "" } else { "s" },
                stats.routing_mode,
                duration.as_secs_f64()
            );
        }

        // Clear stats and reset timer
        state.connection_stats.clear();
        state.disconnection_stats.clear();
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
        assert_eq!(state.connection_stats.len(), 1);
        assert_eq!(state.connection_stats.get("testuser").unwrap().count, 1);
    }

    #[test]
    fn test_multiple_connections_same_user() {
        let aggregator = ConnectionStatsAggregator::new();

        for _ in 0..5 {
            aggregator.record_connection(Some("testuser"), "per-command");
        }

        let state = aggregator.stats.lock().unwrap();
        assert_eq!(state.connection_stats.len(), 1);
        assert_eq!(state.connection_stats.get("testuser").unwrap().count, 5);
    }

    #[test]
    fn test_multiple_users() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_connection(Some("user1"), "hybrid");
        aggregator.record_connection(Some("user2"), "hybrid");
        aggregator.record_connection(Some("user1"), "hybrid");

        let state = aggregator.stats.lock().unwrap();
        assert_eq!(state.connection_stats.len(), 2);
        assert_eq!(state.connection_stats.get("user1").unwrap().count, 2);
        assert_eq!(state.connection_stats.get("user2").unwrap().count, 1);
    }

    #[test]
    fn test_anonymous_connections() {
        let aggregator = ConnectionStatsAggregator::new();

        aggregator.record_connection(None, "standard");
        aggregator.record_connection(None, "standard");

        let state = aggregator.stats.lock().unwrap();
        assert_eq!(state.connection_stats.len(), 1);
        assert_eq!(state.connection_stats.get(ANONYMOUS).unwrap().count, 2);
    }
}
