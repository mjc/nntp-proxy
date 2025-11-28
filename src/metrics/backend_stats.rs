//! Backend statistics snapshot type

use super::types::*;
use crate::types::{
    ArticleBytesTotal, BackendId, BytesReceived, BytesSent, TimingMeasurementCount,
};

/// Statistics for a single backend (cloneable snapshot)
#[derive(Debug, Clone)]
pub struct BackendStats {
    pub backend_id: BackendId,
    pub active_connections: ActiveConnections,
    pub total_commands: CommandCount,
    pub bytes_sent: BytesSent,
    pub bytes_received: BytesReceived,
    pub errors: ErrorCount,
    pub errors_4xx: ErrorCount,
    pub errors_5xx: ErrorCount,
    pub article_bytes_total: ArticleBytesTotal,
    pub article_count: ArticleCount,
    pub ttfb_micros_total: TtfbMicros,
    pub ttfb_count: TimingMeasurementCount,
    pub send_micros_total: SendMicros,
    pub recv_micros_total: RecvMicros,
    pub connection_failures: FailureCount,
    pub health_status: HealthStatus,
    /// Number of times STAT and HEAD disagreed during precheck
    pub precheck_disagreements: u64,
}

impl Default for BackendStats {
    fn default() -> Self {
        Self {
            backend_id: BackendId::from_index(0),
            active_connections: ActiveConnections::default(),
            total_commands: CommandCount::default(),
            bytes_sent: BytesSent::ZERO,
            bytes_received: BytesReceived::ZERO,
            errors: ErrorCount::default(),
            errors_4xx: ErrorCount::default(),
            errors_5xx: ErrorCount::default(),
            article_bytes_total: ArticleBytesTotal::ZERO,
            article_count: ArticleCount::default(),
            ttfb_micros_total: TtfbMicros::default(),
            ttfb_count: TimingMeasurementCount::ZERO,
            send_micros_total: SendMicros::default(),
            recv_micros_total: RecvMicros::default(),
            connection_failures: FailureCount::default(),
            health_status: HealthStatus::Healthy,
            precheck_disagreements: 0,
        }
    }
}

impl BackendStats {
    /// Calculate average article size
    #[must_use]
    #[inline]
    pub fn average_article_size(&self) -> Option<u64> {
        self.article_count
            .average_bytes(self.article_bytes_total.get())
    }

    /// Calculate average time to first byte in milliseconds
    #[must_use]
    #[inline]
    pub fn average_ttfb_ms(&self) -> Option<f64> {
        std::num::NonZeroU64::new(self.ttfb_count.get())
            .map(|count| TtfbMicros::average(self.ttfb_micros_total, count).get())
    }

    /// Calculate average send time in milliseconds
    #[must_use]
    #[inline]
    pub fn average_send_ms(&self) -> Option<f64> {
        std::num::NonZeroU64::new(self.ttfb_count.get())
            .map(|count| SendMicros::average(self.send_micros_total, count).get())
    }

    /// Calculate average receive time in milliseconds
    #[must_use]
    #[inline]
    pub fn average_recv_ms(&self) -> Option<f64> {
        std::num::NonZeroU64::new(self.ttfb_count.get())
            .map(|count| RecvMicros::average(self.recv_micros_total, count).get())
    }

