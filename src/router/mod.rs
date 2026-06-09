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
//! use nntp_proxy::types::{ClientId, ServerName};
//! # use nntp_proxy::pool::DeadpoolConnectionProvider;
//!
//! let mut selector = BackendSelector::new();
//! # let provider = DeadpoolConnectionProvider::new(
//! #     "localhost".to_string(), 119, "test".to_string(), 10, None, None
//! # );
//! selector.add_backend(
//!     ServerName::try_new("server1".to_string()).unwrap(),
//!     provider,
//!     0, // tier (lower = higher priority)
//! );
//!
//! // Route a command
//! let client_id = ClientId::new();
//! let backend_id = selector
//!     .route(nntp_proxy::router::RouteRequest::new(client_id))
//!     .unwrap();
//!
//! // After command completes
//! selector.complete_command(backend_id);
//! ```

mod backend_info;
mod strategies;

use anyhow::Result;
use nutype::nutype;
use std::cmp::Ordering as CmpOrdering;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tracing::{debug, info};

use crate::cache::ArticleAvailability;
use crate::config::BackendSelectionStrategy;
use crate::pool::DeadpoolConnectionProvider;
use crate::types::{BackendId, ClientId, ServerName};
use strategies::{LeastLoaded, WeightedRoundRobin};

use backend_info::BackendInfo;
pub use backend_info::{LoadRatio, PendingCount, StatefulCount};

mod route_mode {
    pub trait Sealed {}
}

struct SelectedBackend<'a> {
    backend: &'a BackendInfo,
    pending_snapshot: Option<usize>,
}

/// Builder for selecting a backend.
#[derive(Debug, Clone)]
pub struct RouteRequest<'a, Mode = RawRoute> {
    _client_id: ClientId,
    suppressed_backends: SuppressedBackends,
    mode: Mode,
    _lifetime: std::marker::PhantomData<&'a ()>,
}

/// Transient backend suppressions for a single retry loop.
///
/// This is intentionally distinct from article availability. Suppression means
/// "do not pick this backend again for this in-flight request because its pool
/// or connection failed"; it does not mean the backend lacks the article.
///
/// Keep this as a fixed bitmap. Real deployments have a small number of Usenet
/// backends, and a dynamic set here would only add overhead to a hot retry path.
/// This is deliberately the same fixed bitmap width as `ArticleAvailability`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SuppressedBackends {
    bits: usize,
}

impl SuppressedBackends {
    #[must_use]
    pub const fn empty() -> Self {
        Self { bits: 0 }
    }

    pub fn suppress(&mut self, backend_id: BackendId) {
        self.bits |= backend_id.availability_bit();
    }

    #[must_use]
    pub fn contains(self, backend_id: BackendId) -> bool {
        self.bits & backend_id.availability_bit() != 0
    }

    #[must_use]
    pub const fn bits(self) -> usize {
        self.bits
    }
}

/// Routing without article availability state.
#[derive(Debug, Clone, Copy)]
pub struct RawRoute;

/// Routing for article commands with availability state.
#[derive(Debug, Clone, Copy)]
pub struct ArticleRoute<'a> {
    availability: &'a ArticleAvailability,
}

/// Backend selected through article availability routing.
///
/// Article execution accepts this type instead of raw `BackendId`, so a caller
/// must route with current `ArticleAvailability` before it can issue an article
/// request to a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArticleBackend {
    backend_id: BackendId,
}

impl ArticleBackend {
    #[inline]
    #[must_use]
    pub(crate) fn from_availability(
        backend_id: BackendId,
        availability: &ArticleAvailability,
    ) -> Option<Self> {
        availability
            .should_try(backend_id)
            .then_some(Self { backend_id })
    }

    #[inline]
    #[must_use]
    pub const fn backend_id(self) -> BackendId {
        self.backend_id
    }

    #[inline]
    #[must_use]
    pub fn as_index(self) -> usize {
        self.backend_id.as_index()
    }
}

