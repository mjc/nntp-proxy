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
//!     ServerName::try_new("server1".to_string()).unwrap(),
//!     provider,
//!     0, // tier (lower = higher priority)
//! );
//!
//! // Route a command
//! let client_id = ClientId::new();
//! let backend_id = selector.route_command(client_id, "LIST").unwrap();
//!
//! // After command completes
//! selector.complete_command(backend_id);
//! ```

mod strategies;

use anyhow::Result;
use derive_more::{AsRef, Deref, Display, From};
use nutype::nutype;
use std::cmp::Ordering as CmpOrdering;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::{debug, info};

use crate::config::BackendSelectionStrategy;
use crate::pool::DeadpoolConnectionProvider;
use crate::types::{BackendId, ClientId, ServerName};
use strategies::{LeastLoaded, WeightedRoundRobin};

/// Selection strategy enum that holds either strategy type
#[derive(Debug)]
enum SelectionStrategy {
    WeightedRoundRobin(WeightedRoundRobin),
    LeastLoaded(LeastLoaded),
}

/// Load ratio (pending requests / max connections)
///
/// Lower ratios indicate less loaded backends. Range: 0.0 (empty) to f64::MAX (no capacity).
#[derive(Debug, Clone, Copy, PartialEq, Display, From, AsRef, Deref)]
pub struct LoadRatio(f64);

impl LoadRatio {
    /// Maximum load ratio when no capacity available
    pub const MAX: Self = Self(f64::MAX);

    /// Minimum load ratio for empty backend
    pub const MIN: Self = Self(0.0);

    /// Create a new load ratio
    #[inline]
    #[must_use]
    pub const fn new(ratio: f64) -> Self {
        Self(ratio)
    }

    /// Get the inner f64 value
    #[inline]
    #[must_use]
    pub const fn get(&self) -> f64 {
        self.0
    }
}

impl PartialOrd for LoadRatio {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        self.0.partial_cmp(&other.0)
    }
}

/// Atomic counter for pending requests on a backend
#[derive(Debug, Clone, Display, From, AsRef, Deref)]
#[display("PendingCount({})", "_0.load(Ordering::Relaxed)")]
pub struct PendingCount(Arc<AtomicUsize>);

// Manual PartialEq because Arc<AtomicUsize> doesn't auto-derive
impl PartialEq for PendingCount {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl PartialEq<usize> for PendingCount {
    fn eq(&self, other: &usize) -> bool {
        self.get() == *other
    }
}

impl Eq for PendingCount {}

impl PendingCount {
    /// Create a new pending count initialized to zero
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    /// Increment the pending count
    #[inline]
    pub fn increment(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement the pending count
    #[inline]
    pub fn decrement(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }

    /// Get the current pending count
    #[inline]
    #[must_use]
    pub fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for PendingCount {
    fn default() -> Self {
        Self::new()
    }
}

/// Atomic counter for stateful connections on a backend
#[derive(Debug, Clone, Display, From, AsRef, Deref)]
#[display("StatefulCount({})", "_0.load(Ordering::Relaxed)")]
pub struct StatefulCount(Arc<AtomicUsize>);

// Manual PartialEq because Arc<AtomicUsize> doesn't auto-derive
impl PartialEq for StatefulCount {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl PartialEq<usize> for StatefulCount {
    fn eq(&self, other: &usize) -> bool {
        self.get() == *other
    }
}

impl Eq for StatefulCount {}

impl StatefulCount {
    /// Create a new stateful count initialized to zero
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    /// Get the current stateful count
    #[inline]
    #[must_use]
    pub fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    /// Try to acquire a stateful slot (compare-exchange loop)
    ///
    /// Returns true if successfully incremented below max_stateful limit
    pub fn try_acquire(&self, max_stateful: usize) -> bool {
        let mut current = self.0.load(Ordering::Acquire);
        loop {
            if current >= max_stateful {
                return false;
            }

            match self.0.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// Release a stateful slot (decrement if > 0)
    ///
    /// Returns Ok(previous_value) if successfully decremented, Err(0) if already zero
    pub fn release(&self) -> Result<usize, usize> {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                if current == 0 {
                    None
                } else {
                    Some(current - 1)
                }
            })
    }
}

impl Default for StatefulCount {
    fn default() -> Self {
        Self::new()
    }
}

/// Number of backend servers in the router
#[nutype(derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Display, From, AsRef
))]
pub struct BackendCount(usize);

impl PartialEq<usize> for BackendCount {
    fn eq(&self, other: &usize) -> bool {
        self.into_inner() == *other
    }
}

impl PartialOrd<usize> for BackendCount {
    fn partial_cmp(&self, other: &usize) -> Option<CmpOrdering> {
        self.into_inner().partial_cmp(other)
    }
}

impl BackendCount {
    /// Zero backends
    pub fn zero() -> Self {
        Self::new(0)
    }

