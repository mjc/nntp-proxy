//! Backend server selection and load balancing
//!
//! Provides two routing strategies:
//! - **Round-robin**: Even distribution across backends
//! - **Adaptive**: Weighted selection based on load, availability, and saturation
//!
//! # Overview
//!
//! The `BackendSelector` provides thread-safe backend selection for routing
//! NNTP commands across multiple backend servers.
//!
//! # Usage
//!
//! ```no_run
//! use nntp_proxy::router::BackendSelector;
//! use nntp_proxy::types::{BackendId, ClientId, ServerName};
//! # use nntp_proxy::pool::DeadpoolConnectionProvider;
//!
//! let mut selector = BackendSelector::default();
//! # let provider = DeadpoolConnectionProvider::new(
//! #     "localhost".to_string(), 119, "test".to_string(), 10, None, None
//! # );
//! selector.add_backend(
//!     BackendId::from_index(0),
//!     ServerName::new("server1".to_string()).unwrap(),
//!     provider,
//!     nntp_proxy::config::PrecheckCommand::default(),
//! );
//!
//! // Route a command
//! let client_id = ClientId::new();
//! let backend_id = selector.route_command(client_id, "LIST").unwrap();
//!
//! // After command completes
//! selector.complete_command(backend_id);
//! ```

pub mod strategy;

pub use strategy::{AdaptiveStrategy, RoundRobinStrategy};

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{debug, info};

use crate::config::RoutingStrategy;
use crate::pool::{ConnectionProvider, DeadpoolConnectionProvider};
use crate::types::{BackendId, ClientId, ServerName};

/// Backend connection information
#[derive(Debug, Clone)]
pub struct BackendInfo {
    /// Backend identifier
    pub id: BackendId,
    /// Server name for logging
    pub name: ServerName,
    /// Connection provider for this backend
    pub provider: DeadpoolConnectionProvider,
    /// Precheck command to use for this backend
    pub precheck_command: crate::config::PrecheckCommand,
    /// Number of pending requests on this backend (for load balancing)
    pub pending_count: Arc<AtomicUsize>,
    /// Number of connections in stateful mode (for hybrid routing reservation)
    pub stateful_count: Arc<AtomicUsize>,
}

/// RAII guard for pending count management
///
/// Automatically increments pending count on creation and decrements on drop.
/// This prevents manual tracking errors and ensures cleanup even on early returns or panics.
///
/// # Example
/// ```no_run
/// # use nntp_proxy::router::BackendSelector;
/// # use nntp_proxy::types::BackendId;
/// # let selector = BackendSelector::default();
/// # let backend_id = BackendId::from_index(0);
/// let _guard = selector.pending_guard(backend_id);
/// // pending count incremented
///
/// // ... do work ...
///
/// // pending count auto-decremented when guard drops
/// ```
pub struct PendingGuard<'a> {
    pub(self) selector: &'a BackendSelector,
    pub(self) backend_id: BackendId,
    pub(self) skip_decrement: bool,
}

impl<'a> PendingGuard<'a> {
    /// Create a new pending guard (private - use BackendSelector::pending_guard)
    fn new(selector: &'a BackendSelector, backend_id: BackendId) -> Self {
        selector.increment_pending(backend_id);
        Self {
            selector,
            backend_id,
            skip_decrement: false,
        }
    }

    /// Create guard without incrementing (for wrapping already-incremented backend)
    fn wrap_existing(selector: &'a BackendSelector, backend_id: BackendId) -> Self {
        Self {
            selector,
            backend_id,
            skip_decrement: false,
        }
    }
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if !self.skip_decrement {
            self.selector.complete_command(self.backend_id);
        }
    }
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
/// let mut selector = BackendSelector::default();
///
/// # let provider = DeadpoolConnectionProvider::new(
/// #     "localhost".to_string(), 119, "test".to_string(), 10, None, None
/// # );
/// selector.add_backend(
///     BackendId::from_index(0),
///     ServerName::new("backend-1".to_string()).unwrap(),
///     provider,
///     nntp_proxy::config::PrecheckCommand::default(),
/// );
///
/// // Route commands
/// let backend = selector.route_command(ClientId::new(), "LIST")?;
/// # Ok::<(), anyhow::Error>(())
/// ```
#[derive(Debug)]
pub struct BackendSelector {
    /// Backend connection providers
    backends: Vec<BackendInfo>,
    /// Round-robin strategy
    round_robin: RoundRobinStrategy,
    /// Adaptive weighted routing strategy
    adaptive: AdaptiveStrategy,
    /// Routing strategy to use for backend selection
    routing_strategy: RoutingStrategy,
}

