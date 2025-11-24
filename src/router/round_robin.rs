//! Round-robin backend selection strategy
//!
//! Distributes requests evenly across all backends using atomic counter.
//! Simple, predictable, and lock-free.

use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::debug;

use super::BackendInfo;

/// Round-robin state for backend selection
#[derive(Debug)]
pub struct RoundRobinStrategy {
    /// Current position in round-robin rotation (atomic for lock-free access)
    current: AtomicUsize,
}

impl RoundRobinStrategy {
    /// Create a new round-robin strategy
    pub fn new() -> Self {
        Self {
            current: AtomicUsize::new(0),
        }
    }

    /// Select next backend using round-robin
    ///
    /// # Algorithm
    /// - Atomically increments counter
    /// - Takes modulo backend count for wraparound
    /// - O(1) complexity, lock-free
    ///
    /// # Returns
    /// Index of selected backend, or None if no backends available
    pub(crate) fn select<'a>(&self, backends: &'a [BackendInfo]) -> Option<&'a BackendInfo> {
        if backends.is_empty() {
            return None;
        }

        let index = self.current.fetch_add(1, Ordering::Relaxed) % backends.len();
        let backend = &backends[index];

        debug!(
            "Round-robin selected backend {:?} ({}) at index {}",
            backend.id, backend.name, index
        );

        Some(backend)
    }
}

impl Default for RoundRobinStrategy {
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

    fn create_test_backend(id: usize, name: &str) -> BackendInfo {
        BackendInfo {
            id: BackendId::from_index(id),
            name: ServerName::new(name.to_string()).unwrap(),
            provider: DeadpoolConnectionProvider::new(
                "localhost".to_string(),
                119,
                name.to_string(),
                10,
                None,
                None,
            ),
            pending_count: Arc::new(AtomicUsize::new(0)),
            stateful_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    #[test]
    fn test_round_robin_empty() {
        let strategy = RoundRobinStrategy::new();
        let backends = vec![];
        assert!(strategy.select(&backends).is_none());
    }

    #[test]
    fn test_round_robin_single_backend() {
        let strategy = RoundRobinStrategy::new();
        let backends = vec![create_test_backend(0, "server1")];

        let b1 = strategy.select(&backends).unwrap();
        let b2 = strategy.select(&backends).unwrap();
        let b3 = strategy.select(&backends).unwrap();

        assert_eq!(b1.id, BackendId::from_index(0));
        assert_eq!(b2.id, BackendId::from_index(0));
        assert_eq!(b3.id, BackendId::from_index(0));
    }

    #[test]
    fn test_round_robin_distribution() {
        let strategy = RoundRobinStrategy::new();
        let backends = vec![
            create_test_backend(0, "server1"),
            create_test_backend(1, "server2"),
            create_test_backend(2, "server3"),
        ];

        let selections: Vec<_> = (0..6)
            .map(|_| strategy.select(&backends).unwrap().id)
            .collect();

        // Should cycle through: 0, 1, 2, 0, 1, 2
        assert_eq!(selections[0], BackendId::from_index(0));
        assert_eq!(selections[1], BackendId::from_index(1));
        assert_eq!(selections[2], BackendId::from_index(2));
        assert_eq!(selections[3], BackendId::from_index(0));
        assert_eq!(selections[4], BackendId::from_index(1));
        assert_eq!(selections[5], BackendId::from_index(2));
    }
}
