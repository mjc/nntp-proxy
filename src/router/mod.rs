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
//! ```no_run
//! use nntp_proxy::router::BackendSelector;
//! use nntp_proxy::types::{BackendId, ClientId};
//! # use nntp_proxy::pool::DeadpoolConnectionProvider;
//!
//! let mut selector = BackendSelector::new();
//! # let provider = DeadpoolConnectionProvider::new(
//! #     "localhost".to_string(), 119, "test".to_string(), 10, None, None
//! # );
//! selector.add_backend(BackendId::from_index(0), "server1".to_string(), provider);
//!
//! // Route a command
//! let client_id = ClientId::new();
//! let backend_id = selector.route_command_sync(client_id, "LIST").unwrap();
//!
//! // After command completes
//! selector.complete_command_sync(backend_id);
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
    /// Number of connections in stateful mode (for hybrid routing reservation)
    stateful_count: Arc<AtomicUsize>,
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
            stateful_count: Arc::new(AtomicUsize::new(0)),
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

    /// Try to acquire a stateful connection slot for hybrid mode
    /// Returns true if acquisition succeeded (within max_connections-1 limit)
    /// Returns false if all stateful slots are taken (need to keep 1 for PCR)
    pub fn try_acquire_stateful(&self, backend_id: BackendId) -> bool {
        if let Some(backend) = self.backends.iter().find(|b| b.id == backend_id) {
            // Get max connections from the provider's pool
            let max_connections = backend.provider.max_size();

            // Reserve 1 connection for per-command routing
            let max_stateful = max_connections.saturating_sub(1);

            // Try to increment if we haven't hit the limit
            let mut current = backend.stateful_count.load(Ordering::Acquire);
            loop {
                if current >= max_stateful {
                    debug!(
                        "Backend {:?} ({}) stateful limit reached: {}/{}",
                        backend_id, backend.name, current, max_stateful
                    );
                    return false;
                }

                match backend.stateful_count.compare_exchange_weak(
                    current,
                    current + 1,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        debug!(
                            "Backend {:?} ({}) acquired stateful slot: {}/{}",
                            backend_id,
                            backend.name,
                            current + 1,
                            max_stateful
                        );
                        return true;
                    }
                    Err(actual) => current = actual,
                }
            }
        }
        false
    }

    /// Release a stateful connection slot
    pub fn release_stateful(&self, backend_id: BackendId) {
        if let Some(backend) = self.backends.iter().find(|b| b.id == backend_id) {
            // Atomically decrement if greater than zero, avoiding underflow and spurious logs
            let result = backend.stateful_count.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                if current == 0 {
                    None
                } else {
                    Some(current - 1)
                }
            });
            match result {
                Ok(prev) => {
                    debug!(
                        "Backend {:?} ({}) released stateful slot: {}/{}",
                        backend_id,
                        backend.name,
                        prev - 1,
                        backend.provider.max_size().saturating_sub(1)
                    );
                }
                Err(0) => {
                    debug!(
                        "Backend {:?} ({}) release_stateful called when count already 0",
                        backend_id,
                        backend.name
                    );
                }
                Err(other) => unreachable!("Unexpected error in fetch_update: got Err({other}), expected only Err(0)")
            }
        }
    }

    /// Get the number of stateful connections for a backend
    #[must_use]
    pub fn stateful_count(&self, backend_id: BackendId) -> Option<usize> {
        self.backends
            .iter()
            .find(|b| b.id == backend_id)
            .map(|b| b.stateful_count.load(Ordering::Relaxed))
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
        let backend1 = router.route_command_sync(client_id, "LIST\r\n").unwrap();
        let backend2 = router.route_command_sync(client_id, "DATE\r\n").unwrap();
        let backend3 = router.route_command_sync(client_id, "HELP\r\n").unwrap();
        let backend4 = router.route_command_sync(client_id, "LIST\r\n").unwrap();
        let backend5 = router.route_command_sync(client_id, "DATE\r\n").unwrap();
        let backend6 = router.route_command_sync(client_id, "HELP\r\n").unwrap();

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
        router.route_command_sync(client_id, "LIST\r\n").unwrap();
        assert_eq!(router.backend_load(backend_id), Some(1));

        // Route another
        router.route_command_sync(client_id, "DATE\r\n").unwrap();
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
            let backend_id = router.route_command_sync(client_id, "LIST\r\n").unwrap();
            backend_counts[backend_id.as_index()] += 1;
        }

        // Each backend should get 3 commands (perfect round-robin)
        assert_eq!(backend_counts, vec![3, 3, 3]);
    }

    #[test]
    fn test_stateful_connection_reservation() {
        let mut router = BackendSelector::new();
        let backend_id = BackendId::from_index(0);
        
        // Create test provider with max_connections = 3 (so max stateful = 2)
        let provider = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            9999,
            "test".to_string(),
            3, // max_connections = 3, so max stateful = 2
            None,
            None,
        );
        
        router.add_backend(
            backend_id,
            "test-backend".to_string(),
            provider,
        );

        // Should be able to acquire 2 stateful connections
        assert!(router.try_acquire_stateful(backend_id));
        assert_eq!(router.stateful_count(backend_id), Some(1));
        
        assert!(router.try_acquire_stateful(backend_id));
        assert_eq!(router.stateful_count(backend_id), Some(2));
        
        // Third attempt should fail (max_connections - 1 = 2)
        assert!(!router.try_acquire_stateful(backend_id));
        assert_eq!(router.stateful_count(backend_id), Some(2));
        
        // Release one connection and try again
        router.release_stateful(backend_id);
        assert_eq!(router.stateful_count(backend_id), Some(1));
        assert!(router.try_acquire_stateful(backend_id));
        assert_eq!(router.stateful_count(backend_id), Some(2));
    }

    #[test]
    fn test_stateful_connection_concurrent_access() {
        let mut router = BackendSelector::new();
        let backend_id = BackendId::from_index(0);
        
        router.add_backend(
            backend_id,
            "test-backend".to_string(),
            create_test_provider(),
        );

        // Simulate concurrent access with multiple threads
        let router_arc = Arc::new(router);
        let handles: Vec<_> = (0..10)
            .map(|_| {
                let router_clone = Arc::clone(&router_arc);
                std::thread::spawn(move || {
                    // Try to acquire and immediately release
                    if router_clone.try_acquire_stateful(backend_id) {
                        // Simulate some work
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        router_clone.release_stateful(backend_id);
                        1
                    } else {
                        0
                    }
                })
            })
            .collect();

        let total_acquired: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        
        // At least some threads should have succeeded
        assert!(total_acquired > 0);
        // Final count should be 0 (all released)
        assert_eq!(router_arc.stateful_count(backend_id), Some(0));
    }

    #[test]
    fn test_stateful_connection_multiple_backends() {
        let mut router = BackendSelector::new();
        
        // Add multiple backends
        let backend1 = BackendId::from_index(0);
        let backend2 = BackendId::from_index(1);
        
        // Create test providers with max_connections = 3 (so max stateful = 2 each)
        let provider1 = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            9999,
            "test-1".to_string(),
            3, // max_connections = 3, so max stateful = 2
            None,
            None,
        );
        let provider2 = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            9998,
            "test-2".to_string(),
            3, // max_connections = 3, so max stateful = 2
            None,
            None,
        );
        
        router.add_backend(backend1, "backend-1".to_string(), provider1);
        router.add_backend(backend2, "backend-2".to_string(), provider2);

        // Each backend should have independent stateful counters
        assert!(router.try_acquire_stateful(backend1));
        assert!(router.try_acquire_stateful(backend2));
        
        assert_eq!(router.stateful_count(backend1), Some(1));
        assert_eq!(router.stateful_count(backend2), Some(1));
        
        // Should be able to max out both backends independently
        assert!(router.try_acquire_stateful(backend1)); // backend1 now at 2/2
        assert!(router.try_acquire_stateful(backend2)); // backend2 now at 2/2
        
        assert!(!router.try_acquire_stateful(backend1)); // Should fail
        assert!(!router.try_acquire_stateful(backend2)); // Should fail
        
        // Release from backend1, should not affect backend2
        router.release_stateful(backend1);
        assert_eq!(router.stateful_count(backend1), Some(1));
        assert_eq!(router.stateful_count(backend2), Some(2));
        
        assert!(router.try_acquire_stateful(backend1)); // Should work
        assert!(!router.try_acquire_stateful(backend2)); // Should still fail
    }

    #[test]
    fn test_stateful_connection_invalid_backend() {
        let router = BackendSelector::new();
        let invalid_backend = BackendId::from_index(999);
        
        // Operations on non-existent backend should handle gracefully
        assert!(!router.try_acquire_stateful(invalid_backend));
        assert_eq!(router.stateful_count(invalid_backend), None);
        
        // Release on non-existent backend should not panic
        router.release_stateful(invalid_backend);
    }

    #[test]
    fn test_stateful_reservation_edge_cases() {
        let mut router = BackendSelector::new();
        let backend_id = BackendId::from_index(0);
        
        let provider = DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            9999,
            "test".to_string(),
            3, // max_connections = 3, so max stateful = 2
            None,
            None,
        );
        
        router.add_backend(
            backend_id,
            "test-backend".to_string(),
            provider,
        );

        // Test multiple releases (should not go negative)
        router.release_stateful(backend_id);
        router.release_stateful(backend_id);
        router.release_stateful(backend_id);
        
        // Count should stay at 0
        assert_eq!(router.stateful_count(backend_id), Some(0));
        
        // Should still be able to acquire after over-releasing
        assert!(router.try_acquire_stateful(backend_id));
        assert_eq!(router.stateful_count(backend_id), Some(1));
    }
}