impl Default for BackendSelector {
    fn default() -> Self {
        Self::new(RoutingStrategy::default())
    }
}

impl BackendSelector {
    /// Create a new backend selector with the specified routing strategy
    #[must_use]
    pub fn new(routing_strategy: RoutingStrategy) -> Self {
        info!(
            "Initializing backend selector with {} strategy",
            routing_strategy
        );
        Self {
            // Pre-allocate for typical number of backend servers (most setups have 2-8)
            backends: Vec::with_capacity(4),
            round_robin: RoundRobinStrategy::new(),
            adaptive: AdaptiveStrategy::new(),
            routing_strategy,
        }
    }

    /// Add a backend server to the router
    pub fn add_backend(
        &mut self,
        backend_id: BackendId,
        name: ServerName,
        provider: DeadpoolConnectionProvider,
        precheck_command: crate::config::PrecheckCommand,
    ) {
        info!("Added backend {:?} ({})", backend_id, name);
        self.backends.push(BackendInfo {
            id: backend_id,
            name,
            provider,
            precheck_command,
            pending_count: Arc::new(AtomicUsize::new(0)),
            stateful_count: Arc::new(AtomicUsize::new(0)),
        });
    }

    /// Select the next backend using the configured routing strategy
    fn select_backend(&self) -> Option<&BackendInfo> {
        if self.backends.is_empty() {
            return None;
        }

        match self.routing_strategy {
            RoutingStrategy::RoundRobin => self.round_robin.select(&self.backends),
            RoutingStrategy::AdaptiveWeighted => self.adaptive.select(&self.backends),
        }
    }

    /// Select a backend for the given command using the configured strategy
    /// Returns the backend ID to use for this command
    ///
    /// DEPRECATED: Use route_command_guarded() instead to ensure pending count is properly managed
    pub fn route_command(&self, _client_id: ClientId, _command: &str) -> Result<BackendId> {
        let backend = self.select_backend().ok_or_else(|| {
            anyhow::anyhow!(
                "No backends available for routing (total backends: {})",
                self.backends.len()
            )
        })?;

        // Increment pending count for load tracking
        backend.pending_count.fetch_add(1, Ordering::Relaxed);

        let pending = backend.pending_count.load(Ordering::Relaxed);
        let score = if self.routing_strategy == RoutingStrategy::AdaptiveWeighted {
            Some(self.adaptive.calculate_score(backend))
        } else {
            None
        };

        if let Some(s) = score {
            debug!(
                "Selected backend {:?} ({}) - pending: {}, AWR score: {:.3}",
                backend.id, backend.name, pending, s
            );
        } else {
            debug!(
                "Selected backend {:?} ({}) - pending: {}",
                backend.id, backend.name, pending
            );
        }

        Ok(backend.id)
    }

    /// Select a backend and return a RAII guard that manages pending count
    ///
    /// Returns (BackendId, PendingGuard) where the guard automatically decrements
    /// pending_count on drop. This prevents pending count leaks from early returns.
    ///
    /// # Example
    /// ```no_run
    /// # use nntp_proxy::router::BackendSelector;
    /// # use nntp_proxy::types::ClientId;
    /// # let selector = BackendSelector::default();
    /// # let client_id = ClientId::new();
    /// let (backend_id, _guard) = selector.route_command_guarded(client_id, "LIST")?;
    /// // Use backend_id...
    /// // _guard automatically decrements pending_count on drop
    /// # Ok::<(), anyhow::Error>(())
    /// ```
    pub fn route_command_guarded(
        &self,
        client_id: ClientId,
        command: &str,
    ) -> Result<(BackendId, PendingGuard<'_>)> {
        let backend_id = self.route_command(client_id, command)?;

        // Wrap the already-incremented backend (don't double-increment!)
        let guard = PendingGuard::wrap_existing(self, backend_id);

        Ok((backend_id, guard))
    }

