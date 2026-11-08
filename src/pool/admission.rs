//! Backlog-aware checkout admission for article routing.
//!
//! A small backend pool under heavy load can have hundreds of requests queued
//! for a couple of connections. Each of those requests waits its whole checkout
//! limit and then fails, which only adds latency for the client: another
//! backend could have served it, or the client could have retried sooner.
//!
//! Before queueing, a checkout estimates how long it would wait from the
//! pool's queue length and a running average of how long one command holds a
//! connection. When that estimate is clearly longer than the time the request
//! is willing to wait, the checkout is skipped without joining the queue. A
//! skipped backend is unavailable for that request only; it is not an article
//! miss and is not recorded as one.
//!
//! Skipping only helps when another backend can answer. When every backend
//! left in the tier is backlogged, a client request queues on the one expected
//! to free a slot first ([`BacklogLastResort`]) instead of failing outright.
//!
//! Everything on the checkout path is a few relaxed atomic loads; nothing here
//! allocates or takes a lock.

use super::provider::DeadpoolStatusCounts;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Weight of a new sample in the service-time average, as a right shift
/// (`1/8`). The same smoothing TCP uses for its RTT estimate: one slow article
/// moves the estimate a little, while a backend that really slows down
/// dominates it within a few dozen commands.
const EWMA_SHIFT: u32 = 3;

/// Shortest gap between two skip summaries for one backend. A backlogged
/// backend can be skipped thousands of times a minute; one line per backend
/// every ten seconds keeps the signal without flooding the log.
pub const SKIP_SUMMARY_INTERVAL: Duration = Duration::from_secs(10);

/// Whether a checkout may refuse to join a long pool queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BacklogPolicy {
    /// Always queue, and rely on the checkout wait limit alone.
    Queue,
    /// Skip the backend when the estimated wait is longer than this
    /// percentage of the checkout's wait limit.
    SkipAbove { percent: u16 },
    /// Queue whatever the backlog, because every other backend that could
    /// still serve the request was skipped as backlogged. Counted apart from
    /// ordinary checkouts so operators can see how often skipping left a
    /// request with nowhere else to go.
    LastResort,
}

/// What an article request does when backlog skipping has left no other
/// backend to try in the tier that should serve it.
///
/// Skipping a backlogged backend saves time only when another backend can
/// answer instead. When the article's remaining sources are all busy,
/// giving up turns a slow answer into a certain failure, and the client then
/// spends one of its own retries asking again.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BacklogLastResort {
    /// Queue on the skipped backend with the shortest estimated wait, bounded
    /// by the checkout wait and what is left of the deadline.
    #[default]
    WaitShortest,
    /// End the request as soon as no unskipped backend is left.
    GiveUp,
}

/// What limited a checkout's wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitBound {
    /// The per-attempt checkout limit.
    Attempt,
    /// What was left of the request's deadline, which was shorter than the
    /// per-attempt limit.
    Deadline,
}

/// How long one checkout may wait for a slot, and whether it may skip a
/// backlogged pool instead of waiting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckoutBudget {
    wait: Duration,
    backlog: BacklogPolicy,
    bound: WaitBound,
}

impl CheckoutBudget {
    #[must_use]
    pub const fn new(wait: Duration, backlog: BacklogPolicy) -> Self {
        Self {
            wait,
            backlog,
            bound: WaitBound::Attempt,
        }
    }

    /// Shorten the wait to `remaining` when that is less than the current wait.
    #[must_use]
    pub fn within(self, remaining: Duration) -> Self {
        if remaining < self.wait {
            Self {
                wait: remaining,
                bound: WaitBound::Deadline,
                ..self
            }
        } else {
            self
        }
    }

    #[must_use]
    pub const fn wait(self) -> Duration {
        self.wait
    }

    #[must_use]
    pub const fn bound(self) -> WaitBound {
        self.bound
    }

    #[must_use]
    pub const fn backlog(self) -> BacklogPolicy {
        self.backlog
    }
}

/// Decision for one checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Join the pool queue, bounded by the checkout wait.
    Queue,
    /// Do not queue: the pool would not hand out a connection in time.
    Skip { estimated_wait: Duration },
}