    /// Get the inner usize value
    #[inline]
    #[must_use]
    pub fn get(&self) -> usize {
        self.into_inner()
    }
}

/// Total weight across all backends (sum of max_connections)
#[nutype(derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Display, From, AsRef
))]
pub struct TotalWeight(usize);

impl PartialEq<usize> for TotalWeight {
    fn eq(&self, other: &usize) -> bool {
        self.into_inner() == *other
    }
}

impl PartialOrd<usize> for TotalWeight {
    fn partial_cmp(&self, other: &usize) -> Option<CmpOrdering> {
        self.into_inner().partial_cmp(other)
    }
}

impl TotalWeight {
    /// Zero weight
    pub fn zero() -> Self {
        Self::new(0)
    }

    /// Get the inner usize value
    #[inline]
    #[must_use]
    pub fn get(&self) -> usize {
        self.into_inner()
    }
}

/// Traffic share percentage for a backend
#[nutype(derive(Debug, Clone, Copy, PartialEq, Display, From, AsRef))]
pub struct TrafficShare(f64);

impl TrafficShare {
    /// Get the inner f64 value
    #[inline]
    #[must_use]
    pub fn get(&self) -> f64 {
        self.into_inner()
    }

    /// Calculate traffic share from max_connections and total_weight
    #[inline]
    #[must_use]
    pub fn from_weight(max_connections: usize, total_weight: TotalWeight) -> Self {
        if total_weight.get() > 0 {
            Self::new((max_connections as f64 / total_weight.get() as f64) * 100.0)
        } else {
            Self::new(0.0)
        }
    }
}

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
    pending_count: PendingCount,
    /// Number of connections in stateful mode (for hybrid routing reservation)
    stateful_count: StatefulCount,
    /// Server tier for prioritization (lower = higher priority)
    tier: u8,
}

impl BackendInfo {
    /// Calculate load ratio (pending requests / max connections)
    ///
    /// Lower ratios indicate less loaded backends.
    #[must_use]
    fn load_ratio(&self) -> LoadRatio {
        let max_conns = self.provider.max_size() as f64;
        if max_conns > 0.0 {
            let pending = self.pending_count.get() as f64;
            LoadRatio::new(pending / max_conns)
        } else {
            LoadRatio::MAX
        }
    }
}

/// RAII guard that decrements the backend's pending command count on drop.
///
/// Prevents TUI in-flight count drift when error paths forget to call `complete_command()`.
/// On success paths, call [`CommandGuard::complete`] to explicitly finalize.
/// On error/early-return paths, `Drop` handles cleanup automatically.
pub struct CommandGuard {
    router: Arc<BackendSelector>,
    backend_id: BackendId,
    completed: bool,
}

impl CommandGuard {
    /// Create a new guard that will call `complete_command` on drop.
    pub fn new(router: Arc<BackendSelector>, backend_id: BackendId) -> Self {
        Self {
            router,
            backend_id,
            completed: false,
        }
    }

    /// Explicitly complete (consumes the obligation, Drop becomes no-op).
    pub fn complete(mut self) {
        self.router.complete_command(self.backend_id);
        self.completed = true;
    }

    /// Get the backend ID this guard is protecting.
    #[must_use]
    pub fn backend_id(&self) -> BackendId {
        self.backend_id
    }
}

impl Drop for CommandGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.router.complete_command(self.backend_id);
        }
    }
}

/// Selects backend servers using weighted round-robin with load tracking
///
/// # Thread Safety
///
/// This struct is designed for concurrent access across multiple threads.
/// The round-robin counter and pending counts use atomic operations for
/// lock-free performance.
///
/// # Load Balancing
///
/// - **Strategy**: Weighted round-robin based on max_connections
/// - **Tracking**: Atomic counters track pending commands per backend
/// - **Monitoring**: Load statistics available via `backend_load()`
/// - **Fairness**: Backends with larger pools receive proportionally more requests
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
///     ServerName::try_new("backend-1".to_string()).unwrap(),
///     provider,
///     0, // tier (lower = higher priority)
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
    /// Selection strategy (weighted round-robin or least-loaded)
    strategy: SelectionStrategy,
}

impl Default for BackendSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl BackendSelector {
    /// Find backend by ID
    ///
    /// Common helper to avoid repeating find logic across methods.
    #[inline]
    fn find_backend(&self, backend_id: BackendId) -> Option<&BackendInfo> {
        self.backends.iter().find(|b| b.id == backend_id)
    }

