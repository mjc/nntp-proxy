//! Backend server selection and load balancing
//!
//! This module handles selecting backend servers using round-robin
//! with simple load tracking for monitoring.
//!
//! # Overview
//!
//! The `BackendSelector` provides thread-safe backend selection for routing
//! NNTP commands across multiple backend servers. It uses a lock-free
//! round-robin algorithm with atomic operations for concurrent access.
//!
//! # Usage
//!
//! ```
//! # use anyhow::Result;
//! use nntp_proxy::router::BackendSelector;
//! use nntp_proxy::types::{BackendId, ClientId};
//! # use nntp_proxy::pool::DeadpoolConnectionProvider;
//!
//! # fn main() -> Result<()> {
//! let mut selector = BackendSelector::new();
//! # let provider = DeadpoolConnectionProvider::new(
//! #     "localhost".to_string(), 119, "test".to_string(), 10, None, None
//! # );
//! selector.add_backend(BackendId::from_index(0), "server1".to_string(), provider);
//!
//! // Route a command
//! let client_id = ClientId::new();
//! let backend_id = selector.route_command_sync(client_id, "LIST")?;
//!
//! // After command completes
//! selector.complete_command_sync(backend_id);
//! # Ok(())
//! # }
//! ```

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{debug, info};

use crate::pool::DeadpoolConnectionProvider;
use crate::types::{BackendId, ClientId};

/// Backend connection information
#[derive(Debug, Clone)]
struct BackendInfo {
    /// Backend identifier
    id: BackendId,
    /// Server name for logging
    name: String,
    /// Connection provider for this backend
    provider: DeadpoolConnectionProvider,
    /// Number of pending requests on this backend (for load balancing)
    pending_count: Arc<AtomicUsize>,
}

/// Selects backend servers using round-robin with load tracking
///
/// # Thread Safety
///
/// This struct is designed for concurrent access across multiple threads.
/// The round-robin counter and pending counts use atomic operations for
/// lock-free performance.
///
/// # Load Balancing
///
/// - **Strategy**: Round-robin rotation through available backends
/// - **Tracking**: Atomic counters track pending commands per backend
/// - **Monitoring**: Load statistics available via `backend_load()`
///
/// # Examples
///
/// ```no_run
/// # use nntp_proxy::router::BackendSelector;
/// # use nntp_proxy::types::{BackendId, ClientId};
/// # use nntp_proxy::pool::DeadpoolConnectionProvider;
/// let mut selector = BackendSelector::new();
///
/// # let provider = DeadpoolConnectionProvider::new(
/// #     "localhost".to_string(), 119, "test".to_string(), 10, None, None
/// # );
/// selector.add_backend(
///     BackendId::from_index(0),
///     "backend-1".to_string(),
///     provider,
/// );
///
/// // Route commands
/// let backend = selector.route_command_sync(ClientId::new(), "LIST")?;
/// # Ok::<(), anyhow::Error>(())
/// ```
#[derive(Debug)]
pub struct BackendSelector {
    /// Backend connection providers
    backends: Vec<BackendInfo>,
    /// Current backend index for round-robin selection
    current_backend: AtomicUsize,
}

impl Default for BackendSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl BackendSelector {
    /// Create a new backend selector
    #[must_use]
    pub fn new() -> Self {
        Self {
            // Pre-allocate for typical number of backend servers (most setups have 2-8)
            backends: Vec::with_capacity(4),
            current_backend: AtomicUsize::new(0),
        }
    }

    /// Add a backend server to the router
    pub fn add_backend(
        &mut self,
        backend_id: BackendId,
        name: String,
        provider: DeadpoolConnectionProvider,
    ) {
        info!("Added backend {:?} ({})", backend_id, name);
        self.backends.push(BackendInfo {
            id: backend_id,
            name,
            provider,
            pending_count: Arc::new(AtomicUsize::new(0)),
        });
    }

    /// Select the next backend using round-robin strategy
    fn select_backend(&self) -> Option<&BackendInfo> {
        if self.backends.is_empty() {
            return None;
        }

        let index = self.current_backend.fetch_add(1, Ordering::Relaxed) % self.backends.len();
        Some(&self.backends[index])
    }

    /// Select a backend for the given command using round-robin
    /// Returns the backend ID to use for this command
    pub fn route_command_sync(&self, _client_id: ClientId, _command: &str) -> Result<BackendId> {
        let backend = self.select_backend().ok_or_else(|| {
            anyhow::anyhow!(
                "No backends available for routing (total backends: {})",
                self.backends.len()
            )
        })?;

        // Increment pending count for load tracking
        backend.pending_count.fetch_add(1, Ordering::Relaxed);

        debug!(
            "Selected backend {:?} ({}) for command",
            backend.id, backend.name
        );

        Ok(backend.id)
    }