    /// Calculate async overhead in milliseconds
    #[must_use]
    #[inline]
    pub fn average_overhead_ms(&self) -> Option<f64> {
        std::num::NonZeroU64::new(self.ttfb_count.get()).map(|count| {
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
        self.bytes_sent
            .as_u64()
            .saturating_add(self.bytes_received.as_u64())
    }

    /// Calculate adaptive timeout based on backend performance
    ///
    /// Returns timeout = mean + 3*mean (approximates mean + 2*stddev for normal distribution)
    /// Falls back to 60 seconds if no measurements available.
    ///
    /// This prevents timeouts on legitimately slow backends while catching servers
    /// that deliberately delay 430 responses.
    #[must_use]
    pub fn adaptive_timeout(&self, _default_timeout: std::time::Duration) -> std::time::Duration {
        use std::time::Duration;

        // Need at least 10 measurements for reasonable estimate
        const MIN_MEASUREMENTS: u64 = 10;

        // Start with conservative 60s timeout until we have data
        const FALLBACK_TIMEOUT_SECS: u64 = 60;

        if self.ttfb_count.get() < MIN_MEASUREMENTS {
            return Duration::from_secs(FALLBACK_TIMEOUT_SECS);
        }

        // Calculate mean TTFB in seconds
        if let Some(mean_ms) = self.average_ttfb_ms() {
            let mean_secs = mean_ms / 1000.0;

            // Use 3x mean as approximation of mean + 2*stddev
            // (for normal distribution, ~95% of values fall within mean ± 2*stddev)
            let adaptive_secs = mean_secs * 3.0;

            // Clamp to reasonable bounds: [5s, 120s]
            const MIN_TIMEOUT_SECS: f64 = 5.0;
            const MAX_TIMEOUT_SECS: f64 = 120.0;

            let clamped_secs = adaptive_secs.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS);
            Duration::from_secs_f64(clamped_secs)
        } else {
            Duration::from_secs(FALLBACK_TIMEOUT_SECS)
        }
    }

    /// Check if backend has any activity
    #[must_use]
    #[inline]
    pub fn has_activity(&self) -> bool {
        self.total_commands.get() > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::BackendId;

    fn create_test_stats() -> BackendStats {
        BackendStats {
            backend_id: BackendId::from_index(0),
            total_commands: CommandCount::new(100),
            errors: ErrorCount::new(5),
            bytes_sent: BytesSent::new(1000),
            bytes_received: BytesReceived::new(2000),
            article_count: ArticleCount::new(10),
            article_bytes_total: ArticleBytesTotal::new(5000),
            ttfb_micros_total: TtfbMicros::new(10000),
            ttfb_count: TimingMeasurementCount::new(10),
            send_micros_total: SendMicros::new(3000),
            recv_micros_total: RecvMicros::new(5000),
            health_status: HealthStatus::Healthy,
            ..Default::default()
        }
    }

    #[test]
    fn test_backend_stats_default() {
        let stats = BackendStats::default();
        assert_eq!(stats.backend_id, BackendId::from_index(0));
        assert_eq!(stats.total_commands.get(), 0);
        assert_eq!(stats.errors.get(), 0);
        assert_eq!(stats.bytes_sent.as_u64(), 0);
        assert_eq!(stats.bytes_received.as_u64(), 0);
        assert_eq!(stats.health_status, HealthStatus::Healthy);
    }

    #[test]
    fn test_average_article_size() {
        let stats = create_test_stats();
        let avg = stats.average_article_size();
        assert_eq!(avg, Some(500)); // 5000 / 10
    }

    #[test]
    fn test_average_article_size_zero_articles() {
        let stats = BackendStats {
            article_count: ArticleCount::new(0),
            article_bytes_total: ArticleBytesTotal::new(1000),
            ..Default::default()
        };
        assert_eq!(stats.average_article_size(), None);
    }

    #[test]
    fn test_average_article_size_zero_bytes() {
        let stats = BackendStats {
            article_count: ArticleCount::new(10),
            article_bytes_total: ArticleBytesTotal::new(0),
            ..Default::default()
        };
        assert_eq!(stats.average_article_size(), Some(0));
    }

    #[test]
    fn test_average_ttfb_ms() {
        let stats = create_test_stats();
        let avg = stats.average_ttfb_ms();
        assert!(avg.is_some());
        // 10000 micros / 10 measurements = 1000 micros = 1.0 ms
        assert!((avg.unwrap() - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_average_ttfb_ms_zero_count() {
        let stats = BackendStats {
            ttfb_micros_total: TtfbMicros::new(1000),
            ttfb_count: TimingMeasurementCount::new(0),
            ..Default::default()
        };
        assert_eq!(stats.average_ttfb_ms(), None);
    }

    #[test]
    fn test_average_send_ms() {
        let stats = create_test_stats();
        let avg = stats.average_send_ms();
        assert!(avg.is_some());
        // 3000 micros / 10 = 300 micros = 0.3 ms
        assert!((avg.unwrap() - 0.3).abs() < 0.01);
    }

    #[test]
    fn test_average_send_ms_zero_count() {
        let stats = BackendStats {
            send_micros_total: SendMicros::new(1000),
            ttfb_count: TimingMeasurementCount::new(0),
            ..Default::default()
        };
        assert_eq!(stats.average_send_ms(), None);
    }

    #[test]
    fn test_average_recv_ms() {
        let stats = create_test_stats();
        let avg = stats.average_recv_ms();
        assert!(avg.is_some());
        // 5000 micros / 10 = 500 micros = 0.5 ms
        assert!((avg.unwrap() - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_average_recv_ms_zero_count() {
        let stats = BackendStats {
            recv_micros_total: RecvMicros::new(1000),
            ttfb_count: TimingMeasurementCount::new(0),
            ..Default::default()
        };
        assert_eq!(stats.average_recv_ms(), None);
    }

    #[test]
    fn test_average_overhead_ms() {
        let stats = create_test_stats();
        let overhead = stats.average_overhead_ms();
        assert!(overhead.is_some());
        // ttfb(1.0) - send(0.3) - recv(0.5) = 0.2 ms overhead
        assert!((overhead.unwrap() - 0.2).abs() < 0.01);
    }

    #[test]
    fn test_average_overhead_ms_zero_count() {
        let stats = BackendStats::default();
        assert_eq!(stats.average_overhead_ms(), None);
    }

    #[test]
    fn test_error_rate_percent() {
        let stats = create_test_stats();
        let rate = stats.error_rate_percent();
        assert!((rate - 5.0).abs() < 0.01); // 5/100 = 5%
    }

    #[test]
    fn test_error_rate_percent_zero_commands() {
        let stats = BackendStats {
            total_commands: CommandCount::new(0),
            errors: ErrorCount::new(10),
            ..Default::default()
        };
        assert_eq!(stats.error_rate_percent(), 0.0);
    }

    #[test]
    fn test_error_rate_percent_no_errors() {
        let stats = BackendStats {
            total_commands: CommandCount::new(100),
            errors: ErrorCount::new(0),
            ..Default::default()
        };
        assert_eq!(stats.error_rate_percent(), 0.0);
    }

    #[test]
    fn test_has_high_error_rate() {
        // 10% error rate - should be high
        let stats = BackendStats {
            total_commands: CommandCount::new(100),
            errors: ErrorCount::new(10),
            ..Default::default()
        };
        assert!(stats.has_high_error_rate());

        // 5% error rate - exactly at threshold, not high
        let stats_threshold = BackendStats {
            total_commands: CommandCount::new(100),
            errors: ErrorCount::new(5),
            ..Default::default()
        };
        assert!(!stats_threshold.has_high_error_rate());

        // 3% error rate - should not be high
        let stats_low = BackendStats {
            total_commands: CommandCount::new(100),
            errors: ErrorCount::new(3),
            ..Default::default()
        };
        assert!(!stats_low.has_high_error_rate());
    }

    #[test]
    fn test_is_healthy() {
        let stats = BackendStats {
            health_status: HealthStatus::Healthy,
            ..Default::default()
        };
        assert!(stats.is_healthy());
        assert!(!stats.is_degraded());
        assert!(!stats.is_down());
    }

    #[test]
    fn test_is_degraded() {
        let stats = BackendStats {
            health_status: HealthStatus::Degraded,
            ..Default::default()
        };
        assert!(!stats.is_healthy());
        assert!(stats.is_degraded());
        assert!(!stats.is_down());
    }

    #[test]
    fn test_is_down() {
        let stats = BackendStats {
            health_status: HealthStatus::Down,
            ..Default::default()
        };
        assert!(!stats.is_healthy());
        assert!(!stats.is_degraded());
        assert!(stats.is_down());
    }

    #[test]
    fn test_total_bytes() {
        let stats = create_test_stats();
        assert_eq!(stats.total_bytes(), 3000); // 1000 + 2000
    }

    #[test]
    fn test_total_bytes_zero() {
        let stats = BackendStats::default();
        assert_eq!(stats.total_bytes(), 0);
    }

    #[test]
    fn test_total_bytes_saturating() {
        let stats = BackendStats {
            bytes_sent: BytesSent::new(u64::MAX),
            bytes_received: BytesReceived::new(1),
            ..Default::default()
        };
        assert_eq!(stats.total_bytes(), u64::MAX);
    }

    #[test]
    fn test_has_activity() {
        let stats = BackendStats {
            total_commands: CommandCount::new(1),
            ..Default::default()
        };
        assert!(stats.has_activity());
    }

    #[test]
    fn test_has_activity_none() {
        let stats = BackendStats::default();
        assert!(!stats.has_activity());
    }

    #[test]
    fn test_backend_stats_clone() {
        let stats = create_test_stats();
        let cloned = stats.clone();

        assert_eq!(stats.backend_id, cloned.backend_id);
        assert_eq!(stats.total_commands.get(), cloned.total_commands.get());
        assert_eq!(stats.total_bytes(), cloned.total_bytes());
        assert_eq!(stats.health_status, cloned.health_status);
    }

    #[test]
    fn test_adaptive_timeout_with_sufficient_measurements() {
        use std::time::Duration;

        // Backend with 1ms average TTFB (1000 microseconds)
        let stats = BackendStats {
            ttfb_micros_total: TtfbMicros::new(10_000), // 10 measurements * 1000 micros each
            ttfb_count: TimingMeasurementCount::new(10),
            ..Default::default()
        };

        let default = Duration::from_secs(15);
        let timeout = stats.adaptive_timeout(default);

        // Should be 3x mean = 3ms, but clamped to min 5s
        assert_eq!(timeout, Duration::from_secs(5));
    }

    #[test]
    fn test_adaptive_timeout_slow_backend() {
        use std::time::Duration;

        // Slow backend: 10s average TTFB
        let stats = BackendStats {
            ttfb_micros_total: TtfbMicros::new(100_000_000), // 10 * 10,000,000 micros
            ttfb_count: TimingMeasurementCount::new(10),
            ..Default::default()
        };

        let default = Duration::from_secs(15);
        let timeout = stats.adaptive_timeout(default);

        // 3x mean = 30s
        assert_eq!(timeout, Duration::from_secs(30));
    }

    #[test]
    fn test_adaptive_timeout_very_slow_backend_clamped() {
        use std::time::Duration;

        // Very slow backend: 50s average TTFB
        let stats = BackendStats {
            ttfb_micros_total: TtfbMicros::new(500_000_000), // 10 * 50,000,000 micros
            ttfb_count: TimingMeasurementCount::new(10),
            ..Default::default()
        };

        let default = Duration::from_secs(15);
        let timeout = stats.adaptive_timeout(default);

        // 3x mean = 150s, but clamped to max 120s
        assert_eq!(timeout, Duration::from_secs(120));
    }

    #[test]
    fn test_adaptive_timeout_insufficient_measurements() {
        use std::time::Duration;

        // Only 5 measurements - not enough for reliable estimate
        let stats = BackendStats {
            ttfb_micros_total: TtfbMicros::new(5000),
            ttfb_count: TimingMeasurementCount::new(5),
            ..Default::default()
        };

        let default = Duration::from_secs(15);
        let timeout = stats.adaptive_timeout(default);

        // Should return 60s fallback (conservative for BODY/ARTICLE)
        assert_eq!(timeout, Duration::from_secs(60));
    }

    #[test]
    fn test_adaptive_timeout_no_measurements() {
        use std::time::Duration;

        let stats = BackendStats::default();
        let default = Duration::from_secs(15);
        let timeout = stats.adaptive_timeout(default);

        // Should return 60s fallback (conservative for BODY/ARTICLE)
        assert_eq!(timeout, Duration::from_secs(60));
    }
}
