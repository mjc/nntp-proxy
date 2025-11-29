//! Adaptive weighted routing strategy
//!
//! Selects backends based on weighted scoring of:
//! - Connection pool availability (40%)
//! - Current pending load (30%)
//! - Pool saturation (30%)

use std::cell::RefCell;
use std::sync::atomic::Ordering;
use tracing::debug;

use crate::pool::ConnectionProvider;
use crate::router::BackendInfo;

thread_local! {
    static RNG_STATE: RefCell<u64> = RefCell::new({
        use std::time::SystemTime;
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    });
}

/// Adaptive weighted routing strategy
#[derive(Debug)]
pub struct AdaptiveStrategy {
    // No state needed - scoring is based on current backend metrics
}

impl AdaptiveStrategy {
    /// Create a new adaptive strategy
    pub fn new() -> Self {
        Self {}
    }

    /// Select backend using weighted random distribution
    ///
    /// Lower scores receive exponentially higher selection probability,
    /// ensuring load distribution while preferring less-loaded backends.
    pub fn select<'a>(&self, backends: &'a [BackendInfo]) -> Option<&'a BackendInfo> {
        if backends.is_empty() {
            return None;
        }

        let scores: Vec<_> = backends.iter().map(|b| self.calculate_score(b)).collect();
        let weights = scores.iter().map(score_to_weight).collect::<Vec<_>>();
        let total_weight: f64 = weights.iter().sum();

        if total_weight == 0.0 {
            return Some(&backends[0]);
        }

        let selection_point = generate_random_point() * total_weight;
        select_by_weighted_roulette(backends, &weights, &scores, selection_point)
    }

    /// Calculate backend load score (lower is better)
    ///
    /// Weighted components:
    /// - Pool saturation (40%): Inverse of available connections
    /// - Active requests (30%): Normalized pending count
    /// - Pool pressure (30%): Quadratic penalty for high utilization
    pub fn calculate_score(&self, backend: &BackendInfo) -> f64 {
        let pool_status = backend.provider.status();
        let max_conns = pool_status.max_size.get() as f64;

        if max_conns == 0.0 {
            return f64::INFINITY;
        }

        let available_conns = pool_status.available.get() as f64;
        let pending_requests =
            sanitize_pending_count(backend.pending_count.load(Ordering::Relaxed));
        let used_conns = max_conns - available_conns;

        let saturation = 1.0 - (available_conns / max_conns);
        let load = pending_requests / max_conns;
        let pressure = (used_conns / max_conns).powi(2);

        (saturation * 0.40) + (load * 0.30) + (pressure * 0.30)
    }
}

/// Convert score to selection weight (lower score = higher weight)
#[inline]
fn score_to_weight(score: &f64) -> f64 {
    (-score).exp()
}

/// Generate random point in [0, 1) using thread-local xorshift64*
///
/// Uses xorshift64* PRNG with thread-local state to ensure high-quality
/// randomness even under high-frequency calls (1M+ calls/sec).
#[inline]
fn generate_random_point() -> f64 {
    RNG_STATE.with(|state| {
        let mut s = state.borrow_mut();

        // xorshift64* algorithm
        *s ^= *s >> 12;
        *s ^= *s << 25;
        *s ^= *s >> 27;
        let result = s.wrapping_mul(0x2545F4914F6CDD1D);

        // Convert to [0, 1)
        (result >> 11) as f64 / (1u64 << 53) as f64
    })
}

/// Select backend using weighted roulette wheel selection
fn select_by_weighted_roulette<'a>(
    backends: &'a [BackendInfo],
    weights: &[f64],
    scores: &[f64],
    mut selection_point: f64,
) -> Option<&'a BackendInfo> {
    weights
        .iter()
        .enumerate()
        .find_map(|(idx, &weight)| {
            selection_point -= weight;
            if selection_point <= 0.0 {
                let backend = &backends[idx];
                let total_weight: f64 = weights.iter().sum();
                debug!(
                    "AWR selected backend {:?} ({}) - score: {:.3}, weight: {:.3}/{:.3}",
                    backend.id, backend.name, scores[idx], weight, total_weight
                );
                Some(backend)
            } else {
                None
            }
        })
        .or_else(|| backends.last())
}

/// Detect and sanitize underflowed pending counts
#[inline]
fn sanitize_pending_count(raw: usize) -> f64 {
    if raw > (usize::MAX / 2) {
        0.0 // Underflow detected
    } else {
        raw as f64
    }
}

impl Default for AdaptiveStrategy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pool::DeadpoolConnectionProvider;
    use crate::types::{BackendId, ServerName};
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    fn create_test_backend(id: usize, max_connections: usize) -> BackendInfo {
        let provider = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            119,
            format!("server{}", id),
            max_connections,
            None,
            None,
        );

        BackendInfo {
            id: BackendId::from_index(id),
            name: ServerName::new(format!("server{}", id)).unwrap(),
            provider,
            pending_count: Arc::new(AtomicUsize::new(0)),
            stateful_count: Arc::new(AtomicUsize::new(0)),
            precheck_command: crate::config::PrecheckCommand::default(),
        }
    }

    #[test]
    fn test_prefers_less_loaded_backend() {
        let strategy = AdaptiveStrategy::new();

        let backend1 = create_test_backend(0, 10);
        let backend2 = create_test_backend(1, 10);

        backend1.pending_count.store(5, Ordering::Relaxed);
        backend2.pending_count.store(2, Ordering::Relaxed);

        let backends = vec![backend1, backend2];

        // AWR is probabilistic - check distribution over many selections
        let mut backend2_selections = 0;
        for _ in 0..200 {
            let selected = strategy.select(&backends).unwrap();
            if selected.id.as_index() == 1 {
                backend2_selections += 1;
            }
        }

        // Backend 2 (less loaded) should be selected majority of the time
        // With larger sample, expect >=100/200 (50%+)
        assert!(
            backend2_selections >= 100,
            "Less loaded backend should be selected >=100/200 times, got {}",
            backend2_selections
        );
    }

    #[test]
    fn test_score_increases_with_load() {
        let strategy = AdaptiveStrategy::new();
        let backend = create_test_backend(0, 10);

        let score_idle = strategy.calculate_score(&backend);

        backend.pending_count.store(5, Ordering::Relaxed);
        let score_loaded = strategy.calculate_score(&backend);

        assert!(score_loaded > score_idle);
    }

    #[test]
    fn test_zero_max_size_returns_infinity() {
        let strategy = AdaptiveStrategy::new();
        let backend = create_test_backend(0, 0);

        let score = strategy.calculate_score(&backend);

        assert_eq!(score, f64::INFINITY);
    }
}
