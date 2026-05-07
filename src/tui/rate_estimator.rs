//! TUI-local rate estimation for cumulative counters.

use crate::types::tui::Timestamp;

/// Cumulative monotonically increasing counter sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CumulativeCount(u64);

impl CumulativeCount {
    #[must_use]
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    const fn get(self) -> u64 {
        self.0
    }
}

/// Estimated events per second.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub(crate) struct RatePerSecond(f64);

impl RatePerSecond {
    #[must_use]
    pub(crate) const fn new(value: f64) -> Self {
        Self(value)
    }

    #[must_use]
    pub(crate) const fn zero() -> Self {
        Self(0.0)
    }

    #[must_use]
    pub(crate) const fn get(self) -> f64 {
        self.0
    }
}

/// Raw and smoothed estimate for one counter movement sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct RateEstimate {
    raw: RatePerSecond,
    smoothed: RatePerSecond,
}

impl RateEstimate {
    #[must_use]
    pub(crate) const fn new(raw: RatePerSecond, smoothed: RatePerSecond) -> Self {
        Self { raw, smoothed }
    }

    #[must_use]
    pub(crate) const fn raw(self) -> RatePerSecond {
        self.raw
    }

    #[must_use]
    pub(crate) const fn smoothed(self) -> RatePerSecond {
        self.smoothed
    }
}

/// Time horizon used by the EWMA decay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct EstimatorHorizon(f64);

impl EstimatorHorizon {
    const DEFAULT_SECS: f64 = 15.0;

    #[must_use]
    const fn default() -> Self {
        Self(Self::DEFAULT_SECS)
    }

    #[must_use]
    fn weight(self, delta_secs: f64) -> f64 {
        0.1_f64.powf(delta_secs / self.0)
    }
}

/// Double-smoothed EWMA rate estimator for cumulative counters.
#[derive(Debug, Clone)]
pub(crate) struct RateEstimator {
    previous: Option<(CumulativeCount, Timestamp)>,
    first_ewma: Option<RatePerSecond>,
    second_ewma: Option<RatePerSecond>,
    horizon: EstimatorHorizon,
}

impl Default for RateEstimator {
    fn default() -> Self {
        Self {
            previous: None,
            first_ewma: None,
            second_ewma: None,
            horizon: EstimatorHorizon::default(),
        }
    }
}

impl RateEstimator {
    /// Record a cumulative counter sample and return an estimate when the counter moved.
    pub(crate) fn record(
        &mut self,
        total: CumulativeCount,
        now: Timestamp,
    ) -> Option<RateEstimate> {
        let Some((previous_total, previous_time)) = self.previous else {
            self.previous = Some((total, now));
            return None;
        };

        if total.get() < previous_total.get() {
            self.reset_to(total, now);
            return None;
        }

        let delta_count = total.get() - previous_total.get();
        if delta_count == 0 {
            return None;
        }

        let delta_secs = now.duration_since(previous_time).as_secs_f64();
        if delta_secs <= 0.0 {
            return None;
        }

        let raw = RatePerSecond::new(delta_count as f64 / delta_secs);
        let smoothed = self.smooth(raw, delta_secs);
        self.previous = Some((total, now));

        Some(RateEstimate::new(raw, smoothed))
    }

    /// Return the current smoothed rate, decayed for time spent idle.
    #[must_use]
    pub(crate) fn rate_at(&self, now: Timestamp) -> RatePerSecond {
        let Some((_, previous_time)) = self.previous else {
            return RatePerSecond::zero();
        };
        let Some(second_ewma) = self.second_ewma else {
            return RatePerSecond::zero();
        };

        let delta_secs = now.duration_since(previous_time).as_secs_f64();
        if delta_secs <= 0.0 {
            return second_ewma;
        }

        RatePerSecond::new(second_ewma.get() * self.horizon.weight(delta_secs))
    }

    fn reset_to(&mut self, total: CumulativeCount, now: Timestamp) {
        self.previous = Some((total, now));
        self.first_ewma = None;
        self.second_ewma = None;
    }

    fn smooth(&mut self, raw: RatePerSecond, delta_secs: f64) -> RatePerSecond {
        let weight = self.horizon.weight(delta_secs);
        let first = Self::weighted_average(self.first_ewma, raw, weight);
        let second = Self::weighted_average(self.second_ewma, first, weight);

        self.first_ewma = Some(first);
        self.second_ewma = Some(second);

        second
    }

    fn weighted_average(
        previous: Option<RatePerSecond>,
        current: RatePerSecond,
        weight: f64,
    ) -> RatePerSecond {
        previous.map_or(current, |previous| {
            RatePerSecond::new(
                previous
                    .get()
                    .mul_add(weight, current.get() * (1.0 - weight)),
            )
        })
    }
}

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