/// Estimate how long a new checkout would wait for a pool slot.
///
/// A checkout that finds an idle connection, or room to open one, does not
/// wait. Otherwise it is behind every current waiter that cannot be served
/// from free capacity. With `max_size` connections each held for about
/// `service_time`, a slot frees up roughly every `service_time / max_size`, so
/// the `n`th checkout in line waits about `n * service_time / max_size`.
///
/// Without a service-time sample the estimate is zero: the pool has never
/// completed a command, so there is nothing to base a skip on and the bounded
/// checkout wait stays the only limit.
#[must_use]
pub fn estimate_wait(status: DeadpoolStatusCounts, service_time: Option<Duration>) -> Duration {
    let Some(service_time) = service_time else {
        return Duration::ZERO;
    };
    let free = status.available + status.max_size.saturating_sub(status.size);
    if status.waiting < free {
        return Duration::ZERO;
    }
    if status.max_size == 0 {
        return Duration::MAX;
    }
    let position = (status.waiting - free + 1) as u128;
    let micros = service_time.as_micros() * position / status.max_size as u128;
    Duration::from_micros(u64::try_from(micros).unwrap_or(u64::MAX))
}

/// Decide whether a checkout with `budget` should queue given `estimated_wait`.
#[must_use]
pub fn admit(estimated_wait: Duration, budget: CheckoutBudget) -> Admission {
    match budget.backlog {
        BacklogPolicy::Queue | BacklogPolicy::LastResort => Admission::Queue,
        BacklogPolicy::SkipAbove { percent } => {
            let limit = budget.wait.saturating_mul(u32::from(percent)) / 100;
            if estimated_wait > limit {
                Admission::Skip { estimated_wait }
            } else {
                Admission::Queue
            }
        }
    }
}

/// Counts of expected busy outcomes since the last summary line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SkipSummary {
    pub backlog_skips: u64,
    pub busy_timeouts: u64,
}

/// Point-in-time copy of a backend's admission counters for the stats file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BackendLoadSnapshot {
    pub backlog_skips: u64,
    pub busy_timeouts: u64,
    pub deadline_expiries: u64,
    pub service_time_ewma_micros: u64,
    pub last_resort_waits: u64,
    pub last_resort_connected: u64,
    pub last_resort_timeouts: u64,
}

/// Per-backend service-time estimate and busy-outcome counters.
///
/// Shared by every clone of a provider. All fields are atomics updated with
/// relaxed ordering: they feed a heuristic and a log line, so a lost update
/// under a race costs nothing.
#[derive(Debug)]
pub struct BackendLoad {
    /// Average time a command holds a connection, in microseconds. Zero means
    /// no sample yet.
    service_time_micros: AtomicU64,
    backlog_skips: AtomicU64,
    busy_timeouts: AtomicU64,
    deadline_expiries: AtomicU64,
    last_resort_waits: AtomicU64,
    last_resort_connected: AtomicU64,
    last_resort_timeouts: AtomicU64,
    unlogged_backlog_skips: AtomicU64,
    unlogged_busy_timeouts: AtomicU64,
    /// Milliseconds since `epoch`, offset by one interval so the first skip
    /// is reported at once, at which the last summary was claimed.
    last_summary_ms: AtomicU64,
    epoch: Instant,
}

impl Default for BackendLoad {
    fn default() -> Self {
        Self {
            service_time_micros: AtomicU64::new(0),
            backlog_skips: AtomicU64::new(0),
            busy_timeouts: AtomicU64::new(0),
            deadline_expiries: AtomicU64::new(0),
            last_resort_waits: AtomicU64::new(0),
            last_resort_connected: AtomicU64::new(0),
            last_resort_timeouts: AtomicU64::new(0),
            unlogged_backlog_skips: AtomicU64::new(0),
            unlogged_busy_timeouts: AtomicU64::new(0),
            last_summary_ms: AtomicU64::new(0),
            epoch: Instant::now(),
        }
    }
}

impl BackendLoad {
    /// Fold one command's connection hold time into the average.
    ///
    /// A plain load and store rather than a compare-and-swap loop: two
    /// commands finishing at once can drop one sample, which does not matter
    /// for an average over many commands, and the hot path stays one load and
    /// one store.
    pub fn record_service_time(&self, held: Duration) {
        // Never store zero, which means "no sample".
        let sample = u64::try_from(held.as_micros()).unwrap_or(u64::MAX).max(1);
        let current = self.service_time_micros.load(Ordering::Relaxed);
        let next = if current == 0 {
            sample
        } else if sample >= current {
            current + ((sample - current) >> EWMA_SHIFT)
        } else {
            current - ((current - sample) >> EWMA_SHIFT)
        };
        self.service_time_micros
            .store(next.max(1), Ordering::Relaxed);
    }