    /// Get the tier for a backend
    ///
    /// Returns the tier value for the specified backend, or None if the backend doesn't exist.
    /// Used by cache to implement tier-aware TTL (higher tier = longer TTL).
    #[inline]
    #[must_use]
    pub fn get_tier(&self, backend_id: BackendId) -> Option<u8> {
        self.find_backend(backend_id).map(|b| b.tier)
    }

    /// Create a new backend selector with weighted round-robin strategy (default)
    #[must_use]
    pub fn new() -> Self {
        Self::with_strategy(BackendSelectionStrategy::WeightedRoundRobin)
    }

    /// Create a new backend selector with specified strategy
    #[must_use]
    pub fn with_strategy(strategy: BackendSelectionStrategy) -> Self {
        let selection_strategy = match strategy {
            BackendSelectionStrategy::WeightedRoundRobin => {
                SelectionStrategy::WeightedRoundRobin(WeightedRoundRobin::new(0))
            }
            BackendSelectionStrategy::LeastLoaded => {
                SelectionStrategy::LeastLoaded(LeastLoaded::new())
            }
        };

        Self {
            // Pre-allocate for typical number of backend servers (most setups have 2-8)
            backends: Vec::with_capacity(4),
            strategy: selection_strategy,
        }
    }

    /// Add a backend server to the router
    ///
    /// # Arguments
    /// * `backend_id` - Unique identifier for this backend
    /// * `name` - Human-readable name for logging
    /// * `provider` - Connection pool provider
    /// * `tier` - Server tier (lower = higher priority, 0 is highest)
    pub fn add_backend(
        &mut self,
        backend_id: BackendId,
        name: ServerName,
        provider: DeadpoolConnectionProvider,
        tier: u8,
    ) {
        let max_connections = provider.max_size();

        // Update strategy-specific state
        match &mut self.strategy {
            SelectionStrategy::WeightedRoundRobin(wrr) => {
                let old_weight = TotalWeight::new(wrr.total_weight());
                let new_weight = TotalWeight::new(old_weight.get() + max_connections);
                wrr.set_total_weight(new_weight.get());

                // Calculate this backend's share of traffic
                let traffic_share = TrafficShare::from_weight(max_connections, new_weight);

                info!(
                    "Added backend {:?} ({}) tier {} with {} connections - will receive {:.1}% of traffic (total weight: {} -> {}) [weighted round-robin]",
                    backend_id,
                    name,
                    tier,
                    max_connections,
                    traffic_share.get(),
                    old_weight,
                    new_weight
                );
            }
            SelectionStrategy::LeastLoaded(_) => {
                info!(
                    "Added backend {:?} ({}) tier {} with {} connections [least-loaded strategy]",
                    backend_id, name, tier, max_connections
                );
            }
        }

        self.backends.push(BackendInfo {
            id: backend_id,
            name,
            provider,
            pending_count: PendingCount::new(),
            stateful_count: StatefulCount::new(),
            tier,
        });
    }