impl RouteRequest<'_, RawRoute> {
    #[must_use]
    pub fn new(client_id: ClientId) -> Self {
        Self {
            _client_id: client_id,
            suppressed_backends: SuppressedBackends::empty(),
            mode: RawRoute,
            _lifetime: std::marker::PhantomData,
        }
    }

    #[must_use]
    pub fn with_availability(
        self,
        availability: &ArticleAvailability,
    ) -> RouteRequest<'_, ArticleRoute<'_>> {
        RouteRequest {
            _client_id: self._client_id,
            suppressed_backends: self.suppressed_backends,
            mode: ArticleRoute { availability },
            _lifetime: std::marker::PhantomData,
        }
    }

    #[must_use]
    pub fn suppressing_backends(mut self, suppressed_backends: SuppressedBackends) -> Self {
        self.suppressed_backends = suppressed_backends;
        self
    }
}

impl RouteRequest<'_, ArticleRoute<'_>> {
    #[must_use]
    pub fn suppressing_backends(mut self, suppressed_backends: SuppressedBackends) -> Self {
        self.suppressed_backends = suppressed_backends;
        self
    }
}

#[doc(hidden)]
pub trait RouteMode: route_mode::Sealed {
    type Output;

    fn availability<'r>(request: &'r RouteRequest<'_, Self>) -> Option<&'r ArticleAvailability>
    where
        Self: Sized;

    fn output(request: &RouteRequest<'_, Self>, backend_id: BackendId) -> Result<Self::Output>
    where
        Self: Sized;
}

impl RouteMode for RawRoute {
    type Output = BackendId;

    fn availability<'r>(_request: &'r RouteRequest<'_, Self>) -> Option<&'r ArticleAvailability> {
        None
    }

    fn output(_request: &RouteRequest<'_, Self>, backend_id: BackendId) -> Result<Self::Output> {
        Ok(backend_id)
    }
}

impl route_mode::Sealed for RawRoute {}

impl RouteMode for ArticleRoute<'_> {
    type Output = ArticleBackend;

    fn availability<'r>(request: &'r RouteRequest<'_, Self>) -> Option<&'r ArticleAvailability> {
        Some(request.mode.availability)
    }

    fn output(request: &RouteRequest<'_, Self>, backend_id: BackendId) -> Result<Self::Output> {
        ArticleBackend::from_availability(backend_id, request.mode.availability).ok_or_else(|| {
            anyhow::anyhow!(
                "selected backend {} is no longer eligible for article routing",
                backend_id.as_index()
            )
        })
    }
}

impl route_mode::Sealed for ArticleRoute<'_> {}

/// Selection strategy enum that holds either strategy type
#[derive(Debug)]
enum SelectionStrategy {
    WeightedRoundRobin(WeightedRoundRobin),
    LeastLoaded(LeastLoaded),
}

/// Number of backend servers in the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct BackendCount(usize);

impl PartialEq<usize> for BackendCount {
    fn eq(&self, other: &usize) -> bool {
        self.0 == *other
    }
}

impl PartialOrd<usize> for BackendCount {
    fn partial_cmp(&self, other: &usize) -> Option<CmpOrdering> {
        self.0.partial_cmp(other)
    }
}

impl std::fmt::Display for BackendCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl BackendCount {
    /// Maximum backend count that fits the article availability bitmap.
    pub const MAX: usize = BackendId::MAX_COUNT;

    /// Zero backends
    #[must_use]
    pub const fn zero() -> Self {
        Self(0)
    }

    /// Construct a bounded backend count from a raw length.
    #[must_use]
    pub const fn try_new(count: usize) -> Option<Self> {
        if count <= Self::MAX {
            Some(Self(count))
        } else {
            None
        }
    }