    /// Mark a command as complete, decrementing the pending count
    pub fn complete_command_sync(&self, backend_id: BackendId) {
        if let Some(backend) = self.backends.iter().find(|b| b.id == backend_id) {
            backend.pending_count.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Get the connection provider for a backend
    #[must_use]
    pub fn get_backend_provider(
        &self,
        backend_id: BackendId,
    ) -> Option<&DeadpoolConnectionProvider> {
        self.backends
            .iter()
            .find(|b| b.id == backend_id)
            .map(|b| &b.provider)
    }

    /// Get the number of backends
    #[must_use]
    #[inline]
    pub fn backend_count(&self) -> usize {
        self.backends.len()
    }

    /// Get backend load (pending requests) for monitoring
    #[must_use]
    pub fn backend_load(&self, backend_id: BackendId) -> Option<usize> {
        self.backends
            .iter()
            .find(|b| b.id == backend_id)
            .map(|b| b.pending_count.load(Ordering::Relaxed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_provider() -> DeadpoolConnectionProvider {
        DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            9999,
            "test".to_string(),
            2,
            None,
            None,
        )
    }

    #[test]
    fn test_router_creation() {
        let router = BackendSelector::new();
        assert_eq!(router.backend_count(), 0);
    }

    #[test]
    fn test_add_backend() {
        let mut router = BackendSelector::new();
        let backend_id = BackendId::from_index(0);
        let provider = create_test_provider();

        router.add_backend(backend_id, "test-backend".to_string(), provider);

        assert_eq!(router.backend_count(), 1);
    }

    #[test]
    fn test_add_multiple_backends() {
        let mut router = BackendSelector::new();

        for i in 0..3 {
            let backend_id = BackendId::from_index(i);
            let provider = create_test_provider();
            router.add_backend(backend_id, format!("backend-{}", i), provider);
        }

        assert_eq!(router.backend_count(), 3);
    }

    #[test]
    fn test_no_backends_fails() {
        let router = BackendSelector::new();
        let client_id = ClientId::new();
        let result = router.route_command_sync(client_id, "LIST\r\n");

        assert!(result.is_err());
    }

    #[test]
    fn test_round_robin_selection() {
        let mut router = BackendSelector::new();
        let client_id = ClientId::new();

        // Add 3 backends
        for i in 0..3 {
            let backend_id = BackendId::from_index(i);
            let provider = create_test_provider();
            router.add_backend(backend_id, format!("backend-{}", i), provider);
        }

        // Route 6 commands and verify round-robin
        let backend1 = router
            .route_command_sync(client_id, "LIST\r\n")
            .expect("Failed to route command");
        let backend2 = router
            .route_command_sync(client_id, "DATE\r\n")
            .expect("Failed to route command");
        let backend3 = router
            .route_command_sync(client_id, "HELP\r\n")
            .expect("Failed to route command");
        let backend4 = router
            .route_command_sync(client_id, "LIST\r\n")
            .expect("Failed to route command");
        let backend5 = router
            .route_command_sync(client_id, "DATE\r\n")
            .expect("Failed to route command");
        let backend6 = router
            .route_command_sync(client_id, "HELP\r\n")
            .expect("Failed to route command");

        // Should cycle through backends in order
        assert_eq!(backend1.as_index(), 0);
        assert_eq!(backend2.as_index(), 1);
        assert_eq!(backend3.as_index(), 2);
        assert_eq!(backend4.as_index(), 0); // Wraps around
        assert_eq!(backend5.as_index(), 1);
        assert_eq!(backend6.as_index(), 2);
    }

    #[test]
    fn test_backend_load_tracking() {
        let mut router = BackendSelector::new();
        let client_id = ClientId::new();
        let backend_id = BackendId::from_index(0);
        let provider = create_test_provider();

        router.add_backend(backend_id, "test".to_string(), provider);

        // Initially no load
        assert_eq!(router.backend_load(backend_id), Some(0));

        // Route a command
        router
            .route_command_sync(client_id, "LIST\r\n")
            .expect("Failed to route command");
        assert_eq!(router.backend_load(backend_id), Some(1));

        // Route another
        router
            .route_command_sync(client_id, "DATE\r\n")
            .expect("Failed to route command");
        assert_eq!(router.backend_load(backend_id), Some(2));

        // Complete one
        router.complete_command_sync(backend_id);
        assert_eq!(router.backend_load(backend_id), Some(1));

        // Complete the other
        router.complete_command_sync(backend_id);
        assert_eq!(router.backend_load(backend_id), Some(0));
    }

    #[test]
    fn test_get_backend_provider() {
        let mut router = BackendSelector::new();
        let backend_id = BackendId::from_index(0);
        let provider = create_test_provider();

        router.add_backend(backend_id, "test".to_string(), provider);

        let retrieved = router.get_backend_provider(backend_id);
        assert!(retrieved.is_some());

        let fake_id = BackendId::from_index(999);
        assert!(router.get_backend_provider(fake_id).is_none());
    }

    #[test]
    fn test_load_balancing_fairness() {
        let mut router = BackendSelector::new();
        let client_id = ClientId::new();

        // Add 3 backends
        for i in 0..3 {
            router.add_backend(
                BackendId::from_index(i),
                format!("backend-{}", i),
                create_test_provider(),
            );
        }

        // Route 9 commands
        let mut backend_counts = vec![0, 0, 0];
        for _ in 0..9 {
            let backend_id = router
                .route_command_sync(client_id, "LIST\r\n")
                .expect("Failed to route command");
            backend_counts[backend_id.as_index()] += 1;
        }

        // Each backend should get 3 commands (perfect round-robin)
        assert_eq!(backend_counts, vec![3, 3, 3]);
    }
}
