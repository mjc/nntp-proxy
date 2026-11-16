//! Fixed-bucket latency histograms for export.
//!
//! Recording happens on the request path, so it is one binary search over a
//! small constant table and two relaxed atomic adds: no allocation, no lock,
//! and no floating point. Buckets are fixed at compile time; Prometheus
//! histograms cannot change bucket bounds without breaking queries anyway.

use std::sync::atomic::{AtomicU64, Ordering};

/// Number of finite bucket bounds; one more bucket counts everything above.
pub const BUCKETS: usize = 13;

/// Upper bounds in microseconds, with the matching `le` label spelled in
/// seconds so the scrape needs no float formatting per bucket.
#[derive(Debug, PartialEq, Eq)]
pub struct BucketBounds {
    pub micros: [u64; BUCKETS],
    pub labels: [&'static str; BUCKETS],
}

/// Time to first response bytes: a healthy backend answers in milliseconds,
/// a struggling one in seconds.
pub static FIRST_BYTE_BUCKETS: BucketBounds = BucketBounds {
    micros: [
        1_000, 2_500, 5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000, 1_000_000,
        2_500_000, 5_000_000, 10_000_000,
    ],
    labels: [
        "0.001", "0.0025", "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5",
        "10",
    ],
};

/// Whole-command connection hold time: includes transferring article bodies
/// of up to several megabytes, so the range reaches a minute.
pub static EXCHANGE_BUCKETS: BucketBounds = BucketBounds {
    micros: [
        5_000, 10_000, 25_000, 50_000, 100_000, 250_000, 500_000, 1_000_000, 2_500_000, 5_000_000,
        10_000_000, 30_000_000, 60_000_000,
    ],
    labels: [
        "0.005", "0.01", "0.025", "0.05", "0.1", "0.25", "0.5", "1", "2.5", "5", "10", "30", "60",
    ],
};

/// Lock-free latency histogram with fixed bounds.
#[derive(Debug)]
pub struct LatencyHistogram {
    bounds: &'static BucketBounds,
    /// Per-bucket (non-cumulative) counts; the last slot is above every bound.
    counts: [AtomicU64; BUCKETS + 1],
    sum_micros: AtomicU64,
}

/// Plain copy of a histogram for rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistogramSnapshot {
    pub bounds: &'static BucketBounds,
    /// Per-bucket (non-cumulative) counts; the last slot is above every bound.
    pub counts: [u64; BUCKETS + 1],
    pub sum_micros: u64,
}

impl LatencyHistogram {
    #[must_use]
    pub fn new(bounds: &'static BucketBounds) -> Self {
        Self {
            bounds,
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            sum_micros: AtomicU64::new(0),
        }
    }

    /// Record one observation. Values equal to a bound fall in that bucket,
    /// matching Prometheus `le` (less than or equal) semantics.
    #[inline]
    pub fn observe_micros(&self, micros: u64) {
        let bucket = self.bounds.micros.partition_point(|&bound| bound < micros);
        self.counts[bucket].fetch_add(1, Ordering::Relaxed);
        self.sum_micros.fetch_add(micros, Ordering::Relaxed);
    }

    #[must_use]
    pub fn snapshot(&self) -> HistogramSnapshot {
        HistogramSnapshot {
            bounds: self.bounds,
            counts: std::array::from_fn(|i| self.counts[i].load(Ordering::Relaxed)),
            sum_micros: self.sum_micros.load(Ordering::Relaxed),
        }
    }
}

impl HistogramSnapshot {
    /// An empty snapshot, for backends that have not been measured.
    #[must_use]
    pub const fn empty(bounds: &'static BucketBounds) -> Self {
        Self {
            bounds,
            counts: [0; BUCKETS + 1],
            sum_micros: 0,
        }
    }

    /// Total observations. Summed from the buckets rather than kept as a
    /// separate counter so `_count` always equals the `+Inf` bucket.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.counts.iter().sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn label_seconds_to_micros(label: &str) -> u64 {
        let seconds: f64 = label.parse().unwrap();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let micros = (seconds * 1_000_000.0).round() as u64;
        micros
    }

    #[test]
    fn labels_match_bounds_and_bounds_increase() {
        for bounds in [&FIRST_BYTE_BUCKETS, &EXCHANGE_BUCKETS] {
            for (micros, label) in bounds.micros.iter().zip(bounds.labels) {
                assert_eq!(label_seconds_to_micros(label), *micros, "{label}");
            }
            let increasing = bounds
                .micros
                .iter()
                .zip(bounds.micros.iter().skip(1))
                .all(|(lower, upper)| lower < upper);
            assert!(increasing);
        }
    }

    #[test]
    fn observations_land_in_le_buckets() {
        let histogram = LatencyHistogram::new(&FIRST_BYTE_BUCKETS);
        histogram.observe_micros(0); // first bucket
        histogram.observe_micros(1_000); // exactly on the first bound
        histogram.observe_micros(1_001); // second bucket
        histogram.observe_micros(10_000_000); // last finite bound
        histogram.observe_micros(10_000_001); // overflow

        let snapshot = histogram.snapshot();
        assert_eq!(snapshot.counts[0], 2);
        assert_eq!(snapshot.counts[1], 1);
        assert_eq!(snapshot.counts[BUCKETS - 1], 1);
        assert_eq!(snapshot.counts[BUCKETS], 1);
        assert_eq!(snapshot.count(), 5);
        assert_eq!(snapshot.sum_micros, 1_000 + 1_001 + 10_000_000 + 10_000_001);
    }

    #[test]
    fn empty_snapshot_counts_nothing() {
        let snapshot = HistogramSnapshot::empty(&EXCHANGE_BUCKETS);
        assert_eq!(snapshot.count(), 0);
        assert_eq!(
            snapshot,
            LatencyHistogram::new(&EXCHANGE_BUCKETS).snapshot()
        );
    }
}
