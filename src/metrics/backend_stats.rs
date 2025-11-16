//! Backend statistics snapshot type

use super::types::*;

/// Statistics for a single backend (cloneable snapshot)
#[derive(Debug, Clone)]
pub struct BackendStats {
    pub backend_id: usize,
    pub active_connections: ActiveConnections,
    pub total_commands: CommandCount,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub errors: ErrorCount,
    pub errors_4xx: ErrorCount,
    pub errors_5xx: ErrorCount,
    pub article_bytes_total: u64,
    pub article_count: ArticleCount,
    pub ttfb_micros_total: TtfbMicros,
    pub ttfb_count: u64,
    pub send_micros_total: SendMicros,
    pub recv_micros_total: RecvMicros,
    pub connection_failures: FailureCount,
    pub health_status: HealthStatus,
}

impl Default for BackendStats {
    fn default() -> Self {
        Self {
            backend_id: 0,
            active_connections: ActiveConnections::default(),
            total_commands: CommandCount::default(),
            bytes_sent: 0,
            bytes_received: 0,
            errors: ErrorCount::default(),
            errors_4xx: ErrorCount::default(),
            errors_5xx: ErrorCount::default(),
            article_bytes_total: 0,
            article_count: ArticleCount::default(),
            ttfb_micros_total: TtfbMicros::default(),
            ttfb_count: 0,
            send_micros_total: SendMicros::default(),
            recv_micros_total: RecvMicros::default(),
            connection_failures: FailureCount::default(),
            health_status: HealthStatus::Healthy,
        }
    }
}

impl BackendStats {
    /// Calculate average article size
    #[must_use]
    #[inline]
    pub fn average_article_size(&self) -> Option<u64> {
        self.article_count.average_bytes(self.article_bytes_total)
    }

    /// Calculate average time to first byte in milliseconds
    #[must_use]
    #[inline]
    pub fn average_ttfb_ms(&self) -> Option<f64> {
        std::num::NonZeroU64::new(self.ttfb_count)
            .map(|count| TtfbMicros::average(self.ttfb_micros_total, count).get())
    }

    /// Calculate average send time in milliseconds
    #[must_use]
    #[inline]
    pub fn average_send_ms(&self) -> Option<f64> {
        std::num::NonZeroU64::new(self.ttfb_count)
            .map(|count| SendMicros::average(self.send_micros_total, count).get())
    }

    /// Calculate average receive time in milliseconds
    #[must_use]
    #[inline]
    pub fn average_recv_ms(&self) -> Option<f64> {
        std::num::NonZeroU64::new(self.ttfb_count)
            .map(|count| RecvMicros::average(self.recv_micros_total, count).get())
    }

    /// Calculate async overhead in milliseconds
    #[must_use]
    #[inline]
    pub fn average_overhead_ms(&self) -> Option<f64> {
        std::num::NonZeroU64::new(self.ttfb_count).map(|count| {
            let ttfb = TtfbMicros::average(self.ttfb_micros_total, count);
            let send = SendMicros::average(self.send_micros_total, count);
            let recv = RecvMicros::average(self.recv_micros_total, count);
            OverheadMillis::from_components(ttfb, send, recv).get()
        })
    }

    /// Calculate error rate as a percentage
    #[must_use]
    #[inline]
    pub fn error_rate_percent(&self) -> f64 {
        ErrorRatePercent::from_counts(self.errors, self.total_commands).get()
    }

    /// Check if backend has high error rate (> 5%)
    #[must_use]
    #[inline]
    pub fn has_high_error_rate(&self) -> bool {
        ErrorRatePercent::from_counts(self.errors, self.total_commands).is_high()
    }

    /// Check if backend is healthy
    #[must_use]
    #[inline]
    pub fn is_healthy(&self) -> bool {
        self.health_status == HealthStatus::Healthy
    }

    /// Check if backend is degraded
    #[must_use]
    #[inline]
    pub fn is_degraded(&self) -> bool {
        self.health_status == HealthStatus::Degraded
    }

    /// Check if backend is down
    #[must_use]
    #[inline]
    pub fn is_down(&self) -> bool {
        self.health_status == HealthStatus::Down
    }

    /// Get total bytes transferred
    #[must_use]
    #[inline]
    pub fn total_bytes(&self) -> u64 {
        self.bytes_sent.saturating_add(self.bytes_received)
    }

    /// Check if backend has any activity
    #[must_use]
    #[inline]
    pub fn has_activity(&self) -> bool {
        self.total_commands.get() > 0
    }
}
