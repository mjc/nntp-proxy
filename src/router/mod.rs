//! Backend server selection and load balancing
//!
//! [`BackendSelector`] selects backends with weighted round-robin or
//! least-loaded routing. It supports concurrent requests and tracks pending
//! work and connection capacity. Article requests can carry recorded provider
//! misses, and queue-pressure limits exclude saturated backends while
//! alternatives remain.
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

use crate::cache::{ArticleAvailability, AvailabilityMask, AvailabilitySlot};
use crate::config::{BackendSelectionStrategy, LowerTierStatPrefetch, QueuePressureLimits};
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
    /// Backends this request may never use, unlike transient suppressions.
    /// A tier made up only of excluded backends falls through to the next tier.
    excluded_backends: SuppressedBackends,
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

    /// Every backend but `backend_id`, to route one request to that backend
    /// alone while keeping the router's availability and tier checks.
    #[must_use]
    pub fn all_except(backend_id: BackendId) -> Self {
        Self {
            bits: !backend_id.availability_bit(),
        }
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

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            bits: self.bits | other.bits,
        }
    }
}

/// Routing without article availability state.
#[derive(Debug, Clone, Copy)]
pub struct RawRoute;

/// Routing for article commands with availability state.
#[derive(Debug, Clone, Copy)]
pub struct ArticleRoute<'a> {
    availability: &'a ArticleAvailability,
    lookup: RouteLookup,
}

/// What an availability-routed request asks for. Per-command routing sends
/// list and other non-article commands through the same availability path,
/// so having availability does not make a request an article lookup.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RouteLookup {
    /// Any other command; routed by capacity and load only.
    #[default]
    Other,
    /// `ARTICLE`, `BODY`, `HEAD`, or `STAT` by message-id; weighed by
    /// article standing when `[routing.article] standing = "adaptive"`.
    Article,
}

/// Backend selected through article availability routing.
///
/// Article execution accepts this type instead of raw `BackendId`, so a caller
/// must route with current `ArticleAvailability` before it can issue an article
/// request to a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArticleBackend {
    backend_id: BackendId,
    availability_slot: AvailabilitySlot,
}

impl ArticleBackend {
    #[inline]
    #[must_use]
    #[cfg(test)]
    pub(crate) fn from_availability(
        backend_id: BackendId,
        availability: &ArticleAvailability,
    ) -> Option<Self> {
        let availability_slot = AvailabilitySlot::new(backend_id.as_index())?;
        Self::from_availability_slot(backend_id, availability_slot, availability)
    }

    #[inline]
    #[must_use]
    pub(crate) fn from_availability_slot(
        backend_id: BackendId,
        availability_slot: AvailabilitySlot,
        availability: &ArticleAvailability,
    ) -> Option<Self> {
        availability
            .should_try_slot(availability_slot)
            .then_some(Self {
                backend_id,
                availability_slot,
            })
    }

    #[inline]
    #[must_use]
    pub const fn backend_id(self) -> BackendId {
        self.backend_id
    }

    #[inline]
    #[must_use]
    pub(crate) const fn availability_slot(self) -> AvailabilitySlot {
        self.availability_slot
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
            excluded_backends: SuppressedBackends::empty(),
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
            excluded_backends: self.excluded_backends,
            mode: ArticleRoute {
                availability,
                lookup: RouteLookup::Other,
            },
            _lifetime: std::marker::PhantomData,
        }
    }

    #[must_use]
    pub fn suppressing_backends(mut self, suppressed_backends: SuppressedBackends) -> Self {
        self.suppressed_backends = suppressed_backends;
        self
    }

    #[must_use]
    pub const fn excluding_backends(mut self, excluded_backends: SuppressedBackends) -> Self {
        self.excluded_backends = excluded_backends;
        self
    }
}

impl RouteRequest<'_, ArticleRoute<'_>> {
    /// Mark this request as an article lookup so article standing applies.
    #[must_use]
    pub const fn for_lookup(mut self, lookup: RouteLookup) -> Self {
        self.mode.lookup = lookup;
        self
    }

    #[must_use]
    pub fn suppressing_backends(mut self, suppressed_backends: SuppressedBackends) -> Self {
        self.suppressed_backends = suppressed_backends;
        self
    }

    #[must_use]
    pub const fn excluding_backends(mut self, excluded_backends: SuppressedBackends) -> Self {
        self.excluded_backends = excluded_backends;
        self
    }
}

#[doc(hidden)]
pub trait RouteMode: route_mode::Sealed {
    type Output;

    fn availability<'r>(request: &'r RouteRequest<'_, Self>) -> Option<&'r ArticleAvailability>
    where
        Self: Sized;

