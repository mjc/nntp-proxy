//! Backend selection strategies
//!
//! This module contains different algorithms for selecting backend servers:
//! - Weighted round-robin: Distributes based on connection pool size
//! - Least-loaded: Selects backend with fewest pending requests

use std::sync::atomic::{AtomicUsize, Ordering};

/// Strategy for selecting backends based on weighted round-robin
///
/// Uses the pool size (`max_connections`) as the weight to ensure backends
/// with larger pools receive proportionally more requests.
///
/// Algorithm: Map counter to weighted position, then find which backend owns that slot.
#[derive(Debug)]
pub struct WeightedRoundRobin {
    /// Current counter for round-robin selection
    counter: AtomicUsize,
    /// Total weight across all backends (sum of all `max_connections`)
    total_weight: usize,
}

impl WeightedRoundRobin {
    /// Create a new weighted round-robin strategy
    #[must_use]
    pub const fn new(total_weight: usize) -> Self {
        Self {
            counter: AtomicUsize::new(0),
            total_weight,
        }
    }

    /// Update total weight when backends are added
    pub const fn set_total_weight(&mut self, total_weight: usize) {
        self.total_weight = total_weight;
    }

    /// Select backend index using a specific weight
    ///
    /// This method allows tier-aware selection by using a tier's total weight
    /// instead of the global total weight. This avoids modulo bias when selecting
    /// within a tier that has a different total weight than the global weight.
    ///
    /// The atomic counter is still shared across all calls, ensuring fair
    /// distribution even when tier weights change.
    #[must_use]
    pub fn select_with_weight(&self, weight: usize) -> Option<usize> {
        if weight == 0 {
            return None;
        }

        let counter = self.counter.fetch_add(1, Ordering::Relaxed);
        Some(counter % weight)
    }

    /// Get total weight
    #[must_use]
    pub const fn total_weight(&self) -> usize {
        self.total_weight
    }
}

/// Strategy for selecting backends based on current load
///
/// Routes requests to the backend with the fewest pending requests,
/// accounting for backend capacity (`max_connections`).
///
/// Algorithm: Calculate `load_ratio` = pending / `max_connections` for each backend,
/// select the one with lowest ratio. Breaks equal-load ties with a cheap
/// pseudo-random replacement decision.
#[derive(Debug)]
pub struct LeastLoaded {
    tie_breaker: AtomicUsize,
}

impl LeastLoaded {
    /// Create a new least-loaded strategy
    #[must_use]
    pub const fn new() -> Self {
        Self {
            tie_breaker: AtomicUsize::new(0),
        }
    }

    /// Return true when an equal-load candidate should replace the current winner.
    ///
    /// This is only called after a real tie is found. It keeps the common no-tie
    /// path free of random work and avoids allocating a candidate list.
    #[must_use]
    pub fn should_replace_tie(&self, tie_count: usize) -> bool {
        debug_assert!(tie_count > 1);
        if tie_count <= 1 {
            return false;
        }

        let counter = self
            .tie_breaker
            .fetch_add(0x9e37_79b9_7f4a_7c15usize, Ordering::Relaxed);
        Self::mix(counter).is_multiple_of(tie_count)
    }

    #[inline]
    fn mix(mut value: usize) -> usize {
        value ^= value >> 16;
        value = value.wrapping_mul(0x7feb_352d);
        value ^= value >> 15;
        value = value.wrapping_mul(0x846c_a68b);
        value ^ (value >> 16)
    }
}

impl Default for LeastLoaded {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn test_weighted_round_robin_basic() {
        let strategy = WeightedRoundRobin::new(10);

        // Should cycle through 0-9
        for i in 0..20 {
            assert_eq!(
                strategy.select_with_weight(strategy.total_weight()),
                Some(i % 10)
            );
        }
    }

    #[test]
    fn test_weighted_round_robin_zero_weight() {
        let strategy = WeightedRoundRobin::new(0);
        assert_eq!(strategy.select_with_weight(strategy.total_weight()), None);
    }

    #[test]
    fn test_weighted_round_robin_one_weight() {
        let strategy = WeightedRoundRobin::new(1);

        // All selections should return 0
        for _ in 0..100 {
            assert_eq!(
                strategy.select_with_weight(strategy.total_weight()),
                Some(0)
            );
        }
    }

    #[test]
    fn test_set_total_weight() {
        let mut strategy = WeightedRoundRobin::new(10);
        assert_eq!(strategy.total_weight(), 10);

        strategy.set_total_weight(20);
        assert_eq!(strategy.total_weight(), 20);
    }

    #[test]
    fn test_set_total_weight_to_zero() {
        let mut strategy = WeightedRoundRobin::new(10);

        // First should work
        assert!(
            strategy
                .select_with_weight(strategy.total_weight())
                .is_some()
        );

        // Change to zero
        strategy.set_total_weight(0);
        assert_eq!(strategy.total_weight(), 0);
        assert_eq!(strategy.select_with_weight(strategy.total_weight()), None);
    }

    #[test]
    fn test_weighted_round_robin_large_weight() {
        let strategy = WeightedRoundRobin::new(1000);

        // Should cycle through 0-999
        for i in 0..2000 {
            assert_eq!(
                strategy.select_with_weight(strategy.total_weight()),
                Some(i % 1000)
            );
        }
    }

    #[test]
    fn test_weighted_round_robin_odd_weight() {
        let strategy = WeightedRoundRobin::new(7);

        // Should cycle through 0-6
        for i in 0..21 {
            assert_eq!(
                strategy.select_with_weight(strategy.total_weight()),
                Some(i % 7)
            );
        }
    }