    /// Current service-time average, or `None` before the first sample.
    #[must_use]
    pub fn service_time(&self) -> Option<Duration> {
        match self.service_time_micros.load(Ordering::Relaxed) {
            0 => None,
            micros => Some(Duration::from_micros(micros)),
        }
    }

    /// Count a checkout that skipped this backend without queueing. Returns
    /// counts to log when a summary is due.
    pub fn note_backlog_skip(&self) -> Option<SkipSummary> {
        self.backlog_skips.fetch_add(1, Ordering::Relaxed);
        self.unlogged_backlog_skips.fetch_add(1, Ordering::Relaxed);
        self.claim_summary(self.now_ms())
    }

    /// Count a bounded checkout that waited its whole limit without a slot.
    /// Returns counts to log when a summary is due.
    pub fn note_busy_timeout(&self) -> Option<SkipSummary> {
        self.busy_timeouts.fetch_add(1, Ordering::Relaxed);
        self.unlogged_busy_timeouts.fetch_add(1, Ordering::Relaxed);
        self.claim_summary(self.now_ms())
    }

    /// Count a request whose deadline ran out while this backend was next.
    pub fn note_deadline_expiry(&self) {
        self.deadline_expiries.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a last-resort checkout joining this backend's queue.
    pub fn note_last_resort_wait(&self) {
        self.last_resort_waits.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a last-resort checkout that got a connection.
    pub fn note_last_resort_connected(&self) {
        self.last_resort_connected.fetch_add(1, Ordering::Relaxed);
    }

    /// Count a last-resort checkout that waited its whole limit.
    pub fn note_last_resort_timeout(&self) {
        self.last_resort_timeouts.fetch_add(1, Ordering::Relaxed);
    }

    #[must_use]
    pub fn snapshot(&self) -> BackendLoadSnapshot {
        BackendLoadSnapshot {
            backlog_skips: self.backlog_skips.load(Ordering::Relaxed),
            busy_timeouts: self.busy_timeouts.load(Ordering::Relaxed),
            deadline_expiries: self.deadline_expiries.load(Ordering::Relaxed),
            service_time_ewma_micros: self.service_time_micros.load(Ordering::Relaxed),
            last_resort_waits: self.last_resort_waits.load(Ordering::Relaxed),
            last_resort_connected: self.last_resort_connected.load(Ordering::Relaxed),
            last_resort_timeouts: self.last_resort_timeouts.load(Ordering::Relaxed),
        }
    }

    fn now_ms(&self) -> u64 {
        let elapsed = u64::try_from(self.epoch.elapsed().as_millis()).unwrap_or(u64::MAX);
        elapsed.saturating_add(interval_ms())
    }

    /// Let exactly one caller per interval take the unlogged counts.
    fn claim_summary(&self, now_ms: u64) -> Option<SkipSummary> {
        let last = self.last_summary_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) < interval_ms() {
            return None;
        }
        self.last_summary_ms
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .ok()?;
        Some(SkipSummary {
            backlog_skips: self.unlogged_backlog_skips.swap(0, Ordering::Relaxed),
            busy_timeouts: self.unlogged_busy_timeouts.swap(0, Ordering::Relaxed),
        })
    }
}

fn interval_ms() -> u64 {
    u64::try_from(SKIP_SUMMARY_INTERVAL.as_millis()).unwrap_or(u64::MAX)
}

/// Reply for an article request that ends before every eligible backend
/// returned `430`: its deadline ran out, or every remaining backend was busy,
/// skipped, or failed.
///
/// `430` is a claim that the article does not exist on any backend that could
/// serve it. Downloaders act on that claim: a typical client gives up on the article on
/// that server, so a `430` sent while a backend that has the article was only
/// queued or skipped turns a busy moment into a permanent miss. `503` says the
/// proxy could not answer now, and the downloader retries later. The default
/// is therefore `503`; `430` is available for operators who prefer fast
/// failure over retries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum UnavailableReply {
    /// `430 No such article`.
    #[serde(rename = "430")]
    NoSuchArticle,
    /// `503 Backend error`.
    #[default]
    #[serde(rename = "503")]
    BackendError,
}

