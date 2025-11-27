//! Round-robin backend selection strategy

use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::debug;

use crate::router::BackendInfo;

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
    use std::sync::atomic::AtomicUsize;

    fn create_test_backend(id: usize) -> BackendInfo {
        let provider = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            119,
            format!("server{}", id),
            10,
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
    fn test_round_robin_rotation() {
        let strategy = RoundRobinStrategy::new();
        let backends = vec![
            create_test_backend(0),
            create_test_backend(1),
            create_test_backend(2),
        ];

        let selected1 = strategy.select(&backends).unwrap();
        let selected2 = strategy.select(&backends).unwrap();
        let selected3 = strategy.select(&backends).unwrap();
        let selected4 = strategy.select(&backends).unwrap();

        assert_eq!(selected1.id.as_index(), 0);
        assert_eq!(selected2.id.as_index(), 1);
        assert_eq!(selected3.id.as_index(), 2);
        assert_eq!(selected4.id.as_index(), 0); // Wraps around
    }

    #[test]
    fn test_empty_backends() {
        let strategy = RoundRobinStrategy::new();
        let backends: Vec<BackendInfo> = vec![];

        assert!(strategy.select(&backends).is_none());
    }
}
