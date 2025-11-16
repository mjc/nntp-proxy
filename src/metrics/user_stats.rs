//! User statistics type and calculation methods

/// Statistics for a single user (snapshot)
#[derive(Debug, Clone, Default)]
pub struct UserStats {
    pub username: String,
    pub active_connections: usize,
    pub total_connections: u64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub total_commands: u64,
    pub errors: u64,
    /// Transfer rates (bytes/sec) - populated by TUI from deltas, 0 in raw snapshots
    pub bytes_sent_per_sec: u64,
    pub bytes_received_per_sec: u64,
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
        self.bytes_sent.saturating_add(self.bytes_received)
    }

    /// Get total transfer rate (sent + received) in bytes/sec
    #[must_use]
    #[inline]
    pub fn total_bytes_per_sec(&self) -> u64 {
        self.bytes_sent_per_sec
            .saturating_add(self.bytes_received_per_sec)
    }

    /// Calculate error rate as percentage
    #[must_use]
    pub fn error_rate_percent(&self) -> f64 {
        if self.total_commands > 0 {
            (self.errors as f64 / self.total_commands as f64) * 100.0
        } else {
            0.0
        }
    }

    /// Check if user has any activity
    #[must_use]
    #[inline]
    pub fn has_activity(&self) -> bool {
        self.total_commands > 0 || self.total_connections > 0
    }

    /// Check if user is currently connected
    #[must_use]
    #[inline]
    pub fn is_connected(&self) -> bool {
        self.active_connections > 0
    }
}
