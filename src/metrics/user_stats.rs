//! User statistics type and calculation methods

use crate::metrics::types::{CommandCount, ErrorCount};
use crate::types::{BytesPerSecondRate, BytesReceived, BytesSent, TotalConnections};

/// Statistics for a single user (snapshot)
#[derive(Debug, Clone, Default)]
pub struct UserStats {
    pub username: String,
    pub active_connections: usize,
    pub total_connections: TotalConnections,
    pub bytes_sent: BytesSent,
    pub bytes_received: BytesReceived,
    pub total_commands: CommandCount,
    pub errors: ErrorCount,
    /// Transfer rates (bytes/sec) - populated by TUI from deltas, 0 in raw snapshots
    pub bytes_sent_per_sec: BytesPerSecondRate,
    pub bytes_received_per_sec: BytesPerSecondRate,
}

impl UserStats {
    /// Create new user stats with username
    #[must_use]
    pub fn new(username: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            ..Default::default()
        }
    }

    /// Get total bytes transferred (sent + received)
    #[must_use]
    #[inline]
    pub fn total_bytes(&self) -> u64 {
        self.bytes_sent
            .as_u64()
            .saturating_add(self.bytes_received.as_u64())
    }

    /// Get total transfer rate (sent + received) in bytes/sec
    #[must_use]
    #[inline]
    pub fn total_bytes_per_sec(&self) -> u64 {
        self.bytes_sent_per_sec
            .get()
            .saturating_add(self.bytes_received_per_sec.get())
    }

    /// Calculate error rate as percentage
    #[must_use]
    pub fn error_rate_percent(&self) -> f64 {
        let total_cmds = self.total_commands.get();
        if total_cmds > 0 {
            (self.errors.get() as f64 / total_cmds as f64) * 100.0
        } else {
            0.0
        }
    }

    /// Check if user has any activity
    #[must_use]
    #[inline]
    pub fn has_activity(&self) -> bool {
        self.total_commands.get() > 0 || self.total_connections.get() > 0
    }

    /// Check if user is currently connected
    #[must_use]
    #[inline]
    pub fn is_connected(&self) -> bool {
        self.active_connections > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_stats_new() {
        let stats = UserStats::new("testuser");
        assert_eq!(stats.username, "testuser");
        assert_eq!(stats.active_connections, 0);
        assert_eq!(stats.total_connections.get(), 0);
        assert_eq!(stats.bytes_sent.as_u64(), 0);
        assert_eq!(stats.bytes_received.as_u64(), 0);
        assert_eq!(stats.total_commands.get(), 0);
        assert_eq!(stats.errors.get(), 0);
    }

    #[test]
    fn test_user_stats_new_with_string() {
        let username = String::from("alice");
        let stats = UserStats::new(username);
        assert_eq!(stats.username, "alice");
    }

    #[test]
    fn test_total_bytes() {
        let stats = UserStats {
            bytes_sent: BytesSent::new(1000),
            bytes_received: BytesReceived::new(2000),
            ..Default::default()
        };
        assert_eq!(stats.total_bytes(), 3000);
    }

    #[test]
    fn test_total_bytes_zero() {
        let stats = UserStats::default();
        assert_eq!(stats.total_bytes(), 0);
    }

    #[test]
    fn test_total_bytes_saturating() {
        let stats = UserStats {
            bytes_sent: BytesSent::new(u64::MAX),
            bytes_received: BytesReceived::new(1),
            ..Default::default()
        };
        // Should saturate at u64::MAX, not overflow
        assert_eq!(stats.total_bytes(), u64::MAX);
    }

    #[test]
    fn test_total_bytes_per_sec() {
        let stats = UserStats {
            bytes_sent_per_sec: BytesPerSecondRate::new(100),
            bytes_received_per_sec: BytesPerSecondRate::new(200),
            ..Default::default()
        };
        assert_eq!(stats.total_bytes_per_sec(), 300);
    }

    #[test]
    fn test_total_bytes_per_sec_zero() {
        let stats = UserStats::default();
        assert_eq!(stats.total_bytes_per_sec(), 0);
    }

    #[test]
    fn test_total_bytes_per_sec_saturating() {
        let stats = UserStats {
            bytes_sent_per_sec: BytesPerSecondRate::new(u64::MAX),
            bytes_received_per_sec: BytesPerSecondRate::new(1),
            ..Default::default()
        };
        assert_eq!(stats.total_bytes_per_sec(), u64::MAX);
    }

    #[test]
    fn test_error_rate_percent() {
        let stats = UserStats {
            total_commands: CommandCount::new(100),
            errors: ErrorCount::new(5),
            ..Default::default()
        };
        assert!((stats.error_rate_percent() - 5.0).abs() < 0.01);
    }

    #[test]
    fn test_error_rate_percent_zero_commands() {
        let stats = UserStats {
            total_commands: CommandCount::new(0),
            errors: ErrorCount::new(10),
            ..Default::default()
        };
        assert_eq!(stats.error_rate_percent(), 0.0);
    }

    #[test]
    fn test_error_rate_percent_no_errors() {
        let stats = UserStats {
            total_commands: CommandCount::new(100),
            errors: ErrorCount::new(0),
            ..Default::default()
        };
        assert_eq!(stats.error_rate_percent(), 0.0);
    }

    #[test]
    fn test_error_rate_percent_high() {
        let stats = UserStats {
            total_commands: CommandCount::new(10),
            errors: ErrorCount::new(3),
            ..Default::default()
        };
        assert!((stats.error_rate_percent() - 30.0).abs() < 0.01);
    }

    #[test]
    fn test_has_activity_with_commands() {
        let stats = UserStats {
            total_commands: CommandCount::new(1),
            ..Default::default()
        };
        assert!(stats.has_activity());
    }

    #[test]
    fn test_has_activity_with_connections() {
        let stats = UserStats {
            total_connections: TotalConnections::new(1),
            ..Default::default()
        };
        assert!(stats.has_activity());
    }

    #[test]
    fn test_has_activity_with_both() {
        let stats = UserStats {
            total_commands: CommandCount::new(5),
            total_connections: TotalConnections::new(2),
            ..Default::default()
        };
        assert!(stats.has_activity());
    }

    #[test]
    fn test_has_activity_none() {
        let stats = UserStats::default();
        assert!(!stats.has_activity());
    }

    #[test]
    fn test_is_connected_active() {
        let stats = UserStats {
            active_connections: 1,
            ..Default::default()
        };
        assert!(stats.is_connected());
    }

    #[test]
    fn test_is_connected_multiple() {
        let stats = UserStats {
            active_connections: 5,
            ..Default::default()
        };
        assert!(stats.is_connected());
    }

    #[test]
    fn test_is_connected_none() {
        let stats = UserStats::default();
        assert!(!stats.is_connected());
    }

    #[test]
    fn test_user_stats_default() {
        let stats = UserStats::default();
        assert_eq!(stats.username, "");
        assert_eq!(stats.active_connections, 0);
        assert!(!stats.has_activity());
        assert!(!stats.is_connected());
    }

    #[test]
    fn test_user_stats_clone() {
        let stats = UserStats {
            username: "bob".to_string(),
            active_connections: 2,
            total_connections: TotalConnections::new(10),
            bytes_sent: BytesSent::new(1000),
            bytes_received: BytesReceived::new(2000),
            total_commands: CommandCount::new(50),
            errors: ErrorCount::new(2),
            bytes_sent_per_sec: BytesPerSecondRate::new(100),
            bytes_received_per_sec: BytesPerSecondRate::new(200),
        };

        let cloned = stats.clone();
        assert_eq!(stats.username, cloned.username);
        assert_eq!(stats.active_connections, cloned.active_connections);
        assert_eq!(stats.total_bytes(), cloned.total_bytes());
    }
}
