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
//! use nntp_proxy::types::{BackendId, ClientId, ServerName};
//! # use nntp_proxy::pool::DeadpoolConnectionProvider;
//!
//! let mut selector = BackendSelector::new();
//! # let provider = DeadpoolConnectionProvider::new(
//! #     "localhost".to_string(), 119, "test".to_string(), 10, None, None
//! # );
//! selector.add_backend(
//!     BackendId::from_index(0),
//!     ServerName::new("server1".to_string()).unwrap(),
//!     provider,
//! );
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
use crate::types::{BackendId, ClientId, ServerName};

/// Backend connection information
#[derive(Debug, Clone)]
struct BackendInfo {
    /// Backend identifier
    id: BackendId,
    /// Server name for logging
    name: ServerName,
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
/// # use nntp_proxy::types::{BackendId, ClientId, ServerName};
/// # use nntp_proxy::pool::DeadpoolConnectionProvider;
/// let mut selector = BackendSelector::new();
///
/// # let provider = DeadpoolConnectionProvider::new(
/// #     "localhost".to_string(), 119, "test".to_string(), 10, None, None
/// # );
/// selector.add_backend(
///     BackendId::from_index(0),
///     ServerName::new("backend-1".to_string()).unwrap(),
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
        name: ServerName,
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
            let result = backend.stateful_count.fetch_update(
                Ordering::AcqRel,
                Ordering::Acquire,
                |current| {
                    if current == 0 {
                        None
                    } else {
                        Some(current - 1)
                    }
                },
            );
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
                        backend_id, backend.name
                    );
                }
                Err(other) => unreachable!(
                    "Unexpected error in fetch_update: got Err({other}), expected only Err(0)"
                ),
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
mod tests;