impl UnavailableReply {
    /// Wire bytes for this reply.
    #[must_use]
    pub const fn response(self) -> &'static [u8] {
        match self {
            Self::NoSuchArticle => crate::protocol::NO_SUCH_ARTICLE,
            Self::BackendError => crate::protocol::BACKEND_ERROR,
        }
    }
}

/// Counts recent unfinished lookups per message-id, so an article that keeps
/// ending unfinished can be reported missing.
///
/// A truly dead article has to be confirmed `430` by every backend, and on a
/// busy multi-tier setup a small, slow backend may never be reached within the
/// deadline. Each unfinished lookup answers `503`, so a downloader retries it
/// many times and a dead release takes minutes to fail. After a bounded number
/// of `503`s the proxy answers `430` instead, but only while every backend
/// that answered said `430`.
///
/// Fixed-size and lock-free: each slot packs a 56-bit message-id hash tag and
/// an 8-bit count. A collision replaces the slot and restarts the count, which
/// errs towards more `503`s rather than a false `430`.
#[derive(Debug)]
pub struct UnfinishedLookups {
    slots: Box<[std::sync::atomic::AtomicU64]>,
}

impl UnfinishedLookups {
    const SLOTS: usize = 1 << 16;
    const COUNT_BITS: u32 = 8;
    const COUNT_MASK: u64 = (1 << Self::COUNT_BITS) - 1;

    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: (0..Self::SLOTS)
                .map(|_| std::sync::atomic::AtomicU64::new(0))
                .collect(),
        }
    }

    fn hash(message_id: &str) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        message_id.hash(&mut hasher);
        hasher.finish()
    }

    /// Record one unfinished lookup and return how many this message-id has
    /// had, including this one.
    pub fn record(&self, message_id: &str) -> u8 {
        use std::sync::atomic::Ordering;
        let hash = Self::hash(message_id);
        let tag = hash & !Self::COUNT_MASK;
        let slot = &self.slots[usize::try_from(hash).unwrap_or_default() % Self::SLOTS];
        let mut current = slot.load(Ordering::Relaxed);
        loop {
            let count = if current & !Self::COUNT_MASK == tag {
                ((current & Self::COUNT_MASK) + 1).min(Self::COUNT_MASK)
            } else {
                1
            };
            match slot.compare_exchange_weak(
                current,
                tag | count,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return u8::try_from(count).unwrap_or(u8::MAX),
                Err(actual) => current = actual,
            }
        }
    }
}

impl Default for UnfinishedLookups {
    fn default() -> Self {
        Self::new()
    }
}

/// Article routing limits from `[routing.article]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArticleAdmission {
    deadline: Option<Duration>,
    checkout_wait: Duration,
    backlog: BacklogPolicy,
    unavailable_reply: UnavailableReply,
    last_resort: BacklogLastResort,
    unfinished_replies_before_430: u8,
}

impl Default for ArticleAdmission {
    fn default() -> Self {
        Self::new(
            Some(crate::constants::pool::ARTICLE_DEADLINE),
            crate::constants::pool::ARTICLE_CHECKOUT_WAIT,
            BacklogPolicy::SkipAbove { percent: 100 },
        )
    }
}

impl ArticleAdmission {
    /// `deadline` of `None` leaves a request unbounded across backends.
    #[must_use]
    pub const fn new(
        deadline: Option<Duration>,
        checkout_wait: Duration,
        backlog: BacklogPolicy,
    ) -> Self {
        Self {
            deadline,
            checkout_wait,
            backlog,
            unavailable_reply: UnavailableReply::BackendError,
            last_resort: BacklogLastResort::WaitShortest,
            unfinished_replies_before_430: crate::constants::pool::UNFINISHED_REPLIES_BEFORE_430,
        }
    }

    /// How many `503` replies one message-id may get, while every backend that
    /// answered it said `430`, before the next unfinished lookup answers `430`.
    /// `0` never converts.
    #[must_use]
    pub const fn with_unfinished_replies_before_430(mut self, replies: u8) -> Self {
        self.unfinished_replies_before_430 = replies;
        self
    }

    #[must_use]
    pub const fn unfinished_replies_before_430(&self) -> u8 {
        self.unfinished_replies_before_430
    }

    /// What a request does when backlog skipping leaves it no other backend.
    #[must_use]
    pub const fn with_backlog_last_resort(mut self, last_resort: BacklogLastResort) -> Self {
        self.last_resort = last_resort;
        self
    }

