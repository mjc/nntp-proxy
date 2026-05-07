//! TUI-local rate estimation for cumulative counters.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::tui::Timestamp;
    use std::time::{Duration, Instant};

    fn ts(base: Instant, secs: u64) -> Timestamp {
        Timestamp::new(base + Duration::from_secs(secs))
    }

    fn assert_close(actual: f64, expected: f64) {
        let diff = (actual - expected).abs();
        assert!(
            diff < 0.001,
            "expected {actual} to be within 0.001 of {expected}"
        );
    }

    #[test]
    fn estimator_converges_on_steady_rate() {
        let base = Instant::now();
        let mut estimator = RateEstimator::default();

        assert!(
            estimator
                .record(CumulativeCount::new(0), ts(base, 0))
                .is_none()
        );

        let mut latest = None;
        for second in 1..=120 {
            latest = estimator.record(CumulativeCount::new(second * 1_000), ts(base, second));
        }

        let estimate = latest.expect("steady samples should produce estimates");
        assert_close(estimate.raw().get(), 1_000.0);
        assert_close(estimate.smoothed().get(), 1_000.0);
    }

    #[test]
    fn estimator_smooths_short_bursts() {
        let base = Instant::now();
        let mut estimator = RateEstimator::default();

        estimator.record(CumulativeCount::new(0), ts(base, 0));
        estimator.record(CumulativeCount::new(1_000), ts(base, 1));
        let burst = estimator
            .record(CumulativeCount::new(101_000), ts(base, 2))
            .expect("burst sample should produce estimate");

        assert_close(burst.raw().get(), 100_000.0);
        assert!(
            burst.smoothed().get() < burst.raw().get(),
            "smoothed rate should dampen raw burst"
        );
        assert!(
            burst.smoothed().get() > 1_000.0,
            "smoothed rate should still move toward the burst"
        );
    }

    #[test]
    fn estimator_decays_while_idle() {
        let base = Instant::now();
        let mut estimator = RateEstimator::default();

        estimator.record(CumulativeCount::new(0), ts(base, 0));
        estimator.record(CumulativeCount::new(15_000), ts(base, 15));
        let active_rate = estimator.rate_at(ts(base, 15));
        let idle_rate = estimator.rate_at(ts(base, 30));

        assert!(active_rate.get() > 0.0);
        assert!(
            idle_rate.get() < active_rate.get(),
            "idle rate should decay without new counter movement"
        );
    }

    #[test]
    fn estimator_resets_after_counter_rewind() {
        let base = Instant::now();
        let mut estimator = RateEstimator::default();

        estimator.record(CumulativeCount::new(10_000), ts(base, 0));
        estimator.record(CumulativeCount::new(20_000), ts(base, 1));
        assert!(
            estimator
                .record(CumulativeCount::new(5_000), ts(base, 2))
                .is_none()
        );

        let after_reset = estimator
            .record(CumulativeCount::new(6_000), ts(base, 3))
            .expect("post-reset increment should produce an estimate");
        assert_close(after_reset.raw().get(), 1_000.0);
        assert_close(after_reset.smoothed().get(), 1_000.0);
    }

    #[test]
    fn estimator_ignores_zero_time_and_unchanged_samples() {
        let base = Instant::now();
        let mut estimator = RateEstimator::default();

        estimator.record(CumulativeCount::new(0), ts(base, 0));
        assert!(
            estimator
                .record(CumulativeCount::new(1_000), ts(base, 0))
                .is_none()
        );
        assert!(
            estimator
                .record(CumulativeCount::new(0), ts(base, 1))
                .is_none()
        );

        let estimate = estimator
            .record(CumulativeCount::new(1_000), ts(base, 2))
            .expect("changed sample with elapsed time should estimate");
        assert_close(estimate.raw().get(), 500.0);
    }
}