    /// Get the inner usize value
    #[inline]
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }

    /// Iterate every valid backend ID in this bounded count.
    pub fn backend_ids(self) -> impl ExactSizeIterator<Item = BackendId> {
        (0..self.0).map(BackendId::from_index)
    }

    fn from_router_len(count: usize) -> Self {
        Self::try_new(count).expect("router backend count exceeds availability bitmap")
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
    const fn new(router: Arc<BackendSelector>, backend_id: BackendId) -> Self {
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
/// # use nntp_proxy::router::{BackendSelector, RouteRequest};
/// # use nntp_proxy::types::{ClientId, ServerName};
/// # use nntp_proxy::pool::DeadpoolConnectionProvider;
/// let mut selector = BackendSelector::new();
///
/// # let provider = DeadpoolConnectionProvider::new(
/// #     "localhost".to_string(), 119, "test".to_string(), 10, None, None
/// # );
/// selector.add_backend(
///     ServerName::try_new("backend-1".to_string()).unwrap(),
///     provider,
///     0, // tier (lower = higher priority)
/// );
///
/// // Route commands without article availability filtering
/// let backend = selector.route(RouteRequest::new(ClientId::new()))?;
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
    /// Capacity-fair probe counter for the first availability-aware article attempt.
    initial_article_probe_counter: AtomicUsize,
    /// Enable/disable per-connection queue-pressure filtering.
    queue_backpressure_enabled: bool,
    /// Soft threshold for queued requests per connection (percentage).
    queue_backpressure_soft_waiters_per_connection_percent: u16,
    /// Hard threshold for queued requests per connection (percentage).
    queue_backpressure_hard_waiters_per_connection_percent: u16,
    /// Delay to apply when all eligible backends in a tier are hard-saturated.
    queue_backpressure_all_busy_sleep_ms: u64,
}

impl Default for BackendSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl BackendSelector {
    const DEFAULT_QUEUE_BACKPRESSURE_ENABLED: bool = true;
    const DEFAULT_QUEUE_BACKPRESSURE_SOFT_WAITERS_PER_CONNECTION_PERCENT: u16 = 25;
    const DEFAULT_QUEUE_BACKPRESSURE_HARD_WAITERS_PER_CONNECTION_PERCENT: u16 = 50;
    const DEFAULT_QUEUE_BACKPRESSURE_ALL_BUSY_SLEEP_MS: u64 = 1;

    /// Find backend by ID
    ///
    /// Common helper to avoid repeating find logic across methods.
    #[inline]
    fn find_backend(&self, backend_id: BackendId) -> Option<&BackendInfo> {
        self.backends.iter().find(|b| b.id == backend_id)
    }

    /// Create a guard for a backend already reserved by `route()`.
    #[must_use]
    pub fn guard_for_routed_backend(router: Arc<Self>, backend_id: BackendId) -> CommandGuard {
        CommandGuard::new(router, backend_id)
    }

    /// Create a guard for manually selected backend work.
    ///
    /// This increments `pending_count` before creating the guard, so the drop path
    /// always has a matching decrement.
    #[must_use]
    pub fn guard_for_manual_backend(router: Arc<Self>, backend_id: BackendId) -> CommandGuard {
        router.mark_backend_pending(backend_id);
        CommandGuard::new(router, backend_id)
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
            initial_article_probe_counter: AtomicUsize::new(0),
            queue_backpressure_enabled: Self::DEFAULT_QUEUE_BACKPRESSURE_ENABLED,
            queue_backpressure_soft_waiters_per_connection_percent:
                Self::DEFAULT_QUEUE_BACKPRESSURE_SOFT_WAITERS_PER_CONNECTION_PERCENT,
            queue_backpressure_hard_waiters_per_connection_percent:
                Self::DEFAULT_QUEUE_BACKPRESSURE_HARD_WAITERS_PER_CONNECTION_PERCENT,
            queue_backpressure_all_busy_sleep_ms:
                Self::DEFAULT_QUEUE_BACKPRESSURE_ALL_BUSY_SLEEP_MS,
        }
    }

    /// Configure queue-pressure routing behavior.
    #[must_use]
    pub fn with_queue_backpressure(
        mut self,
        enabled: bool,
        soft_waiters_per_connection_percent: u16,
        hard_waiters_per_connection_percent: u16,
        all_busy_sleep_ms: u64,
    ) -> Self {
        self.queue_backpressure_enabled = enabled;
        self.queue_backpressure_soft_waiters_per_connection_percent =
            soft_waiters_per_connection_percent;
        self.queue_backpressure_hard_waiters_per_connection_percent =
            if hard_waiters_per_connection_percent < soft_waiters_per_connection_percent {
                soft_waiters_per_connection_percent
            } else {
                hard_waiters_per_connection_percent
            };
        self.queue_backpressure_all_busy_sleep_ms = all_busy_sleep_ms;
        self
    }

    /// Add a backend server to the router
    ///
    /// # Arguments
    /// * `name` - Human-readable name for logging
    /// * `provider` - Connection pool provider
    /// * `tier` - Server tier (lower = higher priority, 0 is highest)
    ///
    /// Returns the assigned `BackendId`. Each proxy port owns one router;
    /// production setup adds servers in config order, so backend IDs stay contiguous.
    pub fn add_backend(
        &mut self,
        name: ServerName,
        provider: DeadpoolConnectionProvider,
        tier: u8,
    ) -> BackendId {
        let backend_id = BackendId::from_index(self.backends.len());
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

        backend_id
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
        suppressed_backends: &SuppressedBackends,
    ) -> Option<SelectedBackend<'_>> {
        if self.backends.is_empty() {
            return None;
        }

        // Availability check closure
        let is_available = |backend: &&BackendInfo| {
            !suppressed_backends.contains(backend.id)
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

            let use_capacity_weighted_initial_probe =
                availability.is_some_and(|avail| !self.availability_missing_in_tier(avail, tier));

            let tier_has_non_over_hard_backend = !use_capacity_weighted_initial_probe
                && self.queue_backpressure_enabled
                && self.backends.iter().any(|backend| {
                    if backend.tier != tier || !is_available(&backend) {
                        return false;
                    }
                    let status = backend.provider.status_counts();
                    let checked_out = status.size.saturating_sub(status.available);
                    let pending = backend.pending_count.get();
                    !self.exceeds_hard_queue_depth(
                        pending,
                        checked_out,
                        status.waiting,
                        status.max_size,
                    )
                });

            // Try to select from this specific tier
            let tier_filter = |b: &&BackendInfo| {
                if b.tier != tier || !is_available(b) {
                    return false;
                }
                if !tier_has_non_over_hard_backend {
                    return true;
                }
                let status = b.provider.status_counts();
                let checked_out = status.size.saturating_sub(status.available);
                let pending = b.pending_count.get();
                !self.exceeds_hard_queue_depth(
                    pending,
                    checked_out,
                    status.waiting,
                    status.max_size,
                )
            };

            let selected = if use_capacity_weighted_initial_probe {
                self.select_capacity_weighted(tier_filter)
                    .map(|backend| SelectedBackend {
                        backend,
                        pending_snapshot: None,
                    })
            } else {
                self.select_weighted(tier_filter)
            };
            if tracing::enabled!(tracing::Level::DEBUG) {
                self.debug_log_selection_candidates(
                    tier,
                    availability,
                    selected.as_ref().map(|selected| selected.backend.id),
                );
            }

            if let Some(selected) = selected {
                tracing::debug!(
                    backend_id = selected.backend.id.as_index(),
                    backend_name = selected.backend.name.as_str(),
                    tier = tier,
                    "Selected backend"
                );
                return Some(selected);
            }

            if availability.is_some()
                && self.backends.iter().any(|backend| {
                    backend.tier == tier
                        && availability.is_none_or(|avail| avail.should_try(backend.id))
                })
            {
                tracing::debug!(
                    tier = tier,
                    suppressed_backends = format_args!("{:08b}", suppressed_backends.bits()),
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

    fn availability_missing_in_tier(&self, availability: &ArticleAvailability, tier: u8) -> bool {
        self.backends.iter().any(|backend| {
            backend.tier == tier && availability.missing_bits() & backend.id.availability_bit() != 0
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
        let availability_missing_bits = availability.map_or(0, ArticleAvailability::missing_bits);

        for backend in self.backends.iter().filter(|backend| backend.tier == tier) {
            let status = backend.provider.status_counts();
            let checked_out = status.size.saturating_sub(status.available);
            let pending = backend.pending_count.get();
            let queue_depth = Self::queue_depth_for_score(pending, checked_out, status.waiting);
            let active_for_score = Self::active_for_score(pending, checked_out, status.waiting);
            let load_ratio = if status.max_size > 0 {
                active_for_score as f64 / status.max_size as f64
            } else {
                f64::MAX
            };

            tracing::trace!(
                backend_id = backend.id.as_index(),
                backend_name = backend.name.as_str(),
                pool = %backend.provider.name(),
                tier,
                selected = selected_backend == Some(backend.id),
                should_try = availability.is_none_or(|avail| avail.should_try(backend.id)),
                availability_missing_bits,
                pending,
                checked_out,
                queue_depth,
                over_hard_backpressure = self.exceeds_hard_queue_depth(
                    pending,
                    checked_out,
                    status.waiting,
                    status.max_size
                ),
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

    /// Returns a short delay when the highest-priority eligible tier is fully hard-saturated.
    #[must_use]
    pub fn queue_backpressure_delay_for_article(
        &self,
        availability: &ArticleAvailability,
        suppressed_backends: SuppressedBackends,
    ) -> Option<Duration> {
        if !self.queue_backpressure_enabled || self.queue_backpressure_all_busy_sleep_ms == 0 {
            return None;
        }

        for &tier in &self.sorted_tiers {
            let mut has_candidate = false;
            let mut has_non_over_hard = false;
            for backend in self.backends.iter().filter(|backend| backend.tier == tier) {
                if suppressed_backends.contains(backend.id) || !availability.should_try(backend.id)
                {
                    continue;
                }
                has_candidate = true;
                let status = backend.provider.status_counts();
                let checked_out = status.size.saturating_sub(status.available);
                let pending = backend.pending_count.get();
                if !self.exceeds_hard_queue_depth(
                    pending,
                    checked_out,
                    status.waiting,
                    status.max_size,
                ) {
                    has_non_over_hard = true;
                    break;
                }
            }

            if has_candidate {
                if has_non_over_hard {
                    return None;
                }
                return Some(Duration::from_millis(
                    self.queue_backpressure_all_busy_sleep_ms,
                ));
            }
        }

        None
    }

    #[inline]
    fn queue_soft_pressure_penalty(
        &self,
        pending: usize,
        checked_out: usize,
        waiting: usize,
        max_connections: usize,
    ) -> usize {
        if !self.queue_backpressure_enabled || max_connections == 0 {
            return 0;
        }
        let queue_depth = Self::queue_depth_for_score(pending, checked_out, waiting);
        let soft_limit = Self::queue_depth_limit(
            max_connections,
            self.queue_backpressure_soft_waiters_per_connection_percent,
        );
        queue_depth.saturating_sub(soft_limit)
    }

    #[inline]
    fn exceeds_hard_queue_depth(
        &self,
        pending: usize,
        checked_out: usize,
        waiting: usize,
        max_connections: usize,
    ) -> bool {
        if !self.queue_backpressure_enabled || max_connections == 0 {
            return false;
        }
        let queue_depth = Self::queue_depth_for_score(pending, checked_out, waiting);
        let hard_limit = Self::queue_depth_limit(
            max_connections,
            self.queue_backpressure_hard_waiters_per_connection_percent,
        );
        queue_depth > hard_limit
    }

    #[inline]
    fn queue_depth_limit(max_connections: usize, per_connection_percent: u16) -> usize {
        let product = max_connections.saturating_mul(usize::from(per_connection_percent));
        product.div_ceil(100)
    }

    #[inline]
    const fn queue_depth_for_score(pending: usize, checked_out: usize, waiting: usize) -> usize {
        Self::active_for_score(pending, checked_out, waiting).saturating_sub(checked_out)
    }

    #[allow(clippy::cast_precision_loss)]
    fn backend_load_ratio_with_pending(&self, backend: &BackendInfo, pending: usize) -> LoadRatio {
        let status = backend.provider.status_counts();
        let max_conns = status.max_size as f64;
        if max_conns > 0.0 {
            let checked_out = status.size.saturating_sub(status.available);
            let queue_penalty = self.queue_soft_pressure_penalty(
                pending,
                checked_out,
                status.waiting,
                status.max_size,
            );
            let waiting_for_score = status.waiting.saturating_add(queue_penalty);
            let active = Self::active_for_score(pending, checked_out, waiting_for_score) as f64;
            LoadRatio::new(active / max_conns)
        } else {
            LoadRatio::MAX
        }
    }

    #[inline]
    const fn active_for_score(pending: usize, checked_out: usize, waiting: usize) -> usize {
        let checked_out_with_waiters = checked_out.saturating_add(waiting);
        if pending > checked_out_with_waiters {
            pending
        } else {
            checked_out_with_waiters
        }
    }

    /// Select a backend using weighted round-robin from backends matching the filter
    fn select_weighted<F>(&self, filter: F) -> Option<SelectedBackend<'_>>
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
                    .map(|backend| SelectedBackend {
                        backend,
                        pending_snapshot: None,
                    })
            }
            SelectionStrategy::LeastLoaded(least_loaded) => {
                let mut selected: Option<SelectedBackend<'_>> = None;
                let mut selected_load = LoadRatio::MAX;
                let mut ties = 0usize;

                for backend in self.backends.iter().filter(&filter) {
                    let pending_snapshot = backend.pending_count.get();
                    let load = self.backend_load_ratio_with_pending(backend, pending_snapshot);
                    match load
                        .partial_cmp(&selected_load)
                        .unwrap_or(std::cmp::Ordering::Greater)
                    {
                        std::cmp::Ordering::Less => {
                            selected = Some(SelectedBackend {
                                backend,
                                pending_snapshot: Some(pending_snapshot),
                            });
                            selected_load = load;
                            ties = 1;
                        }
                        std::cmp::Ordering::Equal => {
                            ties += 1;
                            if least_loaded.should_replace_tie(ties) {
                                selected = Some(SelectedBackend {
                                    backend,
                                    pending_snapshot: Some(pending_snapshot),
                                });
                            }
                        }
                        std::cmp::Ordering::Greater => {}
                    }
                }

                selected
            }
        }
    }

    /// Select a backend for a client request.
    ///
    /// # Errors
    /// Returns an error when no backend remains eligible.
    pub fn route<Mode: RouteMode>(&self, request: RouteRequest<'_, Mode>) -> Result<Mode::Output> {
        self.route_selected_backend(&request)
    }

    fn route_selected_backend<Mode: RouteMode>(
        &self,
        request: &RouteRequest<'_, Mode>,
    ) -> Result<Mode::Output> {
        loop {
            let availability = Mode::availability(request);
            let selected = self
                .select_backend(availability, &request.suppressed_backends)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "No backends available for routing (total backends: {})",
                        self.backends.len()
                    )
                })?;
            let backend = selected.backend;
            let output = Mode::output(request, backend.id)?;

            let reserved = if let Some(observed) = selected.pending_snapshot {
                backend.pending_count.try_increment_from(observed)
            } else {
                backend.pending_count.increment();
                true
            };
            if !reserved {
                continue;
            }

            debug!(
                "Selected backend {:?} ({}) for command",
                backend.id, backend.name
            );

            return Ok(output);
        }
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
        BackendCount::from_router_len(self.backends.len())
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

    fn suppressed(backends: &[BackendId]) -> SuppressedBackends {
        let mut suppressed = SuppressedBackends::empty();
        for backend in backends {
            suppressed.suppress(*backend);
        }
        suppressed
    }

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
            ServerName::try_new("test-server".to_string()).unwrap(),
            provider,
            0,
        );
        (Arc::new(selector), backend_id)
    }

    #[test]
    fn backend_count_iterates_only_constructible_backend_ids() {
        let count = BackendCount::try_new(3).expect("count fits availability bitmap");
        let ids: Vec<_> = count.backend_ids().collect();

        assert_eq!(
            ids,
            vec![
                BackendId::from_index(0),
                BackendId::from_index(1),
                BackendId::from_index(2)
            ]
        );
        assert_eq!(BackendCount::try_new(BackendCount::MAX + 1), None);
    }

    #[test]
    fn command_guard_decrements_on_drop() {
        let (router, backend_id) = make_router_with_backend();

        // Manual guard increments on creation and decrements on drop.
        assert_eq!(router.backend_load(backend_id).unwrap().get(), 0);
        {
            let _guard = BackendSelector::guard_for_manual_backend(router.clone(), backend_id);
            assert_eq!(router.backend_load(backend_id).unwrap().get(), 1);
        }
        assert_eq!(router.backend_load(backend_id).unwrap().get(), 0);
    }

    #[test]
    fn command_guard_explicit_complete() {
        let (router, backend_id) = make_router_with_backend();

        let guard = BackendSelector::guard_for_manual_backend(router.clone(), backend_id);
        assert_eq!(router.backend_load(backend_id).unwrap().get(), 1);
        guard.complete();
        assert_eq!(router.backend_load(backend_id).unwrap().get(), 0);
    }

    #[test]
    fn command_guard_no_double_decrement() {
        let (router, backend_id) = make_router_with_backend();

        // Explicit complete + drop should only decrement once
        let guard = BackendSelector::guard_for_manual_backend(router.clone(), backend_id);
        assert_eq!(router.backend_load(backend_id).unwrap().get(), 1);
        guard.complete();
        // After complete(), count should be 0; drop should be a no-op
        assert_eq!(router.backend_load(backend_id).unwrap().get(), 0);
        // If double-decrement happened, we'd see wrapping (very large number)
        // Since we're at 0 already and drop is a no-op, this confirms correctness
    }

    #[test]
    fn command_guard_backend_id_accessor() {
        let (router, backend_id) = make_router_with_backend();
        let guard = BackendSelector::guard_for_routed_backend(router, backend_id);
        assert_eq!(guard.backend_id(), backend_id);
    }

    #[test]
    fn transient_suppression_can_try_same_tier_without_escalating() {
        let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);
        for (name, tier) in [("tier0-a", 0), ("tier0-b", 0), ("tier1", 1)] {
            selector.add_backend(
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
            .route(
                RouteRequest::new(ClientId::new())
                    .with_availability(&availability)
                    .suppressing_backends(suppressed(&[BackendId::from_index(0)])),
            )
            .unwrap();

        assert_eq!(backend.backend_id(), BackendId::from_index(1));

        let exhausted_tier0 = selector.route(
            RouteRequest::new(ClientId::new())
                .with_availability(&availability)
                .suppressing_backends(suppressed(&[
                    BackendId::from_index(0),
                    BackendId::from_index(1),
                ])),
        );
        assert!(
            exhausted_tier0.is_err(),
            "transient backend failures must not escalate to tier 1 before tier 0 has all 430s"
        );
    }

    #[test]
    fn first_probe_selection_is_tier_local() {
        let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);
        for (name, tier, max_connections) in [
            ("tier0", 0, 10),
            ("tier1-small", 1, 1),
            ("tier1-large", 1, 10),
        ] {
            selector.add_backend(
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
            .route(RouteRequest::new(ClientId::new()).with_availability(&availability))
            .unwrap();

        assert_eq!(
            backend.backend_id(),
            BackendId::from_index(2),
            "least-loaded must honor load on the first probe in a newly eligible tier"
        );
    }

    #[test]
    fn active_for_score_accounts_for_pool_waiters() {
        assert_eq!(BackendSelector::active_for_score(5, 40, 10), 50);
        assert_eq!(BackendSelector::active_for_score(80, 40, 10), 80);
        assert_eq!(
            BackendSelector::active_for_score(0, usize::MAX, 1),
            usize::MAX
        );
    }

    #[test]
    fn queue_depth_limit_scales_with_connection_count() {
        assert_eq!(BackendSelector::queue_depth_limit(40, 25), 10);
        assert_eq!(BackendSelector::queue_depth_limit(50, 25), 13);
        assert_eq!(BackendSelector::queue_depth_limit(40, 50), 20);
        assert_eq!(BackendSelector::queue_depth_limit(50, 50), 25);
    }

    #[test]
    fn queue_depth_for_score_uses_pending_when_it_exceeds_pool_activity() {
        assert_eq!(BackendSelector::queue_depth_for_score(20, 5, 2), 15);
        assert_eq!(BackendSelector::queue_depth_for_score(3, 5, 2), 2);
    }

    #[test]
    fn availability_routing_honors_least_loaded_when_no_missing_known() {
        let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);
        for (name, max_connections) in [("tier0-a", 40), ("tier0-b", 50)] {
            selector.add_backend(
                ServerName::try_new(name.to_string()).unwrap(),
                crate::pool::DeadpoolConnectionProvider::new(
                    "localhost".to_string(),
                    119,
                    name.to_string(),
                    max_connections,
                    None,
                    None,
                ),
                0,
            );
        }

        // Simulate one backend already saturated while the other has headroom.
        for _ in 0..75 {
            selector.mark_backend_pending(BackendId::from_index(1));
        }
        for _ in 0..5 {
            selector.mark_backend_pending(BackendId::from_index(0));
        }

        let availability = ArticleAvailability::new();
        let backend = selector
            .route(RouteRequest::new(ClientId::new()).with_availability(&availability))
            .unwrap();

        assert_eq!(
            backend.backend_id(),
            BackendId::from_index(0),
            "availability routing should not bypass least-loaded when no misses are known"
        );
    }

    #[test]
    fn transient_suppression_does_not_block_escalation_after_real_430s() {
        let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);
        for (name, tier) in [("tier0-a", 0), ("tier0-b", 0), ("tier1", 1)] {
            selector.add_backend(
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
            .route(
                RouteRequest::new(ClientId::new())
                    .with_availability(&availability)
                    .suppressing_backends(suppressed(&[BackendId::from_index(0)])),
            )
            .unwrap();

        assert_eq!(
            backend.backend_id(),
            BackendId::from_index(2),
            "tier escalation is allowed after tier 0 has authoritative 430s"
        );
    }

    #[test]
    fn zero_suppression_keeps_large_backend_routing_off_the_availability_bitset() {
        let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);
        for index in 0..12 {
            selector.add_backend(
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
            .route(crate::router::RouteRequest::new(ClientId::new()))
            .unwrap();

        assert!(
            backend.as_index() < 12,
            "routing without suppression must not touch the 8-backend availability bitset"
        );
    }

    #[test]
    fn transient_suppression_handles_backend_index_at_legacy_u8_boundary() {
        let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);
        for index in 0..10 {
            selector.add_backend(
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

        let mut suppressed = SuppressedBackends::empty();
        suppressed.suppress(BackendId::from_index(8));
        for index in 0..8 {
            selector.mark_backend_pending(BackendId::from_index(index));
        }
        selector.mark_backend_pending(BackendId::from_index(9));

        let selected = selector
            .route(RouteRequest::new(ClientId::new()).suppressing_backends(suppressed))
            .unwrap();

        assert_ne!(selected, BackendId::from_index(8));
    }
}
