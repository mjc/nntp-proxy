//! Adaptive weighted routing strategy
//!
//! Selects backends based on weighted scoring of:
//! - Connection pool availability (40%)
//! - Current pending load (30%)
//! - Pool saturation (30%)

use std::sync::atomic::Ordering;
use tracing::debug;

use crate::pool::ConnectionProvider;
use crate::router::BackendInfo;

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

    /// Select backend with lowest adaptive score
    pub(crate) fn select<'a>(&self, backends: &'a [BackendInfo]) -> Option<&'a BackendInfo> {
        backends
            .iter()
            .min_by(|a, b| {
                let score_a = self.calculate_score(a);
                let score_b = self.calculate_score(b);
                score_a
                    .partial_cmp(&score_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .inspect(|backend| {
                let score = self.calculate_score(backend);
                debug!(
                    "Adaptive selected backend {:?} ({}) with score {:.3}",
                    backend.id, backend.name, score
                );
            })
    }

    /// Calculate adaptive weighted score for a backend (lower is better)
    pub(crate) fn calculate_score(&self, backend: &BackendInfo) -> f64 {
        let status = backend.provider.status();
        let max_size = status.max_size.get() as f64;
        let available = status.available.get() as f64;
        let pending_raw = backend.pending_count.load(Ordering::Relaxed);

        // Detect underflow (pending > half of usize::MAX is impossible)
        let pending = if pending_raw > (usize::MAX / 2) {
            0.0
        } else {
            pending_raw as f64
        };

        // Prevent division by zero
        if max_size == 0.0 {
            return f64::INFINITY;
        }

        let used = max_size - available;

        // Weighted scoring components:
        // 1. Availability (40%) - inverse of available connections
        let availability_score = 1.0 - (available / max_size);

        // 2. Load (30%) - pending requests normalized
        let load_score = pending / max_size;

        // 3. Saturation (30%) - quadratic penalty for pool usage
        let saturation_ratio = used / max_size;
        let saturation_score = saturation_ratio * saturation_ratio;

        (availability_score * 0.40) + (load_score * 0.30) + (saturation_score * 0.30)
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
        }
    }

    #[test]
    fn test_selects_least_loaded_backend() {
        let strategy = AdaptiveStrategy::new();

        let backend1 = create_test_backend(0, 10);
        let backend2 = create_test_backend(1, 10);

        backend1.pending_count.store(5, Ordering::Relaxed);
        backend2.pending_count.store(2, Ordering::Relaxed);

        let backends = vec![backend1, backend2];
        let selected = strategy.select(&backends).unwrap();

        assert_eq!(selected.id.as_index(), 1);
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
