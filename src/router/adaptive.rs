//! Adaptive Weighted Routing (AWR) strategy
//!
//! Selects backends based on multiple weighted factors:
//! - Connection pool availability (40%)
//! - Current load / pending requests (30%)
//! - Pool saturation penalty (30%)
//!
//! Lower scores are better. Heavily penalizes saturated pools.

use std::sync::atomic::Ordering;
use tracing::debug;

use super::BackendInfo;
use crate::pool::ConnectionProvider;

/// Adaptive weighted routing strategy
#[derive(Debug)]
pub struct AdaptiveStrategy {
    // No state needed - scoring is stateless based on current backend metrics
}

impl AdaptiveStrategy {
    /// Create a new adaptive strategy
    pub fn new() -> Self {
        Self {}
    }

    /// Select backend with lowest adaptive score
    ///
    /// # Scoring Algorithm
    ///
    /// For each backend, calculates weighted score:
    ///
    /// 1. **Connection Availability (40% weight)**:
    ///    - `1.0 - (available / max_size)`
    ///    - Lower when more connections available
    ///
    /// 2. **Current Load (30% weight)**:
    ///    - `pending / max_size`
    ///    - Lower when fewer pending requests
    ///
    /// 3. **Saturation Penalty (30% weight)**:
    ///    - `(used / max_size)^2` - quadratic penalty
    ///    - Heavily penalizes near-saturated pools
    ///
    /// # Returns
    /// Backend with lowest score (best fit), or None if no backends available
    pub(crate) fn select<'a>(&self, backends: &'a [BackendInfo]) -> Option<&'a BackendInfo> {
        if backends.is_empty() {
            return None;
        }

        let mut best_backend: Option<&BackendInfo> = None;
        let mut best_score = f64::INFINITY;

        for backend in backends {
            let score = self.calculate_score(backend);

            debug!(
                "Backend {:?} ({}) adaptive score: {:.3} (lower=better)",
                backend.id, backend.name, score
            );

            if score < best_score {
                best_score = score;
                best_backend = Some(backend);
            }
        }

        if let Some(selected) = best_backend {
            debug!(
                "Adaptive routing selected backend {:?} ({}) with score {:.3}",
                selected.id, selected.name, best_score
            );
        }

        best_backend
    }

    /// Calculate adaptive weighted score for a backend
    ///
    /// # Returns
    /// Score where **lower is better**. Typical range: 0.0 (idle) to 3.0+ (saturated)
    fn calculate_score(&self, backend: &BackendInfo) -> f64 {
        const AVAILABILITY_WEIGHT: f64 = 0.40;
        const LOAD_WEIGHT: f64 = 0.30;
        const SATURATION_WEIGHT: f64 = 0.30;

        let status = backend.provider.status();
        let max_size = status.max_size.get() as f64;
        let available = status.available.get() as f64;
        let pending = backend.pending_count.load(Ordering::Relaxed) as f64;
        let used = max_size - available;

        // Prevent division by zero
        if max_size == 0.0 {
            return f64::INFINITY;
        }

        // 1. Connection availability score (lower when more available)
        let availability_score = 1.0 - (available / max_size);

        // 2. Current load score (pending requests normalized)
        let load_score = pending / max_size;

        // 3. Saturation penalty (quadratic to heavily penalize near-full pools)
        let saturation_ratio = used / max_size;
        let saturation_score = saturation_ratio * saturation_ratio;

        // Weighted sum
        (availability_score * AVAILABILITY_WEIGHT)
            + (load_score * LOAD_WEIGHT)
            + (saturation_score * SATURATION_WEIGHT)
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

    fn create_test_backend(id: usize, name: &str, max_connections: usize) -> BackendInfo {
        BackendInfo {
            id: BackendId::from_index(id),
            name: ServerName::new(name.to_string()).unwrap(),
            provider: DeadpoolConnectionProvider::new(
                "localhost".to_string(),
                119,
                name.to_string(),
                max_connections,
                None,
                None,
            ),
            pending_count: Arc::new(AtomicUsize::new(0)),
            stateful_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[test]
    fn test_adaptive_empty() {
        let strategy = AdaptiveStrategy::new();
        let backends = vec![];
        assert!(strategy.select(&backends).is_none());
    }

    #[test]
    fn test_adaptive_prefers_available_connections() {
        let strategy = AdaptiveStrategy::new();

        // Backend 0: Mostly idle
        let backend0 = create_test_backend(0, "idle", 10);

        // Backend 1: More saturated (simulated by having higher pending)
        let backend1 = create_test_backend(1, "busy", 10);
        backend1.pending_count.store(8, Ordering::Relaxed);

        let backends = vec![backend0, backend1];

        // Should prefer backend 0 (idle)
        let selected = strategy.select(&backends).unwrap();
        assert_eq!(selected.id, BackendId::from_index(0));
    }

    #[test]
    fn test_adaptive_score_calculation() {
        let strategy = AdaptiveStrategy::new();
        let backend = create_test_backend(0, "test", 10);

        // Idle backend should have low score
        let score = strategy.calculate_score(&backend);
        assert!(
            score < 1.0,
            "Idle backend should have score < 1.0, got {}",
            score
        );

        // Add load and recalculate
        backend.pending_count.store(5, Ordering::Relaxed);
        let loaded_score = strategy.calculate_score(&backend);
        assert!(
            loaded_score > score,
            "Loaded backend should have higher score"
        );
    }
}
