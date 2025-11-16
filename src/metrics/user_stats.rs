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
