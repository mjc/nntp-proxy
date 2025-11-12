//! Connection statistics aggregation for reduced logging noise

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::info;

/// Time window for aggregating connection events (30 seconds)
const AGGREGATION_WINDOW: Duration = Duration::from_secs(30);

/// Username for unauthenticated connections
const ANONYMOUS: &str = "anonymous";

/// Aggregates connection events to reduce log spam
#[derive(Clone)]
pub struct ConnectionStatsAggregator {
    stats: Arc<Mutex<AggregatorState>>,
}

struct AggregatorState {
    /// Per-user connection stats
    user_stats: HashMap<String, UserConnectionStats>,
    /// Last flush time
    last_flush: Instant,
}

struct UserConnectionStats {
    /// Number of connections
    count: usize,
    /// First connection timestamp
    first_seen: Instant,
    /// Last connection timestamp
    last_seen: Instant,
    /// Routing mode for these connections
    routing_mode: String,
}

impl ConnectionStatsAggregator {
    /// Create a new aggregator
    #[must_use]
    pub fn new() -> Self {
        Self {
            stats: Arc::new(Mutex::new(AggregatorState {
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
                .or_insert_with(|| UserConnectionStats {
                    count: 1,
                    first_seen: now,
                    last_seen: now,
                    routing_mode: routing_mode.to_string(),
                });
        }
    }

    /// Manually flush accumulated stats
    pub fn flush(&self) {
        if let Ok(mut state) = self.stats.lock() {
            Self::flush_stats(&mut state);
        }
    }

    /// Internal flush implementation
    fn flush_stats(state: &mut AggregatorState) {
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
                    user = %username,
                    routing_mode = %stats.routing_mode,
                    "Client connected"
                );
            } else {
                // Multiple connections - log aggregated
                info!(
                    user = %username,
                    count = stats.count,
                    duration_secs = duration.as_secs_f64(),
                    routing_mode = %stats.routing_mode,
                    "User created {} connections in {:.1}s",
                    stats.count,
                    duration.as_secs_f64()
                );
            }
        }

        // Clear stats and reset timer
        state.user_stats.clear();
        state.last_flush = Instant::now();
    }
}

impl Default for ConnectionStatsAggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ConnectionStatsAggregator {
    fn drop(&mut self) {
        // Flush any remaining stats on shutdown
        self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

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