    #[test]
    fn test_weighted_round_robin_prime_weight() {
        let strategy = WeightedRoundRobin::new(13);

        // Test with prime number to ensure no modulo bias
        for i in 0..26 {
            assert_eq!(
                strategy.select_with_weight(strategy.total_weight()),
                Some(i % 13)
            );
        }
    }

    #[test]
    fn test_weighted_round_robin_counter_wraparound() {
        let strategy = WeightedRoundRobin::new(10);

        // Set counter to near max value (simulate wraparound without looping)
        strategy.counter.store(usize::MAX - 5, Ordering::Relaxed);

        // Next few selections should still work correctly
        // Even when counter wraps around, modulo should still produce valid results
        for _ in 0..10 {
            let result = strategy.select_with_weight(strategy.total_weight());
            assert!(result.is_some());
            assert!(result.unwrap() < 10);
        }
    }

    #[test]
    fn test_weighted_round_robin_distribution() {
        let strategy = WeightedRoundRobin::new(10);
        let mut counts = [0usize; 10];

        // Make 1000 selections
        for _ in 0..1000 {
            let pos = strategy
                .select_with_weight(strategy.total_weight())
                .unwrap();
            counts[pos] += 1;
        }

        // Each position should get exactly 100 selections
        for &count in &counts {
            assert_eq!(count, 100);
        }
    }

    #[test]
    fn test_weighted_round_robin_concurrent() {
        let strategy = Arc::new(WeightedRoundRobin::new(100));
        let mut handles = vec![];

        // Spawn 10 threads, each making 100 selections
        for _ in 0..10 {
            let strategy_clone = Arc::clone(&strategy);
            handles.push(thread::spawn(move || {
                let mut results = vec![];
                for _ in 0..100 {
                    results.push(strategy_clone.select_with_weight(100).unwrap());
                }
                results
            }));
        }

        // Collect all results
        let mut all_results = vec![];
        for handle in handles {
            all_results.extend(handle.join().unwrap());
        }

        // Should have 1000 total selections
        assert_eq!(all_results.len(), 1000);

        // All should be valid (< 100)
        for result in all_results {
            assert!(result < 100);
        }
    }

    #[test]
    fn test_weighted_round_robin_concurrent_distribution() {
        let strategy = Arc::new(WeightedRoundRobin::new(50));
        let mut handles = vec![];

        // Spawn 5 threads, each making 1000 selections
        for _ in 0..5 {
            let strategy_clone = Arc::clone(&strategy);
            handles.push(thread::spawn(move || {
                let mut counts = [0usize; 50];
                for _ in 0..1000 {
                    let pos = strategy_clone.select_with_weight(50).unwrap();
                    counts[pos] += 1;
                }
                counts
            }));
        }

        // Aggregate counts from all threads
        let mut total_counts = [0usize; 50];
        for handle in handles {
            let thread_counts = handle.join().unwrap();
            for (i, &count) in thread_counts.iter().enumerate() {
                total_counts[i] += count;
            }
        }

        // Each position should get exactly 100 selections (5 threads × 1000 / 50)
        for &count in &total_counts {
            assert_eq!(count, 100);
        }
    }

    #[test]
    fn test_weighted_round_robin_new_default_state() {
        let strategy = WeightedRoundRobin::new(42);

        // First selection should be 0 (counter starts at 0)
        assert_eq!(
            strategy.select_with_weight(strategy.total_weight()),
            Some(0)
        );
        assert_eq!(
            strategy.select_with_weight(strategy.total_weight()),
            Some(1)
        );
    }

    #[test]
    fn test_weighted_round_robin_debug_format() {
        let strategy = WeightedRoundRobin::new(10);
        let debug_str = format!("{strategy:?}");

        assert!(debug_str.contains("WeightedRoundRobin"));
    }

    #[test]
    fn test_set_total_weight_multiple_times() {
        let mut strategy = WeightedRoundRobin::new(10);

        assert_eq!(strategy.total_weight(), 10);
        assert_eq!(
            strategy.select_with_weight(strategy.total_weight()),
            Some(0)
        );

        strategy.set_total_weight(5);
        assert_eq!(strategy.total_weight(), 5);
        // Counter continues from 1, so 1 % 5 = 1
        assert_eq!(
            strategy.select_with_weight(strategy.total_weight()),
            Some(1)
        );

        strategy.set_total_weight(20);
        assert_eq!(strategy.total_weight(), 20);
        // Counter at 2, so 2 % 20 = 2
        assert_eq!(
            strategy.select_with_weight(strategy.total_weight()),
            Some(2)
        );
    }

    #[test]
    fn test_weighted_round_robin_power_of_two() {
        let strategy = WeightedRoundRobin::new(64);

        // Powers of 2 should work efficiently with modulo
        for i in 0..128 {
            assert_eq!(
                strategy.select_with_weight(strategy.total_weight()),
                Some(i % 64)
            );
        }
    }

    #[test]
    fn test_weighted_round_robin_max_usize_weight() {
        // This is an edge case - extremely large weight
        let strategy = WeightedRoundRobin::new(usize::MAX);

        // First few selections should work normally
        assert_eq!(
            strategy.select_with_weight(strategy.total_weight()),
            Some(0)
        );
        assert_eq!(
            strategy.select_with_weight(strategy.total_weight()),
            Some(1)
        );
        assert_eq!(
            strategy.select_with_weight(strategy.total_weight()),
            Some(2)
        );
    }
}