    fn lookup(request: &RouteRequest<'_, Self>) -> RouteLookup
    where
        Self: Sized;

    fn output(request: &RouteRequest<'_, Self>, backend: &BackendInfo) -> Result<Self::Output>
    where
        Self: Sized;
}

impl RouteMode for RawRoute {
    type Output = BackendId;

    fn availability<'r>(_request: &'r RouteRequest<'_, Self>) -> Option<&'r ArticleAvailability> {
        None
    }

    fn lookup(_request: &RouteRequest<'_, Self>) -> RouteLookup {
        RouteLookup::Other
    }

    fn output(_request: &RouteRequest<'_, Self>, backend: &BackendInfo) -> Result<Self::Output> {
        Ok(backend.id)
    }
}

impl route_mode::Sealed for RawRoute {}

impl RouteMode for ArticleRoute<'_> {
    type Output = ArticleBackend;

    fn availability<'r>(request: &'r RouteRequest<'_, Self>) -> Option<&'r ArticleAvailability> {
        Some(request.mode.availability)
    }

    fn lookup(request: &RouteRequest<'_, Self>) -> RouteLookup {
        request.mode.lookup
    }

    fn output(request: &RouteRequest<'_, Self>, backend: &BackendInfo) -> Result<Self::Output> {
        ArticleBackend::from_availability_slot(
            backend.id,
            backend.availability_slot,
            request.mode.availability,
        )
        .ok_or_else(|| {
            anyhow::anyhow!(
                "selected backend {} is no longer eligible for article routing",
                backend.id.as_index()
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
/// How backends in one tier are weighed against each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionWeighting {
    /// By pool capacity alone.
    Capacity,
    /// By pool capacity scaled by recent article standing.
    Standing,
}

/// One way of weighing backends, chosen once per route so the selection
/// loops are compiled separately for each and the capacity path stays as it
/// was.
trait Weighing {
    /// Share weight for weighted round-robin, in connections.
    fn weight(backend: &BackendInfo) -> usize;
    /// Least-loaded score from active requests and pool size; lower wins.
    fn load(backend: &BackendInfo, active: f64, max_conns: f64) -> LoadRatio;
}

/// Weigh by pool capacity alone.
struct ByCapacity;

impl Weighing for ByCapacity {
    #[inline]
    fn weight(backend: &BackendInfo) -> usize {
        backend.provider.max_size()
    }

    #[inline]
    fn load(_backend: &BackendInfo, active: f64, max_conns: f64) -> LoadRatio {
        LoadRatio::new(active / max_conns)
    }
}

/// Weigh by pool capacity scaled by article standing, never below
/// [`crate::pool::admission::STANDING_FLOOR_PPM`].
struct ByStanding;

impl ByStanding {
    fn standing_ppm(backend: &BackendInfo) -> u32 {
        backend
            .provider
            .article_standing_ppm()
            .max(crate::pool::admission::STANDING_FLOOR_PPM)
    }
}

impl Weighing for ByStanding {
    /// Rounds up so a demoted pool keeps at least one slot, and stays in
    /// whole connections because the round-robin position advances by one
    /// per request: finer units would lengthen the cycle and hand each
    /// backend its share in long bursts.
    fn weight(backend: &BackendInfo) -> usize {
        let capacity = backend.provider.max_size();
        let scaled = (capacity as u64)
            .saturating_mul(u64::from(Self::standing_ppm(backend)))
            .div_ceil(u64::from(crate::pool::admission::STANDING_FULL_PPM));
        usize::try_from(scaled).unwrap_or(usize::MAX)
    }

    /// Counting the request being placed matters here: fast 430s keep a
    /// demoted backend idle, and an idle backend's load is zero whatever its
    /// standing. With the extra request a backend at standing `s` looks as
    /// busy as one with `1/s` times the load, so it only wins once its peers
    /// fill up.
    fn load(backend: &BackendInfo, active: f64, max_conns: f64) -> LoadRatio {
        let standing = f64::from(Self::standing_ppm(backend))
            / f64::from(crate::pool::admission::STANDING_FULL_PPM);
        LoadRatio::new((active + 1.0) / (max_conns * standing))
    }
}

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
    /// Bitmap of distinct configured article-availability identities.
    availability_mask: AvailabilityMask,
    /// Enable/disable per-connection queue-pressure filtering.
    queue_backpressure_enabled: bool,
    /// Validated soft and hard thresholds for queued requests per connection.
    queue_backpressure_limits: QueuePressureLimits,
    /// Delay to apply when all eligible backends in a tier are hard-saturated.
    queue_backpressure_all_busy_sleep_ms: u64,
    /// Backends configured with `stateful_sessions = "excluded"`.
    stateful_exclusions: SuppressedBackends,
    /// Deadline, checkout wait, and backlog skipping for article requests.
    article_admission: crate::pool::admission::ArticleAdmission,
    /// When ARTICLE/BODY requests probe lower tiers with background STATs.
    lower_tier_stat_prefetch: LowerTierStatPrefetch,
    /// Recent unfinished lookups per message-id, for
    /// `unfinished_replies_before_430`.
    unfinished_lookups: crate::pool::admission::UnfinishedLookups,
}

impl Default for BackendSelector {
    fn default() -> Self {
        Self::new()
    }
}

impl BackendSelector {
    const DEFAULT_QUEUE_BACKPRESSURE_ENABLED: bool = true;
    const DEFAULT_QUEUE_BACKPRESSURE_ALL_BUSY_SLEEP_MS: u64 = 1;

    /// Find backend by ID
    ///
    /// Common helper to avoid repeating find logic across methods.
    #[inline]
    fn find_backend(&self, backend_id: BackendId) -> Option<&BackendInfo> {
        self.backends.iter().find(|b| b.id == backend_id)
    }

    #[inline]
    pub(crate) fn availability_slot(&self, backend_id: BackendId) -> Option<AvailabilitySlot> {
        self.find_backend(backend_id)
            .map(|backend| backend.availability_slot)
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
            availability_mask: AvailabilityMask::empty(),
            queue_backpressure_enabled: Self::DEFAULT_QUEUE_BACKPRESSURE_ENABLED,
            queue_backpressure_limits: QueuePressureLimits::default(),
            queue_backpressure_all_busy_sleep_ms:
                Self::DEFAULT_QUEUE_BACKPRESSURE_ALL_BUSY_SLEEP_MS,
            stateful_exclusions: SuppressedBackends::empty(),
            article_admission: crate::pool::admission::ArticleAdmission::default(),
            lower_tier_stat_prefetch: LowerTierStatPrefetch::default(),
            unfinished_lookups: crate::pool::admission::UnfinishedLookups::new(),
        }
    }

    /// Configure article request deadlines and backlog skipping.
    #[must_use]
    pub const fn with_article_admission(
        mut self,
        admission: crate::pool::admission::ArticleAdmission,
    ) -> Self {
        self.article_admission = admission;
        self
    }

    /// Reply for a lookup that ended before every eligible backend returned
    /// `430`. Answers `430` once the message-id has already had
    /// `unfinished_replies_before_430` such replies and at least one backend
    /// has returned `430` for it (now or as a cached fact). Backends that
    /// were busy, skipped, failed, or refused with another error say nothing
    /// about the article either way.
    #[must_use]
    pub fn unfinished_lookup_reply(
        &self,
        message_id: Option<&str>,
        availability: &crate::cache::ArticleAvailability,
        backlogged: SuppressedBackends,
    ) -> crate::pool::admission::UnavailableReply {
        use crate::pool::admission::UnavailableReply;
        let admission = self.article_admission;
        let reply = admission.unavailable_reply();
        let limit = admission.unfinished_replies_before_430();
        if reply == UnavailableReply::NoSuchArticle || availability.missing_bits() == 0 {
            return reply;
        }
        if self.only_unlikely_rescuers_unconfirmed(availability, backlogged) {
            return UnavailableReply::NoSuchArticle;
        }
        if limit == 0 {
            return reply;
        }
        let Some(message_id) = message_id else {
            return reply;
        };
        if self.unfinished_lookups.record(message_id) > limit {
            UnavailableReply::NoSuchArticle
        } else {
            reply
        }
    }

    /// Whether every backend that has not answered `430` for this article was
    /// skipped as backlogged by this request and has a recent rescue rate
    /// under the configured floor. Waiting for such a backend to confirm the
    /// miss is what makes downloads of articles gone everywhere slow, and it
    /// would almost never have them. A backend that failed, refused, or
    /// timed out rather than being busy keeps the normal `503` handling.
    fn only_unlikely_rescuers_unconfirmed(
        &self,
        availability: &crate::cache::ArticleAvailability,
        backlogged: SuppressedBackends,
    ) -> bool {
        let admission = self.article_admission;
        let mut unconfirmed = false;
        for backend in &self.backends {
            if !availability.should_try_slot(backend.availability_slot) {
                continue;
            }
            unconfirmed = true;
            if !backlogged.contains(backend.id)
                || !backend.provider.is_unlikely_to_rescue(&admission)
            {
                return false;
            }
        }
        unconfirmed
    }

    /// Deadline, checkout wait, and backlog skipping for article requests.
    #[must_use]
    pub const fn article_admission(&self) -> crate::pool::admission::ArticleAdmission {
        self.article_admission
    }

    /// Configure background lower-tier STAT probes for ARTICLE/BODY requests.
    #[must_use]
    pub const fn with_lower_tier_stat_prefetch(mut self, policy: LowerTierStatPrefetch) -> Self {
        self.lower_tier_stat_prefetch = policy;
        self
    }

    #[inline]
    pub(crate) const fn lower_tier_stat_prefetch(&self) -> LowerTierStatPrefetch {
        self.lower_tier_stat_prefetch
    }

    /// Whether the availability facts already show a tier-0 backend returning `430`.
    pub(crate) fn has_primary_tier_miss(&self, availability: &ArticleAvailability) -> bool {
        self.backend_ids_in_tier(0).any(|backend_id| {
            self.availability_slot(backend_id)
                .is_some_and(|slot| availability.is_missing_slot(slot))
        })
    }

    /// Configure queue-pressure routing behavior.
    #[must_use]
    pub fn with_queue_backpressure(
        mut self,
        enabled: bool,
        limits: QueuePressureLimits,
        all_busy_sleep_ms: u64,
    ) -> Self {
        self.queue_backpressure_enabled = enabled;
        self.queue_backpressure_limits = limits;
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
        let slot = AvailabilitySlot::new(self.backends.len()).expect("backend count fits bitmap");
        self.add_backend_with_slot(name, provider, tier, slot)
    }

    pub(crate) fn add_backend_with_slot(
        &mut self,
        name: ServerName,
        provider: DeadpoolConnectionProvider,
        tier: u8,
        availability_slot: AvailabilitySlot,
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
            availability_slot,
            name,
            provider,
            pending_count: PendingCount::new(),
            stateful_count: StatefulCount::new(),
            tier,
        });
        self.availability_mask.insert(availability_slot);

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
        lookup: RouteLookup,
        suppressed_backends: &SuppressedBackends,
        excluded_backends: &SuppressedBackends,
    ) -> Option<SelectedBackend<'_>> {
        match self.selection_weighting(lookup) {
            SelectionWeighting::Capacity => self.select_backend_weighed::<ByCapacity>(
                availability,
                suppressed_backends,
                excluded_backends,
            ),
            SelectionWeighting::Standing => self.select_backend_weighed::<ByStanding>(
                availability,
                suppressed_backends,
                excluded_backends,
            ),
        }
    }

