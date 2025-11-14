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

impl UserConnectionStats {
    /// Create new stats for a single event
    fn new(routing_mode: String, timestamp: Instant) -> Self {
        Self {
            count: 1,
            routing_mode,
            first_seen: timestamp,
            last_seen: timestamp,
        }
    }

    /// Update stats with a new event at the given timestamp
    fn record_event(&mut self, timestamp: Instant) {
        self.count += 1;
        self.last_seen = timestamp;
    }

    /// Duration between first and last event
    fn duration(&self) -> Duration {
        self.last_seen.duration_since(self.first_seen)
    }

    /// Pluralized "connection" or "connections"
    fn connection_noun(&self) -> &'static str {
        if self.count == 1 {
            "connection"
        } else {
            "connections"
        }
    }

    /// Pluralized "session" or "sessions"
    fn session_noun(&self) -> &'static str {
        if self.count == 1 {
            "session"
        } else {
            "sessions"
        }
    }

    /// Log this as a connection event
    fn log_connection(&self, username: &str) {
        info!(
            username = %username,
            count = self.count,
            routing_mode = %self.routing_mode,
            duration_secs = self.duration().as_secs_f64(),
            "User {} created {} {} in {} in {:.1}s",
            username,
            self.count,
            self.connection_noun(),
            self.routing_mode,
            self.duration().as_secs_f64()
        );
    }

    /// Log this as a disconnection event
    fn log_disconnection(&self, username: &str) {
        info!(
            username = %username,
            count = self.count,
            routing_mode = %self.routing_mode,
            duration_secs = self.duration().as_secs_f64(),
            "{} {} closed for {} in {} over {:.1}s",
            self.count,
            self.session_noun(),
            username,
            self.routing_mode,
            self.duration().as_secs_f64()
        );
    }
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

impl AggregationState {
    /// Create new aggregation state
    fn new() -> Self {
        Self {
            connection_stats: HashMap::new(),
            disconnection_stats: HashMap::new(),
            last_flush: Instant::now(),
        }
    }

    /// Check if aggregation window has elapsed and we have stats to flush
    fn should_flush(&self, now: Instant) -> bool {
        let elapsed = now.duration_since(self.last_flush) >= AGGREGATION_WINDOW;
        let has_stats = !self.connection_stats.is_empty() || !self.disconnection_stats.is_empty();
        elapsed && has_stats
    }

    /// Record an event in the specified stats map
    fn record_event(
        stats_map: &mut HashMap<String, UserConnectionStats>,
        username: String,
        routing_mode: String,
        timestamp: Instant,
    ) {
        stats_map
            .entry(username)
            .and_modify(|stats| stats.record_event(timestamp))
            .or_insert_with(|| UserConnectionStats::new(routing_mode, timestamp));
    }

    /// Flush all stats and reset
    fn flush(&mut self) {
        if self.connection_stats.is_empty() && self.disconnection_stats.is_empty() {
            self.last_flush = Instant::now();
            return;
        }

        // Log connections
        self.connection_stats
            .iter()
            .map(|(username, stats)| stats.log_connection(username))
            .last();

        // Log disconnections
        self.disconnection_stats
            .iter()
            .map(|(username, stats)| stats.log_disconnection(username))
            .last();

        self.connection_stats.clear();
        self.disconnection_stats.clear();
        self.last_flush = Instant::now();
    }
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
            stats: Arc::new(Mutex::new(AggregationState::new())),
        }
    }

    /// Record a connection or disconnection event
    fn record_event(&self, username: Option<&str>, routing_mode: &str, is_connection: bool) {
        let username = username.unwrap_or(ANONYMOUS).to_string();
        let routing_mode = routing_mode.to_string();

        if let Ok(mut state) = self.stats.lock() {
            let now = Instant::now();

            // Auto-flush if window elapsed and we have pending stats
            if state.should_flush(now) {
                state.flush();
            }

            // Record the event in the appropriate stats map
            let stats_map = if is_connection {
                &mut state.connection_stats
            } else {
                &mut state.disconnection_stats
            };

            AggregationState::record_event(stats_map, username, routing_mode, now);
        }
    }

    /// Record a new connection
    pub fn record_connection(&self, username: Option<&str>, routing_mode: &str) {
        self.record_event(username, routing_mode, true);
    }

    /// Record a disconnection
    pub fn record_disconnection(&self, username: Option<&str>, routing_mode: &str) {
        self.record_event(username, routing_mode, false);
    }

    /// Force flush all pending stats (for graceful shutdown)
    pub fn flush(&self) {
        if let Ok(mut state) = self.stats.lock() {
            state.flush();
        }
    }
}

impl Default for ConnectionStatsAggregator {
    fn default() -> Self {
        Self::new()
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