    /// Mark a command as complete, decrementing the pending count
    pub fn complete_command(&self, backend_id: BackendId) {
        if let Some(backend) = self.backends.iter().find(|b| b.id == backend_id) {
            // Use fetch_update to prevent underflow
            let _ = backend.pending_count.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |current| current.checked_sub(1),
            );
        }
    }

    /// Manually increment pending count (for bypassing route_command)
    pub fn increment_pending(&self, backend_id: BackendId) {
        if let Some(backend) = self.backends.iter().find(|b| b.id == backend_id) {
            backend.pending_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Create a RAII guard that manages pending count automatically
    ///
    /// The guard increments the pending count on creation and decrements it on drop.
    /// This ensures proper cleanup even on early returns or panics.
    ///
    /// # Example
    /// ```no_run
    /// # use nntp_proxy::router::BackendSelector;
    /// # use nntp_proxy::types::BackendId;
    /// # let selector = BackendSelector::default();
    /// # let backend_id = BackendId::from_index(0);
    /// {
    ///     let _guard = selector.pending_guard(backend_id);
    ///     // Work with backend - pending count is tracked
    /// } // Guard drops, pending count decremented
    /// ```
    #[must_use]
    pub fn pending_guard(&self, backend_id: BackendId) -> PendingGuard<'_> {
        PendingGuard::new(self, backend_id)
    }

    /// Get the connection provider for a backend
    #[must_use]
    pub fn backend_provider(&self, backend_id: BackendId) -> Option<&DeadpoolConnectionProvider> {
        self.backends
            .iter()
            .find(|b| b.id == backend_id)
            .map(|b| &b.provider)
    }

    /// Get all backends with their providers and precheck commands for parallel operations
    ///
    /// Returns a vector of (BackendId, Arc<DeadpoolConnectionProvider>, PrecheckCommand) tuples
    /// for all configured backends. This is used by parallel STAT/HEAD checks to
    /// query all backends concurrently.
    #[must_use]
    pub fn all_backend_providers(
        &self,
    ) -> Vec<(
        BackendId,
        Arc<DeadpoolConnectionProvider>,
        crate::config::PrecheckCommand,
    )> {
        self.backends
            .iter()
            .map(|b| (b.id, Arc::new(b.provider.clone()), b.precheck_command))
            .collect()
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

    /// Route command with availability bitmap
    ///
    /// For AWR: Selects best backend based on load, saturation, and transfer speed
    /// For Round-Robin: Selects next backend in rotation that has the article
    ///
    /// # Arguments
    /// * `availability` - Backend availability bitmap from cache
    /// * `metrics` - Optional metrics collector for transfer performance data
    ///
    /// # Returns
    /// Backend ID, or None if no backends have the article
    #[must_use]
    pub fn route_command_with_availability(
        &self,
        availability: &crate::cache::BackendAvailability,
        metrics: Option<&crate::metrics::MetricsCollector>,
    ) -> Option<BackendId> {
        match self.routing_strategy {
            RoutingStrategy::RoundRobin => self.round_robin_with_availability(availability),
            RoutingStrategy::AdaptiveWeighted => {
                self.adaptive_with_availability(availability, metrics)
            }
        }
    }

    /// Round-robin selection filtered by availability
    fn round_robin_with_availability(
        &self,
        availability: &crate::cache::BackendAvailability,
    ) -> Option<BackendId> {
        use tracing::debug;

        // Filter backends to only those that have the article
        let available_backends: Vec<BackendInfo> = self
            .backends
            .iter()
            .filter(|b| availability.has_article(b.id))
            .cloned()
            .collect();

        if available_backends.is_empty() {
            return None;
        }

        // Use round-robin strategy on filtered list
        self.round_robin.select(&available_backends).map(|backend| {
            debug!(
                "Round-robin selected backend {:?} ({}) from {} backends with article",
                backend.id,
                backend.name,
                available_backends.len()
            );
            backend.id
        })
    }

    /// AWR selection filtered by availability
    fn adaptive_with_availability(
        &self,
        availability: &crate::cache::BackendAvailability,
        metrics: Option<&crate::metrics::MetricsCollector>,
    ) -> Option<BackendId> {
        use tracing::debug;

        // Take metrics snapshot ONCE for all backends (avoid repeated snapshot() calls)
        let metrics_snapshot = metrics.map(|m| m.snapshot());

        let mut selected_backend: Option<(BackendId, f64)> = None;

        for backend in &self.backends {
            // Skip backends that don't have the article
            if !availability.has_article(backend.id) {
                continue;
            }

            // Get pool status
            let status = backend.provider.status();
            let max_size = status.max_size.get() as f64;
            let available = status.available.get() as f64;
            let pending = backend.pending_count.load(Ordering::Relaxed) as f64;

            // Prevent division by zero
            if max_size == 0.0 {
                continue;
            }

            let used = max_size - available;

            // Get historical transfer performance if available
            let transfer_penalty = if let Some(ref snapshot) = metrics_snapshot {
                if let Some(backend_stats) = snapshot.backend_stats.get(backend.id.as_index()) {
                    // Calculate bytes/sec from backend's historical performance
                    // Higher transfer rate = lower penalty
                    let uptime_secs = snapshot.uptime.as_secs_f64().max(1.0);
                    let bytes_received = backend_stats.bytes_received.as_u64() as f64;
                    let transfer_rate = bytes_received / uptime_secs;

                    // Normalize: 0 MB/s = 1.0 penalty, 10+ MB/s = 0.0 penalty
                    let mb_per_sec = transfer_rate / 1_000_000.0;
                    1.0 - (mb_per_sec / 10.0).min(1.0)
                } else {
                    0.5 // Unknown - neutral penalty
                }
            } else {
                0.5 // No metrics - neutral penalty
            };

            // AWR scoring (lower is better):
            // - 30% connection availability
            // - 20% current load
            // - 20% saturation penalty (quadratic)
            // - 30% transfer speed penalty (prefer faster backends)
            let availability_score = 1.0 - (available / max_size);
            let load_score = pending / max_size;
            let saturation_ratio = used / max_size;
            let saturation_score = saturation_ratio * saturation_ratio;

            let score = (availability_score * 0.30)
                + (load_score * 0.20)
                + (saturation_score * 0.20)
                + (transfer_penalty * 0.30);

            debug!(
                "Backend {:?} ({}): available={}/{}, pending={}, transfer_penalty={:.3}, score={:.3} (lower=better)",
                backend.id,
                backend.name,
                available as usize,
                max_size as usize,
                pending as usize,
                transfer_penalty,
                score
            );

            // Select backend with lowest score
            if selected_backend.is_none() || score < selected_backend.unwrap().1 {
                selected_backend = Some((backend.id, score));
            }
        }

        selected_backend.map(|(id, score)| {
            debug!("AWR selected backend {:?} with score {:.3}", id, score);
            id
        })
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

    /// Get backend info for display (pending, pool status, score)
    #[must_use]
    pub fn backend_routing_info(&self, backend_id: BackendId) -> Option<BackendRoutingInfo> {
        self.backends
            .iter()
            .find(|b| b.id == backend_id)
            .map(|backend| {
                let status = backend.provider.status();
                let pending = backend.pending_count.load(Ordering::Relaxed);
                let stateful = backend.stateful_count.load(Ordering::Relaxed);
                let score = if self.routing_strategy == RoutingStrategy::AdaptiveWeighted {
                    Some(self.adaptive.calculate_score(backend))
                } else {
                    None
                };

                BackendRoutingInfo {
                    pending,
                    stateful,
                    available: status.available.get(),
                    max_size: status.max_size.get(),
                    adaptive_score: score,
                }
            })
    }

    /// Get the current routing strategy
    #[must_use]
    pub fn routing_strategy(&self) -> RoutingStrategy {
        self.routing_strategy
    }
}

/// Backend routing information for TUI display
#[derive(Debug, Clone, Copy)]
pub struct BackendRoutingInfo {
    /// Number of pending requests
    pub pending: usize,
    /// Number of stateful (hybrid mode) connections
    pub stateful: usize,
    /// Number of available connections in pool
    pub available: usize,
    /// Maximum pool size
    pub max_size: usize,
    /// Adaptive routing score (if using AWR)
    pub adaptive_score: Option<f64>,
}

#[cfg(test)]
mod tests;