    fn select_backend_weighed<W: Weighing>(
        &self,
        availability: Option<&ArticleAvailability>,
        suppressed_backends: &SuppressedBackends,
        excluded_backends: &SuppressedBackends,
    ) -> Option<SelectedBackend<'_>> {
        if self.backends.is_empty() {
            return None;
        }

        // Availability check closure
        let is_available = |backend: &&BackendInfo| {
            !suppressed_backends.contains(backend.id)
                && !excluded_backends.contains(backend.id)
                && availability.is_none_or(|avail| avail.should_try_slot(backend.availability_slot))
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

            let tier_has_non_over_hard_backend = self.queue_backpressure_enabled
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

            let selected = if availability
                .is_some_and(|avail| !self.availability_missing_in_tier(avail, tier))
            {
                match &self.strategy {
                    SelectionStrategy::WeightedRoundRobin(_) => self
                        .select_capacity_weighted::<_, W>(tier_filter)
                        .map(|backend| SelectedBackend {
                            backend,
                            pending_snapshot: None,
                        }),
                    SelectionStrategy::LeastLoaded(_) => self.select_weighted::<_, W>(tier_filter),
                }
            } else {
                self.select_weighted::<_, W>(tier_filter)
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
                        && !excluded_backends.contains(backend.id)
                        && availability
                            .is_none_or(|avail| avail.should_try_slot(backend.availability_slot))
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
            backend.tier == tier && availability.is_missing_slot(backend.availability_slot)
        })
    }

    #[inline]
    pub(crate) const fn availability_mask(&self) -> AvailabilityMask {
        self.availability_mask
    }

    /// Select a backend by capacity weight, independent of current pending load.
    fn select_capacity_weighted<F, W: Weighing>(&self, filter: F) -> Option<&BackendInfo>
    where
        F: Fn(&&BackendInfo) -> bool,
    {
        let total_weight: usize = self.backends.iter().filter(&filter).map(W::weight).sum();

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
                *cumulative += W::weight(backend);
                Some((*cumulative, backend))
            })
            .find(|(cumulative_weight, _)| position < *cumulative_weight)
            .map(|(_, backend)| backend)
            .or_else(|| self.backends.iter().find(&filter))
    }

    /// Article lookups weigh backends by standing when it is turned on;
    /// everything else (group, overview, list commands) only by capacity.
    fn selection_weighting(&self, lookup: RouteLookup) -> SelectionWeighting {
        match (lookup, self.article_admission.standing()) {
            (RouteLookup::Article, crate::pool::admission::ArticleStanding::Adaptive) => {
                SelectionWeighting::Standing
            }
            _ => SelectionWeighting::Capacity,
        }
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
                should_try = availability.is_none_or(|avail| avail.should_try_slot(backend.availability_slot)),
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
                if suppressed_backends.contains(backend.id)
                    || !availability.should_try_slot(backend.availability_slot)
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
        let soft_limit =
            Self::queue_depth_limit(max_connections, self.queue_backpressure_limits.soft());
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
        let hard_limit =
            Self::queue_depth_limit(max_connections, self.queue_backpressure_limits.hard());
        queue_depth > hard_limit
    }

    #[inline]
    fn queue_depth_limit(
        max_connections: usize,
        per_connection_percent: crate::types::QueuePressurePercent,
    ) -> usize {
        let product = max_connections.saturating_mul(usize::from(per_connection_percent.get()));
        product.div_ceil(100)
    }

    #[inline]
    const fn queue_depth_for_score(pending: usize, checked_out: usize, waiting: usize) -> usize {
        Self::active_for_score(pending, checked_out, waiting).saturating_sub(checked_out)
    }

    #[allow(clippy::cast_precision_loss)]
    fn backend_load_ratio_with_pending<W: Weighing>(
        &self,
        backend: &BackendInfo,
        pending: usize,
    ) -> LoadRatio {
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
            W::load(backend, active, max_conns)
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
    fn select_weighted<F, W: Weighing>(&self, filter: F) -> Option<SelectedBackend<'_>>
    where
        F: Fn(&&BackendInfo) -> bool,
    {
        match &self.strategy {
            SelectionStrategy::WeightedRoundRobin(wrr) => {
                // Sum weights for backends passing filter
                let total_weight: usize = self.backends.iter().filter(&filter).map(W::weight).sum();

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
                        *cumulative += W::weight(backend);
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
                    let load = self.backend_load_ratio_with_pending::<W>(backend, pending_snapshot);
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
                .select_backend(
                    availability,
                    Mode::lookup(request),
                    &request.suppressed_backends,
                    &request.excluded_backends,
                )
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "No backends available for routing (total backends: {})",
                        self.backends.len()
                    )
                })?;
            let backend = selected.backend;
            let output = Mode::output(request, backend)?;

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

    /// Never pin a stateful session to this backend.
    ///
    /// The backend keeps serving message-ID requests. See
    /// [`crate::config::StatefulSessions`] for why indexers need this.
    pub fn exclude_from_stateful_sessions(&mut self, backend_id: BackendId) {
        self.stateful_exclusions.suppress(backend_id);
    }

    /// Backends that must not answer requests whose replies can carry article
    /// numbers, which is every request not addressed by message-ID.
    #[must_use]
    pub const fn stateful_exclusions(&self) -> SuppressedBackends {
        self.stateful_exclusions
    }

    /// Route a client that is about to hold a backend for stateful commands.
    ///
    /// Excluded backends are skipped, and a tier made up only of excluded
    /// backends falls through to the next tier.
    ///
    /// # Errors
    /// Returns an error when every backend is excluded or unavailable.
    pub fn route_stateful_session(&self, client_id: ClientId) -> Result<BackendId> {
        self.route(RouteRequest::new(client_id).excluding_backends(self.stateful_exclusions))
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
        availability.record_missing_slot(crate::cache::AvailabilitySlot::new(0).unwrap());
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
    fn article_routing_shares_negative_slot_between_transport_backends() {
        let mut selector = BackendSelector::new();
        for name in ["plain", "tls"] {
            selector.add_backend_with_slot(
                ServerName::try_new(name.to_string()).unwrap(),
                crate::pool::DeadpoolConnectionProvider::new(
                    "news.example".to_string(),
                    119,
                    name.to_string(),
                    10,
                    None,
                    None,
                ),
                0,
                AvailabilitySlot::new(0).unwrap(),
            );
        }

        let mut availability = ArticleAvailability::new();
        availability.record_missing_slot(AvailabilitySlot::new(0).unwrap());
        assert!(
            selector
                .route(RouteRequest::new(ClientId::new()).with_availability(&availability))
                .is_err()
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
        assert_eq!(
            BackendSelector::queue_depth_limit(
                40,
                crate::types::QueuePressurePercent::try_new(25).unwrap(),
            ),
            10
        );
        assert_eq!(
            BackendSelector::queue_depth_limit(
                50,
                crate::types::QueuePressurePercent::try_new(25).unwrap(),
            ),
            13
        );
        assert_eq!(
            BackendSelector::queue_depth_limit(
                40,
                crate::types::QueuePressurePercent::try_new(50).unwrap(),
            ),
            20
        );
        assert_eq!(
            BackendSelector::queue_depth_limit(
                50,
                crate::types::QueuePressurePercent::try_new(50).unwrap(),
            ),
            25
        );
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
        availability.record_missing_slot(crate::cache::AvailabilitySlot::new(0).unwrap());
        availability.record_missing_slot(crate::cache::AvailabilitySlot::new(1).unwrap());

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

    fn standing_selector(
        strategy: BackendSelectionStrategy,
        standing: crate::pool::admission::ArticleStanding,
        backends: &[(&str, usize, u8)],
    ) -> BackendSelector {
        let mut selector = BackendSelector::with_strategy(strategy).with_article_admission(
            crate::pool::admission::ArticleAdmission::default().with_standing(standing),
        );
        for &(name, max_connections, tier) in backends {
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
        selector
    }

    fn note_lookups(
        selector: &BackendSelector,
        backend: BackendId,
        outcome: crate::pool::admission::ArticleLookupOutcome,
        count: usize,
    ) {
        let provider = selector.backend_provider(backend).unwrap();
        for _ in 0..count {
            provider.note_article_lookup(outcome);
        }
    }

    /// Route `count` requests one at a time, completing each, and count where
    /// they went.
    fn route_counts(
        selector: &BackendSelector,
        availability: Option<&ArticleAvailability>,
        count: usize,
    ) -> Vec<usize> {
        let mut counts = vec![0; selector.backend_count().get()];
        for _ in 0..count {
            let request = RouteRequest::new(ClientId::new());
            let backend = match availability {
                Some(availability) => selector
                    .route(
                        request
                            .with_availability(availability)
                            .for_lookup(RouteLookup::Article),
                    )
                    .unwrap()
                    .backend_id(),
                None => selector.route(request).unwrap(),
            };
            counts[backend.as_index()] += 1;
            selector.complete_command(backend);
        }
        counts
    }

    /// Route `count` requests and keep them all in flight, as concurrent
    /// downloads do, and count where they went.
    fn route_held(selector: &BackendSelector, count: usize) -> Vec<usize> {
        let availability = ArticleAvailability::new();
        let mut counts = vec![0; selector.backend_count().get()];
        for _ in 0..count {
            let backend = selector
                .route(
                    RouteRequest::new(ClientId::new())
                        .with_availability(&availability)
                        .for_lookup(RouteLookup::Article),
                )
                .unwrap()
                .backend_id();
            counts[backend.as_index()] += 1;
        }
        counts
    }

    #[test]
    fn adaptive_standing_at_full_shares_load_by_capacity() {
        use crate::pool::admission::ArticleStanding;
        let selector = standing_selector(
            BackendSelectionStrategy::LeastLoaded,
            ArticleStanding::Adaptive,
            &[("large", 80, 0), ("small", 40, 0)],
        );
        let counts = route_held(&selector, 120);
        assert!(
            (75..=85).contains(&counts[0]) && (35..=45).contains(&counts[1]),
            "concurrent lookups split by capacity: {counts:?}"
        );
    }

    #[test]
    fn adaptive_standing_moves_article_first_attempts_off_a_missing_backend() {
        use crate::pool::admission::{ArticleLookupOutcome, ArticleStanding};
        let selector = standing_selector(
            BackendSelectionStrategy::LeastLoaded,
            ArticleStanding::Adaptive,
            &[("steady", 80, 0), ("taken-down", 70, 0)],
        );
        note_lookups(
            &selector,
            BackendId::from_index(1),
            ArticleLookupOutcome::Missing,
            40,
        );
        // Stay under the peer's hard queue-depth limit (half its pool), which
        // would exclude it whatever the standing.
        let counts = route_held(&selector, 35);
        assert_eq!(
            counts,
            vec![35, 0],
            "a peer with headroom wins every first attempt over a demoted backend"
        );
    }

    #[test]
    fn adaptive_standing_keeps_a_demoted_backend_as_overflow() {
        use crate::pool::admission::{ArticleLookupOutcome, ArticleStanding};
        let selector = standing_selector(
            BackendSelectionStrategy::LeastLoaded,
            ArticleStanding::Adaptive,
            &[("steady", 80, 0), ("taken-down", 70, 0)],
        );
        note_lookups(
            &selector,
            BackendId::from_index(1),
            ArticleLookupOutcome::Missing,
            200,
        );
        for _ in 0..79 {
            selector.mark_backend_pending(BackendId::from_index(0));
        }

        let availability = ArticleAvailability::new();
        let backend = selector
            .route(
                RouteRequest::new(ClientId::new())
                    .with_availability(&availability)
                    .for_lookup(RouteLookup::Article),
            )
            .unwrap();
        assert_eq!(
            backend.backend_id(),
            BackendId::from_index(1),
            "a full peer spills onto the demoted backend instead of queueing"
        );
    }

    #[test]
    fn adaptive_standing_lets_a_backend_earn_its_share_back() {
        use crate::pool::admission::{ArticleLookupOutcome, ArticleStanding};
        let selector = standing_selector(
            BackendSelectionStrategy::LeastLoaded,
            ArticleStanding::Adaptive,
            &[("steady", 80, 0), ("recovering", 70, 0)],
        );
        let recovering = BackendId::from_index(1);
        note_lookups(&selector, recovering, ArticleLookupOutcome::Missing, 200);
        note_lookups(&selector, recovering, ArticleLookupOutcome::Found, 1_000);

        let counts = route_held(&selector, 150);
        assert!(
            counts[1] > 50,
            "a redeemed backend gets its share back: {counts:?}"
        );
    }

    #[test]
    fn standing_off_routes_exactly_as_before() {
        use crate::pool::admission::{ArticleLookupOutcome, ArticleStanding};
        let selector = standing_selector(
            BackendSelectionStrategy::LeastLoaded,
            ArticleStanding::Off,
            &[("steady", 80, 0), ("taken-down", 70, 0)],
        );
        note_lookups(
            &selector,
            BackendId::from_index(1),
            ArticleLookupOutcome::Missing,
            200,
        );

        let counts = route_held(&selector, 150);
        assert!(counts[1] > 50, "standing is ignored when off: {counts:?}");
    }

    #[test]
    fn adaptive_standing_ignores_non_article_routing() {
        use crate::pool::admission::{ArticleLookupOutcome, ArticleStanding};
        let selector = standing_selector(
            BackendSelectionStrategy::LeastLoaded,
            ArticleStanding::Adaptive,
            &[("steady", 80, 0), ("taken-down", 70, 0)],
        );
        note_lookups(
            &selector,
            BackendId::from_index(1),
            ArticleLookupOutcome::Missing,
            200,
        );

        let mut counts = vec![0; 2];
        for _ in 0..150 {
            let backend = selector.route(RouteRequest::new(ClientId::new())).unwrap();
            counts[backend.as_index()] += 1;
        }
        assert!(
            counts[1] > 50,
            "group and list commands ignore standing: {counts:?}"
        );
    }

    #[test]
    fn adaptive_standing_ignores_availability_routed_non_article_commands() {
        use crate::pool::admission::{ArticleLookupOutcome, ArticleStanding};
        let selector = standing_selector(
            BackendSelectionStrategy::LeastLoaded,
            ArticleStanding::Adaptive,
            &[("steady", 80, 0), ("taken-down", 70, 0)],
        );
        note_lookups(
            &selector,
            BackendId::from_index(1),
            ArticleLookupOutcome::Missing,
            200,
        );

        // Per-command LIST ACTIVE and similar go through availability routing
        // without being article lookups.
        let availability = ArticleAvailability::new();
        let mut counts = vec![0; 2];
        for _ in 0..150 {
            let backend = selector
                .route(RouteRequest::new(ClientId::new()).with_availability(&availability))
                .unwrap()
                .backend_id();
            counts[backend.as_index()] += 1;
        }
        assert!(
            counts[1] > 50,
            "standing is for article lookups only: {counts:?}"
        );
    }

    #[test]
    fn adaptive_standing_never_crosses_tiers() {
        use crate::pool::admission::{ArticleLookupOutcome, ArticleStanding};
        let selector = standing_selector(
            BackendSelectionStrategy::LeastLoaded,
            ArticleStanding::Adaptive,
            &[("primary", 80, 0), ("taken-down", 70, 0), ("backup", 20, 1)],
        );
        let taken_down = BackendId::from_index(1);
        note_lookups(&selector, taken_down, ArticleLookupOutcome::Missing, 200);

        // The healthy primary already answered 430 for this article, so the
        // demoted tier-0 backend must still be tried before the backup tier.
        let mut availability = ArticleAvailability::new();
        availability.record_missing_slot(crate::cache::AvailabilitySlot::new(0).unwrap());
        let counts = route_counts(&selector, Some(&availability), 50);
        assert_eq!(counts, vec![0, 50, 0]);
    }

    #[test]
    fn adaptive_standing_scales_weighted_round_robin_shares() {
        use crate::pool::admission::{ArticleLookupOutcome, ArticleStanding};
        let selector = standing_selector(
            BackendSelectionStrategy::WeightedRoundRobin,
            ArticleStanding::Adaptive,
            &[("steady", 80, 0), ("taken-down", 70, 0)],
        );
        note_lookups(
            &selector,
            BackendId::from_index(1),
            ArticleLookupOutcome::Missing,
            200,
        );

        let availability = ArticleAvailability::new();
        let counts = route_counts(&selector, Some(&availability), 820);
        // Floor standing 1/64 turns 70 connections into a weight of 2
        // against 80, so 2 of every 82 first attempts still probe it.
        assert_eq!(
            counts,
            vec![800, 20],
            "a demoted backend keeps a trickle of first attempts"
        );
    }

    fn tiered_selector(backends: &[(&str, u8)]) -> BackendSelector {
        let mut selector = BackendSelector::with_strategy(BackendSelectionStrategy::LeastLoaded);
        for &(name, tier) in backends {
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
        selector
    }

    #[test]
    fn stateful_sessions_never_route_to_excluded_backends() {
        let mut selector =
            tiered_selector(&[("numbering-a", 0), ("other-numbering", 0), ("backup", 1)]);
        selector.exclude_from_stateful_sessions(BackendId::from_index(1));

        // Each route leaves its backend pending, so least-loaded would
        // otherwise move on to the idle excluded peer.
        for _ in 0..32 {
            let backend = selector.route_stateful_session(ClientId::new()).unwrap();
            assert_eq!(backend, BackendId::from_index(0));
        }
    }

    #[test]
    fn excluded_backends_still_serve_message_id_routing() {
        let mut selector = tiered_selector(&[("numbering-a", 0), ("other-numbering", 0)]);
        selector.exclude_from_stateful_sessions(BackendId::from_index(1));

        let availability = ArticleAvailability::new();
        let routed: std::collections::HashSet<_> = (0..32)
            .map(|_| {
                selector
                    .route(RouteRequest::new(ClientId::new()).with_availability(&availability))
                    .unwrap()
                    .backend_id()
            })
            .collect();
        assert!(routed.contains(&BackendId::from_index(1)));
    }

    #[test]
    fn stateful_sessions_fall_through_an_all_excluded_tier() {
        let mut selector = tiered_selector(&[("download-only", 0), ("indexer", 1)]);
        selector.exclude_from_stateful_sessions(BackendId::from_index(0));

        assert_eq!(
            selector.route_stateful_session(ClientId::new()).unwrap(),
            BackendId::from_index(1)
        );
    }

    #[test]
    fn stateful_sessions_error_when_every_backend_is_excluded() {
        let mut selector = tiered_selector(&[("a", 0), ("b", 1)]);
        selector.exclude_from_stateful_sessions(BackendId::from_index(0));
        selector.exclude_from_stateful_sessions(BackendId::from_index(1));

        assert!(selector.route_stateful_session(ClientId::new()).is_err());
    }

    #[test]
    fn article_routing_skips_excluded_backends_and_escalates_past_an_excluded_tier() {
        let selector = tiered_selector(&[("allowed", 0), ("excluded", 0), ("excluded-only", 1)]);
        let availability = ArticleAvailability::new();
        let excluded = suppressed(&[BackendId::from_index(1)]);
        for _ in 0..16 {
            let backend = selector
                .route(
                    RouteRequest::new(ClientId::new())
                        .with_availability(&availability)
                        .excluding_backends(excluded),
                )
                .unwrap();
            assert_eq!(backend.backend_id(), BackendId::from_index(0));
        }

        let selector = tiered_selector(&[("excluded-a", 0), ("excluded-b", 0), ("allowed", 1)]);
        let excluded = suppressed(&[BackendId::from_index(0), BackendId::from_index(1)]);
        let backend = selector
            .route(
                RouteRequest::new(ClientId::new())
                    .with_availability(&availability)
                    .excluding_backends(excluded),
            )
            .unwrap();
        assert_eq!(
            backend.backend_id(),
            BackendId::from_index(2),
            "permanent exclusions are not transient failures, so the next tier is eligible"
        );
    }

    #[test]
    fn transient_suppression_in_a_tier_still_blocks_escalation_with_exclusions() {
        // Excluding one backend must not let a transient failure of the other
        // backend in the tier skip ahead to a lower-priority tier.
        let selector = tiered_selector(&[("allowed", 0), ("excluded", 0), ("fallback", 1)]);
        let availability = ArticleAvailability::new();
        let result = selector.route(
            RouteRequest::new(ClientId::new())
                .with_availability(&availability)
                .suppressing_backends(suppressed(&[BackendId::from_index(0)]))
                .excluding_backends(suppressed(&[BackendId::from_index(1)])),
        );
        assert!(result.is_err());
    }
}