    #[must_use]
    pub const fn backlog_last_resort(&self) -> BacklogLastResort {
        self.last_resort
    }

    /// Use `reply` both when the deadline expires and when no backend is left
    /// to try.
    #[must_use]
    pub const fn with_unavailable_reply(mut self, reply: UnavailableReply) -> Self {
        self.unavailable_reply = reply;
        self
    }

    /// Reply for a request that ended before every eligible backend returned
    /// `430`, whether its deadline expired or no backend was left to try.
    #[must_use]
    pub const fn unavailable_reply(&self) -> UnavailableReply {
        self.unavailable_reply
    }

    /// Start the clock for one client article request.
    #[must_use]
    pub fn start_request(&self) -> ArticleBudget {
        ArticleBudget {
            expires_at: self
                .deadline
                .map(|deadline| tokio::time::Instant::now() + deadline),
            checkout_wait: self.checkout_wait,
            backlog: self.backlog,
            last_resort: self.last_resort,
        }
    }

    /// Budget for a background `STAT` probe that is not part of a client
    /// request's deadline.
    #[must_use]
    pub const fn probe(&self) -> CheckoutBudget {
        CheckoutBudget::new(
            crate::constants::pool::STAT_PROBE_CHECKOUT_WAIT,
            self.backlog,
        )
    }

    #[must_use]
    pub const fn deadline(&self) -> Option<Duration> {
        self.deadline
    }

    #[must_use]
    pub const fn checkout_wait(&self) -> Duration {
        self.checkout_wait
    }

    #[must_use]
    pub const fn backlog(&self) -> BacklogPolicy {
        self.backlog
    }
}

/// Remaining time for one client article request across all backend attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArticleBudget {
    expires_at: Option<tokio::time::Instant>,
    checkout_wait: Duration,
    backlog: BacklogPolicy,
    last_resort: BacklogLastResort,
}

impl ArticleBudget {
    fn remaining(&self) -> Option<Duration> {
        self.expires_at
            .map(|expires_at| expires_at.saturating_duration_since(tokio::time::Instant::now()))
    }

