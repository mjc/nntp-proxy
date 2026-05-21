//! Backend server selection and load balancing
//!
//! This module handles selecting backend servers using round-robin
//! with simple load tracking for monitoring.
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
//! let backend_id = selector.route_without_availability(client_id).unwrap();
//!
//! // After command completes
//! selector.complete_command(backend_id);
//! ```

mod backend_info;
mod strategies;

use anyhow::Result;
use nutype::nutype;
use std::cmp::Ordering as CmpOrdering;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{debug, info, warn};

use crate::cache::ArticleAvailability;
use crate::config::BackendSelectionStrategy;
use crate::pool::DeadpoolConnectionProvider;
use crate::types::{BackendId, ClientId, ServerName};
use strategies::{LeastLoaded, WeightedRoundRobin};

use backend_info::BackendInfo;
pub use backend_info::{LoadRatio, PendingCount, StatefulCount};

/// Selection strategy enum that holds either strategy type
#[derive(Debug)]
enum SelectionStrategy {
    WeightedRoundRobin(WeightedRoundRobin),
    LeastLoaded(LeastLoaded),
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
    #[must_use]
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

/// Total weight across all backends (sum of `max_connections`)
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
    #[must_use]
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

    /// Calculate traffic share from `max_connections` and `total_weight`
    #[inline]
    #[must_use]
    pub fn from_weight(max_connections: usize, total_weight: TotalWeight) -> Self {
        if total_weight.get() > 0 {
            // Traffic share is a display percentage; routing uses the original
            // integer weights, so precision loss here cannot affect selection.
            #[allow(clippy::cast_precision_loss)] // This is a display-only capacity percentage.
            // Display-only percentage; backend weights stay in integer form.
            Self::new((max_connections as f64 / total_weight.get() as f64) * 100.0)
        } else {
            Self::new(0.0)
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
    pub const fn new(router: Arc<BackendSelector>, backend_id: BackendId) -> Self {
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
    pub const fn backend_id(&self) -> BackendId {
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
/// - **Strategy**: Weighted round-robin based on `max_connections`
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
/// // Route commands without article availability filtering
/// let backend = selector.route_without_availability(ClientId::new())?;
/// # Ok::<(), anyhow::Error>(())
/// ```
#[derive(Debug)]
pub struct BackendSelector {
    /// Backend connection providers
    backends: Vec<BackendInfo>,
    /// Selection strategy (weighted round-robin or least-loaded)
    strategy: SelectionStrategy,
    /// H4: Pre-computed sorted unique tiers (avoids Vec allocation in hot path)
    sorted_tiers: smallvec::SmallVec<[u8; 4]>,
    /// Serializes backend selection with the matching pending-count increment.
    selection_lock: Mutex<()>,
    /// Capacity-fair probe counter for the first availability-aware article attempt.
    initial_article_probe_counter: AtomicUsize,
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

    /// Iterate backend tiers in priority order.
    ///
    /// Lower tier numbers have higher priority.
    pub(crate) fn tiers(&self) -> impl Iterator<Item = u8> + '_ {
        self.sorted_tiers.iter().copied()
    }

    /// Iterate backend IDs within a tier.
    pub(crate) fn backend_ids_in_tier(&self, tier: u8) -> impl Iterator<Item = BackendId> + '_ {
        self.backends
            .iter()
            .filter(move |backend| backend.tier == tier)
            .map(|backend| backend.id)
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
            sorted_tiers: smallvec::SmallVec::new(),
            selection_lock: Mutex::new(()),
            initial_article_probe_counter: AtomicUsize::new(0),
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

        // H4: Maintain sorted unique tiers (avoids Vec allocation in select_backend hot path)
        if !self.sorted_tiers.contains(&tier) {
            self.sorted_tiers.push(tier);
            self.sorted_tiers.sort_unstable();
        }
    }

    /// Select the next backend using the configured strategy with tier-aware prioritization
    ///
    /// Selection is tier-aware: backends with lower tier numbers are tried first.
    /// Within each tier, the configured strategy applies:
    /// - **Weighted round-robin**: Distributes proportionally to `max_connections`
    /// - **Least-loaded**: Routes to backend with fewest pending requests
    ///
    /// # Arguments
    /// * `availability` - Optional filter to restrict selection to available backends
    fn select_backend(
        &self,
        availability: Option<&ArticleAvailability>,
        suppressed_backends: u8,
    ) -> Option<&BackendInfo> {
        if self.backends.is_empty() {
            return None;
        }

        // Availability check closure
        let is_available = |backend: &&BackendInfo| {
            (suppressed_backends == 0 || backend.id.availability_bit() & suppressed_backends == 0)
                && availability.is_none_or(|avail| avail.should_try(backend.id))
        };

        // H4: Tier filtering enabled - try tiers in order 0, 1, 2, ...
        // Use pre-computed sorted tiers (no allocation)
        // Try each tier until we find an available backend
        for &tier in &self.sorted_tiers {
            // Only count available backends if debug logging is enabled (avoid O(n) scan)
            if tracing::enabled!(tracing::Level::DEBUG) {
                let available_in_tier = self
                    .backends
                    .iter()
                    .filter(|b| b.tier == tier && is_available(b))
                    .count();

                tracing::debug!(
                    tier = tier,
                    available_in_tier = available_in_tier,
                    "Checking tier for available backends"
                );
            }

            // Try to select from this specific tier
            let tier_filter = |b: &&BackendInfo| b.tier == tier && is_available(b);

            let selected = if availability
                .is_some_and(|avail| !self.availability_checked_in_tier(avail, tier))
            {
                self.select_capacity_weighted(tier_filter)
            } else {
                self.select_weighted(tier_filter)
            };
            if tracing::enabled!(tracing::Level::DEBUG) {
                self.debug_log_selection_candidates(tier, availability, selected.map(|b| b.id));
            }

            if let Some(backend) = selected {
                tracing::debug!(
                    backend_id = backend.id.as_index(),
                    backend_name = backend.name.as_str(),
                    tier = tier,
                    "Selected backend"
                );
                return Some(backend);
            }

            if availability.is_some()
                && self.backends.iter().any(|backend| {
                    backend.tier == tier
                        && availability.is_none_or(|avail| avail.should_try(backend.id))
                })
            {
                tracing::debug!(
                    tier = tier,
                    suppressed_backends = format_args!("{suppressed_backends:08b}"),
                    "No selectable backend remains in current tier after transient suppressions"
                );
                return None;
            }

            tracing::debug!(tier = tier, "No available backends in tier, trying next");
        }

        // All tiers exhausted
        tracing::debug!("All tiers exhausted, no backends available");
        None
    }

    fn availability_checked_in_tier(&self, availability: &ArticleAvailability, tier: u8) -> bool {
        self.backends.iter().any(|backend| {
            backend.tier == tier && availability.checked_bits() & backend.id.availability_bit() != 0
        })
    }

    /// Select a backend by capacity weight, independent of current pending load.
    fn select_capacity_weighted<F>(&self, filter: F) -> Option<&BackendInfo>
    where
        F: Fn(&&BackendInfo) -> bool,
    {
        let total_weight: usize = self
            .backends
            .iter()
            .filter(&filter)
            .map(|b| b.provider.max_size())
            .sum();

        if total_weight == 0 {
            return None;
        }

        let position = self
            .initial_article_probe_counter
            .fetch_add(1, Ordering::Relaxed)
            % total_weight;

        self.backends
            .iter()
            .filter(&filter)
            .scan(0, |cumulative, backend| {
                *cumulative += backend.provider.max_size();
                Some((*cumulative, backend))
            })
            .find(|(cumulative_weight, _)| position < *cumulative_weight)
            .map(|(_, backend)| backend)
            .or_else(|| self.backends.iter().find(&filter))
    }

    #[allow(clippy::cast_precision_loss)]
    fn debug_log_selection_candidates(
        &self,
        tier: u8,
        availability: Option<&ArticleAvailability>,
        selected_backend: Option<BackendId>,
    ) {
        let availability_checked_bits = availability.map_or(0, ArticleAvailability::checked_bits);
        let availability_missing_bits = availability.map_or(0, ArticleAvailability::missing_bits);

        for backend in self.backends.iter().filter(|backend| backend.tier == tier) {
            let status = backend.provider.status_counts();
            let checked_out = status.size.saturating_sub(status.available);
            let pending = backend.pending_count.get();
            let active_for_score = pending.max(checked_out);
            let load_ratio = if status.max_size > 0 {
                active_for_score as f64 / status.max_size as f64
            } else {
                f64::MAX
            };

            tracing::debug!(
                backend_id = backend.id.as_index(),
                backend_name = backend.name.as_str(),
                pool = %backend.provider.name(),
                tier,
                selected = selected_backend == Some(backend.id),
                should_try = availability.is_none_or(|avail| avail.should_try(backend.id)),
                availability_checked_bits,
                availability_missing_bits,
                pending,
                checked_out,
                active_for_score,
                load_ratio,
                pool_available = status.available,
                pool_size = status.size,
                pool_max_size = status.max_size,
                pool_waiting = status.waiting,
                weight = status.max_size,
                "Backend selection candidate"
            );
        }
    }

    /// Select a backend using weighted round-robin from backends matching the filter
    fn select_weighted<F>(&self, filter: F) -> Option<&BackendInfo>
    where
        F: Fn(&&BackendInfo) -> bool,
    {
        match &self.strategy {
            SelectionStrategy::WeightedRoundRobin(wrr) => {
                // Sum weights for backends passing filter
                let total_weight: usize = self
                    .backends
                    .iter()
                    .filter(&filter)
                    .map(|b| b.provider.max_size())
                    .sum();

                if total_weight == 0 {
                    return None; // No backends match filter
                }

                // Select position in weighted distribution
                let position = wrr.select_with_weight(total_weight)?;

                // Find backend at that position
                self.backends
                    .iter()
                    .filter(&filter)
                    .scan(0, |cumulative, backend| {
                        *cumulative += backend.provider.max_size();
                        Some((*cumulative, backend))
                    })
                    .find(|(cumulative_weight, _)| position < *cumulative_weight)
                    .map(|(_, backend)| backend)
                    .or_else(|| {
                        // Fallback: first backend matching filter
                        self.backends.iter().find(&filter)
                    })
            }
            SelectionStrategy::LeastLoaded(least_loaded) => {
                let mut selected: Option<&BackendInfo> = None;
                let mut selected_load = LoadRatio::MAX;
                let mut ties = 0usize;

                for backend in self.backends.iter().filter(&filter) {
                    let load = backend.load_ratio();
                    match load
                        .partial_cmp(&selected_load)
                        .unwrap_or(std::cmp::Ordering::Greater)
                    {
                        std::cmp::Ordering::Less => {
                            selected = Some(backend);
                            selected_load = load;
                            ties = 1;
                        }
                        std::cmp::Ordering::Equal => {
                            ties += 1;
                            if least_loaded.should_replace_tie(ties) {
                                selected = Some(backend);
                            }
                        }
                        std::cmp::Ordering::Greater => {}
                    }
                }

                selected
            }
        }
    }

    /// Select a backend for a client request without article availability filtering.
    ///
    /// # Errors
    /// Returns an error when no backend is currently eligible for routing.
    pub fn route_without_availability(&self, client_id: ClientId) -> Result<BackendId> {
        self.route_with_availability(client_id, None)
    }

    /// Select a backend for a client request, optionally filtering by availability.
    ///
    /// # Errors
    /// Returns an error when no backend remains eligible after applying the
    /// optional availability filter.
    pub fn route_with_availability(
        &self,
        _client_id: ClientId,
        availability: Option<&ArticleAvailability>,
    ) -> Result<BackendId> {
        if self.should_serialize_selection(availability) {
            let _selection_guard = self.selection_lock.lock().unwrap_or_else(|poisoned| {
                warn!("Backend selector lock was poisoned; continuing with recovered state");
                poisoned.into_inner()
            });
            return self.route_selected_backend(availability, 0);
        }

        self.route_selected_backend(availability, 0)
    }

    pub(crate) fn route_with_availability_suppressing(
        &self,
        _client_id: ClientId,
        availability: Option<&ArticleAvailability>,
        suppressed_backends: u8,
    ) -> Result<BackendId> {
        if self.should_serialize_selection(availability) {
            let _selection_guard = self.selection_lock.lock().unwrap_or_else(|poisoned| {
                warn!("Backend selector lock was poisoned; continuing with recovered state");
                poisoned.into_inner()
            });
            return self.route_selected_backend(availability, suppressed_backends);
        }

        self.route_selected_backend(availability, suppressed_backends)
    }

    fn should_serialize_selection(&self, availability: Option<&ArticleAvailability>) -> bool {
        matches!(self.strategy, SelectionStrategy::LeastLoaded(_))
            && availability.is_none_or(|avail| avail.checked_bits() != 0)
    }

    fn route_selected_backend(
        &self,
        availability: Option<&ArticleAvailability>,
        suppressed_backends: u8,
    ) -> Result<BackendId> {
        let backend = self
            .select_backend(availability, suppressed_backends)
            .ok_or_else(|| {
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
    /// Used when directly selecting a backend instead of using `route`.
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

    /// Get total weight (sum of all `max_connections`)
    /// Only applicable for weighted round-robin strategy
    #[must_use]
    #[inline]
    pub fn total_weight(&self) -> TotalWeight {
        match &self.strategy {
            SelectionStrategy::WeightedRoundRobin(wrr) => TotalWeight::new(wrr.total_weight()),
            SelectionStrategy::LeastLoaded(_) => {
                // Least-loaded does not use weights; expose aggregate capacity.
                TotalWeight::new(self.backends.iter().map(|b| b.provider.max_size()).sum())
            }
        }
    }

    /// Get backend load (pending requests) for monitoring
    ///
    /// Returns a clone of the `PendingCount` for the backend, allowing the caller
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
        self.find_backend(backend_id).is_some_and(|backend| {
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
        })
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
    /// Returns a clone of the `StatefulCount` for the backend, allowing the caller
    /// to query the current value or track it over time.
    #[must_use]
    pub fn stateful_count(&self, backend_id: BackendId) -> Option<StatefulCount> {
        self.find_backend(backend_id)
            .map(|b| b.stateful_count.clone())
    }

    /// Get the load ratio for a backend (pending / `max_connections`)
    ///
    /// Lower ratios indicate less loaded backends. Range: 0.0 (empty) to `f64::MAX` (no capacity).
    #[must_use]
    pub fn backend_load_ratio(&self, backend_id: BackendId) -> Option<LoadRatio> {
        self.find_backend(backend_id)
            .map(backend_info::BackendInfo::load_ratio)
    }

    /// Get the traffic share percentage for a backend
    ///
    /// Only applicable for weighted round-robin strategy. Returns the percentage
    /// of traffic this backend should receive based on its `max_connections`.
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

        // Simulate route incrementing the pending count.
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

    #[test]
    fn selection_lock_is_only_needed_for_load_based_routing() {
        let weighted = BackendSelector::with_strategy(BackendSelectionStrategy::WeightedRoundRobin);
        assert!(!weighted.should_serialize_selection(None));
        assert!(!weighted.should_serialize_selection(Some(&ArticleAvailability::new())));

        let least_loaded = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);
        assert!(least_loaded.should_serialize_selection(None));
        assert!(!least_loaded.should_serialize_selection(Some(&ArticleAvailability::new())));
    }

    #[test]
    fn transient_suppression_can_try_same_tier_without_escalating() {
        let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);
        for (index, name, tier) in [(0, "tier0-a", 0), (1, "tier0-b", 0), (2, "tier1", 1)] {
            selector.add_backend(
                BackendId::from_index(index),
                ServerName::try_new(name.to_string()).unwrap(),
                crate::pool::DeadpoolConnectionProvider::new(
                    "localhost".to_string(),
                    119,
                    name.to_string(),
                    10,
                    None,
                    None,
                ),
                tier,
            );
        }

        let availability = ArticleAvailability::new();
        let backend = selector
            .route_with_availability_suppressing(
                ClientId::new(),
                Some(&availability),
                BackendId::from_index(0).availability_bit(),
            )
            .unwrap();

        assert_eq!(backend, BackendId::from_index(1));

        let exhausted_tier0 = selector.route_with_availability_suppressing(
            ClientId::new(),
            Some(&availability),
            BackendId::from_index(0).availability_bit()
                | BackendId::from_index(1).availability_bit(),
        );
        assert!(
            exhausted_tier0.is_err(),
            "transient backend failures must not escalate to tier 1 before tier 0 has all 430s"
        );
    }

    #[test]
    fn first_probe_selection_is_tier_local() {
        let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);
        for (index, name, tier, max_connections) in [
            (0, "tier0", 0, 10),
            (1, "tier1-small", 1, 1),
            (2, "tier1-large", 1, 10),
        ] {
            selector.add_backend(
                BackendId::from_index(index),
                ServerName::try_new(name.to_string()).unwrap(),
                crate::pool::DeadpoolConnectionProvider::new(
                    "localhost".to_string(),
                    119,
                    name.to_string(),
                    max_connections,
                    None,
                    None,
                ),
                tier,
            );
        }
        selector.mark_backend_pending(BackendId::from_index(1));

        let mut availability = ArticleAvailability::new();
        availability.record_missing(BackendId::from_index(0));
        let backend = selector
            .route_with_availability(ClientId::new(), Some(&availability))
            .unwrap();

        assert_eq!(
            backend,
            BackendId::from_index(1),
            "first probe in newly eligible tier should be capacity-fair, not biased by load"
        );
    }

    #[test]
    fn transient_suppression_does_not_block_escalation_after_real_430s() {
        let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);
        for (index, name, tier) in [(0, "tier0-a", 0), (1, "tier0-b", 0), (2, "tier1", 1)] {
            selector.add_backend(
                BackendId::from_index(index),
                ServerName::try_new(name.to_string()).unwrap(),
                crate::pool::DeadpoolConnectionProvider::new(
                    "localhost".to_string(),
                    119,
                    name.to_string(),
                    10,
                    None,
                    None,
                ),
                tier,
            );
        }

        let mut availability = ArticleAvailability::new();
        availability.record_missing(BackendId::from_index(0));
        availability.record_missing(BackendId::from_index(1));

        let backend = selector
            .route_with_availability_suppressing(
                ClientId::new(),
                Some(&availability),
                BackendId::from_index(0).availability_bit(),
            )
            .unwrap();

        assert_eq!(
            backend,
            BackendId::from_index(2),
            "tier escalation is allowed after tier 0 has authoritative 430s"
        );
    }

    #[test]
    fn zero_suppression_keeps_large_backend_routing_off_the_availability_bitset() {
        let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);
        for index in 0..12 {
            selector.add_backend(
                BackendId::from_index(index),
                ServerName::try_new(format!("backend-{index}")).unwrap(),
                crate::pool::DeadpoolConnectionProvider::new(
                    "localhost".to_string(),
                    119,
                    format!("backend-{index}"),
                    10,
                    None,
                    None,
                ),
                0,
            );
        }

        let backend = selector
            .route_without_availability(ClientId::new())
            .unwrap();

        assert!(
            backend.as_index() < 12,
            "routing without suppression must not touch the 8-backend availability bitset"
        );
    }
}