    /// Select the next backend using the configured strategy with tier-aware prioritization
    ///
    /// Selection is tier-aware: backends with lower tier numbers are tried first.
    /// Within each tier, the configured strategy applies:
    /// - **Weighted round-robin**: Distributes proportionally to max_connections
    /// - **Least-loaded**: Routes to backend with fewest pending requests
    ///
    /// # Arguments
    /// * `availability` - Optional filter to restrict selection to available backends
    fn select_backend(
        &self,
        availability: Option<&crate::cache::ArticleAvailability>,
    ) -> Option<&BackendInfo> {
        if self.backends.is_empty() {
            return None;
        }

        // Filter backends by availability if provided
        let is_available =
            |backend: &&BackendInfo| availability.is_none_or(|avail| avail.should_try(backend.id));

        // Tier filtering only applies to article requests (when availability is provided).
        // For non-article commands (LIST, CAPABILITIES, etc), use all backends regardless of tier.
        let should_apply_tier_filtering = availability.is_some();

        // If tiering applies, find the lowest available tier; otherwise this value is unused
        let lowest_available_tier = if should_apply_tier_filtering {
            self.backends
                .iter()
                .filter(|b| is_available(b))
                .map(|b| b.tier)
                .min()?
        } else {
            0 // Unused when should_apply_tier_filtering is false; see tier_filter closure
        };

        // Filter: if tiering applies, only backends in lowest tier; otherwise all available backends
        let tier_filter = |backend: &&BackendInfo| {
            if should_apply_tier_filtering {
                backend.tier == lowest_available_tier && is_available(backend)
            } else {
                is_available(backend)
            }
        };

        match &self.strategy {
            SelectionStrategy::WeightedRoundRobin(wrr) => {
                // Calculate total weight for this tier only
                let tier_total_weight: usize = self
                    .backends
                    .iter()
                    .filter(tier_filter)
                    .map(|b| b.provider.max_size())
                    .sum();

                if tier_total_weight == 0 {
                    // No backends with positive weight - cannot route
                    return None;
                }

                // Use tier-specific weight for selection to avoid modulo bias
                // (directly select within tier's weight range instead of global % tier)
                let tier_position = wrr.select_with_weight(tier_total_weight)?;

                // Find backend owning this weighted position using cumulative weights
                self.backends
                    .iter()
                    .filter(tier_filter)
                    .scan(0, |cumulative, backend| {
                        *cumulative += backend.provider.max_size();
                        Some((*cumulative, backend))
                    })
                    .find(|(cumulative_weight, _)| tier_position < *cumulative_weight)
                    .map(|(_, backend)| backend)
                    .or_else(|| {
                        // Fallback to first available backend in tier
                        self.backends.iter().find(tier_filter)
                    })
            }
            SelectionStrategy::LeastLoaded(_) => {
                // Find backend with lowest load ratio in the lowest available tier
                self.backends.iter().filter(tier_filter).min_by(|a, b| {
                    a.load_ratio()
                        .partial_cmp(&b.load_ratio())
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
            }
        }
    }

    /// Select a backend for the given command using round-robin
    /// Returns the backend ID to use for this command
    pub fn route_command(&self, _client_id: ClientId, _command: &str) -> Result<BackendId> {
        self.route_command_with_availability(_client_id, _command, None)
    }

    /// Select a backend for the given command, optionally filtering by availability
    pub fn route_command_with_availability(
        &self,
        _client_id: ClientId,
        _command: &str,
        availability: Option<&crate::cache::ArticleAvailability>,
    ) -> Result<BackendId> {
        let backend = self.select_backend(availability).ok_or_else(|| {
            anyhow::anyhow!(
                "No backends available for routing (total backends: {})",
                self.backends.len()
            )
        })?;

        // Increment pending count for load tracking
        backend.pending_count.increment();

        debug!(
            "Selected backend {:?} ({}) for command",
            backend.id, backend.name
        );

        Ok(backend.id)
    }

    /// Mark a command as complete, decrementing the pending count
    pub fn complete_command(&self, backend_id: BackendId) {
        if let Some(backend) = self.find_backend(backend_id) {
            backend.pending_count.decrement();
        }
    }

    /// Manually increment pending count for a specific backend
    /// Used when directly selecting a backend instead of using route_command
    pub fn mark_backend_pending(&self, backend_id: BackendId) {
        if let Some(backend) = self.find_backend(backend_id) {
            backend.pending_count.increment();
        }
    }

    /// Get the connection provider for a backend
    #[must_use]
    pub fn backend_provider(&self, backend_id: BackendId) -> Option<&DeadpoolConnectionProvider> {
        self.find_backend(backend_id).map(|b| &b.provider)
    }

    /// Get the number of backends
    #[must_use]
    #[inline]
    pub fn backend_count(&self) -> BackendCount {
        BackendCount::new(self.backends.len())
    }

    /// Get total weight (sum of all max_connections)
    /// Only applicable for weighted round-robin strategy
    #[must_use]
    #[inline]
    pub fn total_weight(&self) -> TotalWeight {
        match &self.strategy {
            SelectionStrategy::WeightedRoundRobin(wrr) => TotalWeight::new(wrr.total_weight()),
            SelectionStrategy::LeastLoaded(_) => {
                // For least-loaded, return sum of all max_connections for compatibility
                TotalWeight::new(self.backends.iter().map(|b| b.provider.max_size()).sum())
            }
        }
    }

    /// Get backend load (pending requests) for monitoring
    ///
    /// Returns a clone of the PendingCount for the backend, allowing the caller
    /// to query the current value or track it over time.
    #[must_use]
    pub fn backend_load(&self, backend_id: BackendId) -> Option<PendingCount> {
        self.find_backend(backend_id)
            .map(|b| b.pending_count.clone())
    }

    /// Try to acquire a stateful connection slot for hybrid mode
    /// Returns true if acquisition succeeded (within max_connections-1 limit)
    /// Returns false if all stateful slots are taken (need to keep 1 for PCR)
    pub fn try_acquire_stateful(&self, backend_id: BackendId) -> bool {
        if let Some(backend) = self.find_backend(backend_id) {
            // Get max connections from the provider's pool
            let max_connections = backend.provider.max_size();

            // Reserve 1 connection for per-command routing
            let max_stateful = max_connections.saturating_sub(1);

            // Try to acquire slot using StatefulCount's atomic logic
            let acquired = backend.stateful_count.try_acquire(max_stateful);

            if acquired {
                debug!(
                    "Backend {:?} ({}) acquired stateful slot: {}/{}",
                    backend_id,
                    backend.name,
                    backend.stateful_count.get(),
                    max_stateful
                );
            } else {
                debug!(
                    "Backend {:?} ({}) stateful limit reached: {}/{}",
                    backend_id,
                    backend.name,
                    backend.stateful_count.get(),
                    max_stateful
                );
            }

            acquired
        } else {
            false
        }
    }

    /// Release a stateful connection slot
    pub fn release_stateful(&self, backend_id: BackendId) {
        if let Some(backend) = self.find_backend(backend_id) {
            // Atomically decrement using StatefulCount's release method
            match backend.stateful_count.release() {
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
                    "Unexpected error in release: got Err({other}), expected only Err(0)"
                ),
            }
        }
    }

    /// Get the number of stateful connections for a backend
    ///
    /// Returns a clone of the StatefulCount for the backend, allowing the caller
    /// to query the current value or track it over time.
    #[must_use]
    pub fn stateful_count(&self, backend_id: BackendId) -> Option<StatefulCount> {
        self.find_backend(backend_id)
            .map(|b| b.stateful_count.clone())
    }

    /// Get the load ratio for a backend (pending / max_connections)
    ///
    /// Lower ratios indicate less loaded backends. Range: 0.0 (empty) to f64::MAX (no capacity).
    #[must_use]
    pub fn backend_load_ratio(&self, backend_id: BackendId) -> Option<LoadRatio> {
        self.find_backend(backend_id).map(|b| b.load_ratio())
    }

    /// Get the traffic share percentage for a backend
    ///
    /// Only applicable for weighted round-robin strategy. Returns the percentage
    /// of traffic this backend should receive based on its max_connections.
    #[must_use]
    pub fn backend_traffic_share(&self, backend_id: BackendId) -> Option<TrafficShare> {
        self.find_backend(backend_id).map(|b| {
            let total = self.total_weight();
            TrafficShare::from_weight(b.provider.max_size(), total)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_router_with_backend() -> (Arc<BackendSelector>, BackendId) {
        let mut selector = BackendSelector::new();
        let backend_id = BackendId::from_index(0);
        let provider = crate::pool::DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            119,
            "test".to_string(),
            10,
            None,
            None,
        );
        selector.add_backend(
            backend_id,
            ServerName::try_new("test-server".to_string()).unwrap(),
            provider,
            0,
        );
        (Arc::new(selector), backend_id)
    }

    #[test]
    fn command_guard_decrements_on_drop() {
        let (router, backend_id) = make_router_with_backend();

        // Simulate route_command incrementing the pending count
        router.mark_backend_pending(backend_id);
        assert_eq!(router.backend_load(backend_id).unwrap().get(), 1);

        // Guard should decrement on drop
        {
            let _guard = CommandGuard::new(router.clone(), backend_id);
        }
        assert_eq!(router.backend_load(backend_id).unwrap().get(), 0);
    }

    #[test]
    fn command_guard_explicit_complete() {
        let (router, backend_id) = make_router_with_backend();

        router.mark_backend_pending(backend_id);
        assert_eq!(router.backend_load(backend_id).unwrap().get(), 1);

        let guard = CommandGuard::new(router.clone(), backend_id);
        guard.complete();
        assert_eq!(router.backend_load(backend_id).unwrap().get(), 0);
    }

    #[test]
    fn command_guard_no_double_decrement() {
        let (router, backend_id) = make_router_with_backend();

        // Start with pending count of 1
        router.mark_backend_pending(backend_id);
        assert_eq!(router.backend_load(backend_id).unwrap().get(), 1);

        // Explicit complete + drop should only decrement once
        let guard = CommandGuard::new(router.clone(), backend_id);
        guard.complete();
        // After complete(), count should be 0; drop should be a no-op
        assert_eq!(router.backend_load(backend_id).unwrap().get(), 0);
        // If double-decrement happened, we'd see wrapping (very large number)
        // Since we're at 0 already and drop is a no-op, this confirms correctness
    }

    #[test]
    fn command_guard_backend_id_accessor() {
        let (router, backend_id) = make_router_with_backend();
        let guard = CommandGuard::new(router, backend_id);
        assert_eq!(guard.backend_id(), backend_id);
    }
}