    /// Whether the request has used its whole deadline.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        self.remaining()
            .is_some_and(|remaining| remaining.is_zero())
    }

    /// Budget for the next article checkout: the per-attempt wait, cut to what
    /// is left of the deadline.
    #[must_use]
    pub fn checkout(&self) -> CheckoutBudget {
        self.bounded(CheckoutBudget::new(self.checkout_wait, self.backlog))
    }

    /// Budget for queueing on a backend that was skipped as backlogged,
    /// because no other backend is left to try. Bounded like any other
    /// article checkout, so a last resort never outlives the deadline.
    #[must_use]
    pub fn last_resort_checkout(&self) -> CheckoutBudget {
        self.bounded(CheckoutBudget::new(
            self.checkout_wait,
            BacklogPolicy::LastResort,
        ))
    }

    #[must_use]
    pub const fn last_resort(&self) -> BacklogLastResort {
        self.last_resort
    }

    /// Budget for a `STAT` probe that runs inside this request.
    #[must_use]
    pub fn probe(&self) -> CheckoutBudget {
        self.bounded(CheckoutBudget::new(
            crate::constants::pool::STAT_PROBE_CHECKOUT_WAIT,
            self.backlog,
        ))
    }

    fn bounded(&self, budget: CheckoutBudget) -> CheckoutBudget {
        match self.remaining() {
            Some(remaining) => budget.within(remaining),
            None => budget,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn status(
        available: usize,
        size: usize,
        max_size: usize,
        waiting: usize,
    ) -> DeadpoolStatusCounts {
        DeadpoolStatusCounts {
            available,
            size,
            max_size,
            waiting,
        }
    }

    const SKIP_AT_BUDGET: BacklogPolicy = BacklogPolicy::SkipAbove { percent: 100 };

    #[test]
    fn no_service_sample_never_estimates_a_wait() {
        assert_eq!(estimate_wait(status(0, 2, 2, 400), None), Duration::ZERO);
    }

    #[test]
    fn free_capacity_means_no_wait() {
        let service = Some(Duration::from_secs(1));
        assert_eq!(estimate_wait(status(1, 2, 2, 0), service), Duration::ZERO);
        assert_eq!(
            estimate_wait(status(0, 1, 2, 0), service),
            Duration::ZERO,
            "room to open a connection is free capacity"
        );
    }

    #[test]
    fn queue_position_scales_with_waiters_over_pool_size() {
        let service = Some(Duration::from_millis(500));
        // Full pool, nobody waiting: next release, about service / max_size.
        assert_eq!(
            estimate_wait(status(0, 2, 2, 0), service),
            Duration::from_millis(250)
        );
        // Hundreds of waiters on a 2-connection pool.
        assert_eq!(
            estimate_wait(status(0, 2, 2, 400), service),
            Duration::from_micros(500_000 * 401 / 2)
        );
        // A big pool drains the same queue faster.
        assert_eq!(
            estimate_wait(status(0, 50, 50, 49), service),
            Duration::from_millis(500)
        );
    }

    #[test]
    fn zero_sized_pool_never_frees_a_slot() {
        assert_eq!(
            estimate_wait(status(0, 0, 0, 0), Some(Duration::from_millis(1))),
            Duration::MAX
        );
    }

    #[test]
    fn admission_skips_only_above_the_budget_share() {
        let budget = CheckoutBudget::new(Duration::from_secs(3), SKIP_AT_BUDGET);
        assert_eq!(admit(Duration::from_secs(3), budget), Admission::Queue);
        assert_eq!(
            admit(Duration::from_millis(3001), budget),
            Admission::Skip {
                estimated_wait: Duration::from_millis(3001)
            }
        );

        let half = CheckoutBudget::new(
            Duration::from_secs(3),
            BacklogPolicy::SkipAbove { percent: 50 },
        );
        assert!(matches!(
            admit(Duration::from_secs(2), half),
            Admission::Skip { .. }
        ));
    }

    #[test]
    fn queue_policy_never_skips() {
        let budget = CheckoutBudget::new(Duration::from_millis(1), BacklogPolicy::Queue);
        assert_eq!(admit(Duration::MAX, budget), Admission::Queue);
    }

    #[test]
    fn a_shorter_deadline_bounds_the_wait() {
        let budget = CheckoutBudget::new(Duration::from_secs(3), SKIP_AT_BUDGET);
        assert_eq!(budget.within(Duration::from_secs(5)), budget);
        let cut = budget.within(Duration::from_secs(1));
        assert_eq!(cut.wait(), Duration::from_secs(1));
        assert_eq!(cut.bound(), WaitBound::Deadline);
        assert!(
            matches!(admit(Duration::from_secs(2), cut), Admission::Skip { .. }),
            "the skip threshold follows the shortened wait"
        );
    }

    #[test]
    fn service_time_average_starts_at_the_first_sample_and_smooths() {
        let load = BackendLoad::default();
        assert_eq!(load.service_time(), None);
        load.record_service_time(Duration::from_millis(800));
        assert_eq!(load.service_time(), Some(Duration::from_millis(800)));
        load.record_service_time(Duration::from_millis(400));
        assert_eq!(load.service_time(), Some(Duration::from_millis(750)));
        load.record_service_time(Duration::from_millis(1550));
        assert_eq!(load.service_time(), Some(Duration::from_millis(850)));
        load.record_service_time(Duration::ZERO);
        assert!(
            load.service_time().is_some(),
            "a zero sample is not \"no sample\""
        );
    }

    #[test]
    fn service_time_average_converges_to_a_new_level() {
        let load = BackendLoad::default();
        load.record_service_time(Duration::from_secs(5));
        for _ in 0..100 {
            load.record_service_time(Duration::from_millis(100));
        }
        let estimate = load.service_time().unwrap();
        assert!(
            estimate < Duration::from_millis(101),
            "average follows a sustained change, got {estimate:?}"
        );
    }

    #[test]
    fn summaries_are_rate_limited_and_carry_unlogged_counts() {
        let load = BackendLoad::default();
        let first = load.note_backlog_skip();
        assert_eq!(
            first,
            Some(SkipSummary {
                backlog_skips: 1,
                busy_timeouts: 0
            }),
            "the first skip is reported at once"
        );
        assert_eq!(load.note_backlog_skip(), None);
        assert_eq!(load.note_busy_timeout(), None);

        let later = load.now_ms() + interval_ms();
        assert_eq!(
            load.claim_summary(later),
            Some(SkipSummary {
                backlog_skips: 1,
                busy_timeouts: 1
            })
        );
        assert_eq!(load.claim_summary(later), None);
        assert_eq!(
            load.snapshot(),
            BackendLoadSnapshot {
                backlog_skips: 2,
                busy_timeouts: 1,
                deadline_expiries: 0,
                service_time_ewma_micros: 0,
                last_resort_waits: 0,
                last_resort_connected: 0,
                last_resort_timeouts: 0,
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn article_budget_shrinks_checkout_waits_to_the_deadline() {
        let admission = ArticleAdmission::new(
            Some(Duration::from_secs(5)),
            Duration::from_secs(3),
            SKIP_AT_BUDGET,
        );
        let budget = admission.start_request();
        assert_eq!(budget.checkout().wait(), Duration::from_secs(3));
        assert_eq!(budget.checkout().bound(), WaitBound::Attempt);

        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(budget.checkout().wait(), Duration::from_secs(1));
        assert_eq!(budget.checkout().bound(), WaitBound::Deadline);
        assert!(!budget.is_expired());

        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(budget.is_expired());
        assert_eq!(budget.checkout().wait(), Duration::ZERO);
    }

    #[test]
    fn unfinished_lookups_count_per_message_id() {
        let lookups = UnfinishedLookups::new();
        assert_eq!(lookups.record("<a@b>"), 1);
        assert_eq!(lookups.record("<a@b>"), 2);
        assert_eq!(lookups.record("<c@d>"), 1);
        assert_eq!(lookups.record("<a@b>"), 3);
        for _ in 0..300 {
            lookups.record("<e@f>");
        }
        assert_eq!(lookups.record("<e@f>"), u8::MAX);
    }

    #[test]
    fn unfinished_replies_before_430_defaults_and_overrides() {
        let default = ArticleAdmission::default();
        assert_eq!(
            default.unfinished_replies_before_430(),
            crate::constants::pool::UNFINISHED_REPLIES_BEFORE_430
        );
        assert_eq!(
            default
                .with_unfinished_replies_before_430(0)
                .unfinished_replies_before_430(),
            0
        );
    }

    #[test]
    fn unavailable_reply_defaults_to_503_and_can_be_overridden() {
        let default = ArticleAdmission::default();
        assert_eq!(default.unavailable_reply(), UnavailableReply::BackendError);
        assert_eq!(UnavailableReply::default(), UnavailableReply::BackendError);

        for reply in [
            UnavailableReply::NoSuchArticle,
            UnavailableReply::BackendError,
        ] {
            let set = default.with_unavailable_reply(reply);
            assert_eq!(set.unavailable_reply(), reply);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn last_resort_checkout_queues_within_the_deadline() {
        let admission = ArticleAdmission::new(
            Some(Duration::from_secs(5)),
            Duration::from_secs(3),
            BacklogPolicy::SkipAbove { percent: 10 },
        );
        assert_eq!(
            admission.backlog_last_resort(),
            BacklogLastResort::WaitShortest
        );
        let budget = admission.start_request();
        let last_resort = budget.last_resort_checkout();
        assert_eq!(last_resort.backlog(), BacklogPolicy::LastResort);
        assert_eq!(last_resort.wait(), Duration::from_secs(3));
        assert_eq!(
            admit(Duration::MAX, last_resort),
            Admission::Queue,
            "a last resort queues however long the backlog"
        );

        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(budget.last_resort_checkout().wait(), Duration::from_secs(1));
        assert_eq!(budget.last_resort_checkout().bound(), WaitBound::Deadline);

        let give_up = admission
            .with_backlog_last_resort(BacklogLastResort::GiveUp)
            .start_request();
        assert_eq!(give_up.last_resort(), BacklogLastResort::GiveUp);
    }

    #[test]
    fn last_resort_counters_reach_the_snapshot() {
        let load = BackendLoad::default();
        load.note_last_resort_wait();
        load.note_last_resort_wait();
        load.note_last_resort_connected();
        load.note_last_resort_timeout();
        let snapshot = load.snapshot();
        assert_eq!(snapshot.last_resort_waits, 2);
        assert_eq!(snapshot.last_resort_connected, 1);
        assert_eq!(snapshot.last_resort_timeouts, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn no_deadline_keeps_the_per_attempt_wait() {
        let admission = ArticleAdmission::new(None, Duration::from_secs(3), BacklogPolicy::Queue);
        let budget = admission.start_request();
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert!(!budget.is_expired());
        assert_eq!(budget.checkout().wait(), Duration::from_secs(3));
    }
}
