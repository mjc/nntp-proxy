//! Rate limiting and clear transitions for operator-facing log events.
//!
//! A persistent fault (a backend refusing logins, a full disk) can repeat
//! thousands of times a minute. Operators alerting on log lines want one line
//! when it starts, a line about once a minute while it lasts with how many
//! times it happened, and one line when it clears. The types here make that
//! decision; callers keep the `tracing` macro so the level, message and fields
//! stay visible at the call site.
//!
//! - [`SummaryWindow`]: the lock-free "one claim per window" primitive.
//! - [`BurstLimiter`]: lock-free counting for conditions that repeat per
//!   request (pool exhaustion) and have no success signal.
//! - [`EventGate`]: episodes that clear on evidence of recovery, for failure
//!   paths and periodic tasks where a small mutex costs nothing.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::Instant;

/// Minimum spacing between repeated lines for one condition.
pub const OPERATOR_EVENT_WINDOW: Duration = Duration::from_secs(60);

/// Milliseconds since construction, never zero, so zero can mean "never" in
/// an atomic timestamp. Uses tokio's clock so paused-time tests control it.
#[derive(Debug)]
struct Clock {
    epoch: Instant,
}

impl Clock {
    fn new() -> Self {
        Self {
            epoch: Instant::now(),
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_millis())
            .unwrap_or(u64::MAX)
            .saturating_add(1)
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// Whether `since_ms` (zero meaning never) is less than `window_ms` before
/// `now_ms`.
const fn within(since_ms: u64, now_ms: u64, window_ms: u64) -> bool {
    since_ms != 0 && now_ms.saturating_sub(since_ms) < window_ms
}

/// Lets exactly one caller per window claim the right to log.
///
/// Lock-free: a claim is a load and, when due, one compare-and-swap, so
/// racing callers get exactly one winner per window.
#[derive(Debug)]
pub struct SummaryWindow {
    window_ms: u64,
    clock: Clock,
    /// Time of the last claim; zero means none, so the first claim succeeds
    /// at once.
    last_claim_ms: AtomicU64,
}

impl SummaryWindow {
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window_ms: duration_ms(window),
            clock: Clock::new(),
            last_claim_ms: AtomicU64::new(0),
        }
    }

    /// Current time in this window's units. Never zero.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// The window length in milliseconds.
    #[must_use]
    pub const fn window_ms(&self) -> u64 {
        self.window_ms
    }

    /// Claim the window at `now_ms` if it is due. Returns whether this caller won.
    pub fn try_claim_at(&self, now_ms: u64) -> bool {
        let last = self.last_claim_ms.load(Ordering::Relaxed);
        if within(last, now_ms, self.window_ms) {
            return false;
        }
        self.last_claim_ms
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }
}

/// Rate limit for a condition that repeats per request.
///
/// The steady state (an occurrence within a burst, no line due) is a few
/// relaxed atomic operations and never takes a lock, so it is safe on paths
/// that run per request under exactly the load that triggers them. Only the
/// rare transitions, a line being due or a burst starting, take a small
/// mutex, which makes them atomic: exactly one line per window, and a late
/// burst reset can never reopen a window another thread already claimed. A
/// burst ends after a full window with no occurrence; the next burst's first
/// line logs at once. Counts at a burst boundary are approximate when callers
/// race it, which is acceptable for a log line.
#[derive(Debug)]
pub struct BurstLimiter {
    window_ms: u64,
    clock: Clock,
    last_line_ms: AtomicU64,
    last_occurrence_ms: AtomicU64,
    unreported: AtomicU64,
    transition: Mutex<()>,
}

impl Default for BurstLimiter {
    fn default() -> Self {
        Self::new(OPERATOR_EVENT_WINDOW)
    }
}

impl BurstLimiter {
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window_ms: duration_ms(window),
            clock: Clock::new(),
            last_line_ms: AtomicU64::new(0),
            last_occurrence_ms: AtomicU64::new(0),
            unreported: AtomicU64::new(0),
            transition: Mutex::new(()),
        }
    }

    /// Record one occurrence. Returns `Some(count)` when the caller should
    /// log: at the start of a burst and then at most once per window.
    /// `count` covers occurrences since the previous line in this burst,
    /// this one included, and is never zero.
    #[must_use]
    pub fn record(&self) -> Option<u64> {
        let now = self.clock.now_ms();
        // Exactly one caller sees the gap before a new burst: the swap.
        let previous = self.last_occurrence_ms.swap(now, Ordering::Relaxed);
        let burst_starts = !within(previous, now, self.window_ms);
        if !burst_starts
            && within(
                self.last_line_ms.load(Ordering::Relaxed),
                now,
                self.window_ms,
            )
        {
            self.unreported.fetch_add(1, Ordering::Relaxed);
            return None;
        }

        let _transition = self
            .transition
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Drop the old burst's unreported count, unless another thread has
        // already logged a line after the gap (its line opened this burst).
        if burst_starts && self.last_line_ms.load(Ordering::Relaxed) <= previous {
            self.unreported.store(0, Ordering::Relaxed);
            self.last_line_ms.store(0, Ordering::Relaxed);
        }
        self.unreported.fetch_add(1, Ordering::Relaxed);
        if within(
            self.last_line_ms.load(Ordering::Relaxed),
            now,
            self.window_ms,
        ) {
            return None;
        }
        self.last_line_ms.store(now, Ordering::Relaxed);
        Some(self.unreported.swap(0, Ordering::Relaxed).max(1))
    }
}

/// A condition that was active and has now cleared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cleared {
    /// Occurrences recorded while the condition was active.
    pub occurrences: u64,
    /// How long the condition lasted, from its first occurrence.
    pub duration: Duration,
}

/// One episode of a condition. Default means clear.
#[derive(Debug, Default)]
struct Episode {
    occurrences: u64,
    unreported: u64,
    started_ms: u64,
    last_line_ms: u64,
}

/// Decides when a recurring condition earns a log line, and when it clears.
///
/// All episode state changes under one small mutex, so a clear racing a
/// record cannot lose an episode's first line, report a zero count, or clear
/// an episode that was never announced. `record` is for failure paths and
/// periodic tasks; per-request conditions use [`BurstLimiter`].
///
/// An episode ends on evidence: [`Self::note_success`] records that the
/// thing that was failing has since worked, and [`Self::clear_if_recovered`]
/// ends the episode once that is true and a full window has passed since the
/// last failure. Both check atomics before locking, so calling them often
/// costs a couple of relaxed loads while healthy.
#[derive(Debug)]
pub struct EventGate {
    window_ms: u64,
    clock: Clock,
    /// Mirrors "an episode is open" for the lock-free fast paths.
    active: AtomicBool,
    /// Bumped by every failure.
    failure_generation: AtomicU64,
    /// The highest failure generation a success has observed. The episode
    /// has recovered when this equals `failure_generation`: a success that
    /// started before a newer failure saw an older generation and does not
    /// count, whatever order the threads ran in.
    success_generation: AtomicU64,
    /// Time of the last failure; the single record of it.
    last_occurrence_ms: AtomicU64,
    episode: Mutex<Episode>,
}

impl Default for EventGate {
    fn default() -> Self {
        Self::new(OPERATOR_EVENT_WINDOW)
    }
}

impl EventGate {
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window_ms: duration_ms(window),
            clock: Clock::new(),
            active: AtomicBool::new(false),
            failure_generation: AtomicU64::new(0),
            success_generation: AtomicU64::new(0),
            last_occurrence_ms: AtomicU64::new(0),
            episode: Mutex::new(Episode::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Episode> {
        self.episode
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record one occurrence.
    ///
    /// Returns `Some(count)` when the caller should log now: on the first
    /// occurrence of an episode, and then at most once per window. `count` is
    /// the number of occurrences since the previous line, this one included,
    /// and is never zero.
    #[must_use]
    pub fn record(&self) -> Option<u64> {
        let now = self.clock.now_ms();
        let mut episode = self.lock();
        if episode.occurrences == 0 {
            episode.started_ms = now;
            self.active.store(true, Ordering::Relaxed);
        }
        episode.occurrences += 1;
        episode.unreported += 1;
        self.last_occurrence_ms.store(now, Ordering::Relaxed);
        self.failure_generation.fetch_add(1, Ordering::Release);
        if within(episode.last_line_ms, now, self.window_ms) {
            return None;
        }
        episode.last_line_ms = now;
        Some(std::mem::take(&mut episode.unreported))
    }

    fn end(&self, episode: &mut Episode, now: u64) -> Option<Cleared> {
        if episode.occurrences == 0 {
            return None;
        }
        let cleared = Cleared {
            occurrences: episode.occurrences,
            duration: Duration::from_millis(now.saturating_sub(episode.started_ms)),
        };
        *episode = Episode::default();
        self.active.store(false, Ordering::Relaxed);
        Some(cleared)
    }

    /// End the episode now, for callers whose success signal is itself
    /// proof of recovery (a save that worked, a clean health-check cycle).
    ///
    /// Returns `Some` only on the transition from active to clear. The next
    /// occurrence after a clear logs at once.
    #[must_use]
    pub fn clear(&self) -> Option<Cleared> {
        if !self.is_active() {
            return None;
        }
        self.end(&mut self.lock(), self.clock.now_ms())
    }

    /// Note that the failing operation has since succeeded.
    ///
    /// Lock-free: the success records the failure generation it observed,
    /// so a success older than the latest failure is ignored.
    pub fn note_success(&self) {
        if !self.is_active() {
            return;
        }
        let observed = self.failure_generation.load(Ordering::Acquire);
        self.success_generation
            .fetch_max(observed, Ordering::Relaxed);
    }

    fn recovered(&self) -> bool {
        let failures = self.failure_generation.load(Ordering::Acquire);
        failures != 0 && self.success_generation.load(Ordering::Relaxed) == failures
    }

    /// End the episode if a success was seen after the last failure and a
    /// full window has passed since that failure.
    ///
    /// With partial failure successes interleave with failures; requiring a
    /// quiet window keeps the log from alternating failure and cleared
    /// lines. Requiring a success keeps a condition from "clearing" just
    /// because nothing was tried. Meant to be polled.
    #[must_use]
    pub fn clear_if_recovered(&self) -> Option<Cleared> {
        if !self.is_active() || !self.recovered() {
            return None;
        }
        let now = self.clock.now_ms();
        if within(
            self.last_occurrence_ms.load(Ordering::Relaxed),
            now,
            self.window_ms,
        ) {
            return None;
        }
        let mut episode = self.lock();
        // Re-check under the lock: a failure may have just been recorded.
        if !self.recovered()
            || within(
                self.last_occurrence_ms.load(Ordering::Relaxed),
                now,
                self.window_ms,
            )
        {
            return None;
        }
        self.end(&mut episode, now)
    }

    /// Whether an episode is open: the condition occurred and has not been
    /// cleared yet.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn first_occurrence_logs_then_once_per_window_with_count() {
        let gate = EventGate::new(Duration::from_secs(60));

        assert_eq!(gate.record(), Some(1));
        for _ in 0..9 {
            assert_eq!(gate.record(), None);
        }

        tokio::time::advance(Duration::from_secs(59)).await;
        assert_eq!(gate.record(), None);

        tokio::time::advance(Duration::from_secs(1)).await;
        // Nine suppressed, one at 59 s, and this one.
        assert_eq!(gate.record(), Some(11));
        assert_eq!(gate.record(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn clear_reports_only_the_transition() {
        let gate = EventGate::new(Duration::from_secs(60));
        assert_eq!(gate.clear(), None, "never active");

        assert_eq!(gate.record(), Some(1));
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(gate.record(), None);
        assert!(gate.is_active());

        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(
            gate.clear(),
            Some(Cleared {
                occurrences: 2,
                duration: Duration::from_secs(10),
            })
        );
        assert_eq!(gate.clear(), None, "already clear");
        assert!(!gate.is_active());
    }

    #[tokio::test(start_paused = true)]
    async fn occurrence_after_clear_logs_at_once() {
        let gate = EventGate::new(Duration::from_secs(60));
        assert_eq!(gate.record(), Some(1));
        let _ = gate.clear();
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(gate.record(), Some(1));
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_needs_a_success_and_a_quiet_window() {
        let gate = EventGate::new(Duration::from_secs(60));
        let mut lines = 0;
        // A failure and a success every 10 s for 3 minutes.
        for _ in 0..18 {
            if gate.record().is_some() {
                lines += 1;
            }
            gate.note_success();
            assert_eq!(gate.clear_if_recovered(), None, "a failure was recent");
            tokio::time::advance(Duration::from_secs(10)).await;
        }
        assert_eq!(lines, 3, "one line per minute, not one per failure");

        tokio::time::advance(Duration::from_secs(50)).await;
        let cleared = gate.clear_if_recovered().expect("quiet and recovered");
        assert_eq!(cleared.occurrences, 18);
        assert!(!gate.is_active());
    }

    #[tokio::test(start_paused = true)]
    async fn quiet_without_a_success_does_not_clear() {
        let gate = EventGate::new(Duration::from_secs(60));
        assert_eq!(gate.record(), Some(1));
        tokio::time::advance(Duration::from_secs(600)).await;
        assert_eq!(gate.clear_if_recovered(), None, "nothing worked yet");
        gate.note_success();
        assert!(gate.clear_if_recovered().is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_failure_after_the_success_resets_recovery() {
        let gate = EventGate::new(Duration::from_secs(60));
        assert_eq!(gate.record(), Some(1));
        gate.note_success();
        let _ = gate.record();
        tokio::time::advance(Duration::from_secs(120)).await;
        assert_eq!(gate.clear_if_recovered(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn burst_limiter_counts_each_burst_separately() {
        let limiter = BurstLimiter::new(Duration::from_secs(60));
        assert_eq!(limiter.record(), Some(1));
        for _ in 0..5 {
            assert_eq!(limiter.record(), None);
        }
        tokio::time::advance(Duration::from_secs(120)).await;
        // The five suppressed occurrences belong to the old burst.
        assert_eq!(limiter.record(), Some(1));
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(limiter.record(), None);
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(limiter.record(), Some(2));
    }

    /// The steady state must not take the transition lock. Hold the lock
    /// here and record on another thread: if the steady state ever locked,
    /// the worker would block and this test fails on the timeout instead of
    /// hanging.
    #[test]
    fn burst_limiter_steady_state_takes_no_lock() {
        let limiter = std::sync::Arc::new(BurstLimiter::new(Duration::from_secs(3600)));
        assert_eq!(limiter.record(), Some(1));
        let held = limiter.transition.lock().unwrap();

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = {
            let limiter = std::sync::Arc::clone(&limiter);
            std::thread::spawn(move || {
                let lines = (0..10_000).filter(|_| limiter.record().is_some()).count();
                let _ = done_tx.send(lines);
            })
        };
        let lines = done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("steady-state record blocked on the transition lock");
        drop(held);
        worker.join().unwrap();
        assert_eq!(lines, 0);
    }

    /// Bursts separated by a quiet window, each started by many threads
    /// released at once to race the boundary, log exactly one line each.
    /// The limiter reads real time on these threads; an attempt where a
    /// burst outlived its window proves nothing and is retried, and the test
    /// fails if no attempt fits.
    #[test]
    fn burst_limiter_logs_exactly_one_line_per_burst_under_contention() {
        const ATTEMPTS: usize = 5;
        for _ in 0..ATTEMPTS {
            if let Some(lines) = run_contended_bursts() {
                assert_eq!(lines, 2, "one line per burst");
                return;
            }
        }
        panic!("no attempt finished its bursts inside the window");
    }

    /// Two contended bursts on a fresh limiter: `Some(lines)`, or `None` if
    /// a burst outlived its window.
    fn run_contended_bursts() -> Option<usize> {
        const THREADS: usize = 8;
        const BURSTS: usize = 2;
        let window = Duration::from_millis(250);
        let limiter = std::sync::Arc::new(BurstLimiter::new(window));
        let mut lines = 0;
        for burst in 0..BURSTS {
            if burst > 0 {
                std::thread::sleep(window * 3);
            }
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(THREADS));
            let started = std::time::Instant::now();
            let mut handles = Vec::with_capacity(THREADS);
            for _ in 0..THREADS {
                let limiter = std::sync::Arc::clone(&limiter);
                let barrier = std::sync::Arc::clone(&barrier);
                handles.push(std::thread::spawn(move || {
                    barrier.wait();
                    (0..500).filter(|_| limiter.record().is_some()).count()
                }));
            }
            lines += handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .sum::<usize>();
            if started.elapsed() >= window {
                return None;
            }
        }
        Some(lines)
    }

    /// A success that observed an older failure generation is ignored.
    #[tokio::test(start_paused = true)]
    async fn stale_success_does_not_recover_a_newer_failure() {
        let gate = EventGate::new(Duration::from_secs(60));
        assert_eq!(gate.record(), Some(1));
        // A success that started before the next failure: it observed the
        // first generation...
        let observed = gate.failure_generation.load(Ordering::Acquire);
        let _ = gate.record();
        // ...and lands after it.
        gate.success_generation
            .fetch_max(observed, Ordering::Relaxed);
        tokio::time::advance(Duration::from_secs(120)).await;
        assert_eq!(gate.clear_if_recovered(), None);
        gate.note_success();
        assert!(gate.clear_if_recovered().is_some());
    }

    /// Heavy contention inside one long window still yields one line.
    #[test]
    fn burst_limiter_is_one_line_per_window_under_contention() {
        let limiter = std::sync::Arc::new(BurstLimiter::new(Duration::from_secs(3600)));
        let mut handles = Vec::with_capacity(8);
        for _ in 0..8 {
            let limiter = std::sync::Arc::clone(&limiter);
            handles.push(std::thread::spawn(move || {
                (0..100_000).filter(|_| limiter.record().is_some()).count()
            }));
        }
        let lines: usize = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .sum();
        assert_eq!(lines, 1);
    }

    #[test]
    fn racing_recorders_get_one_line() {
        let gate = std::sync::Arc::new(EventGate::new(Duration::from_secs(3600)));
        // Spawn every thread before joining any so they actually race.
        let mut handles = Vec::with_capacity(8);
        for _ in 0..8 {
            let gate = std::sync::Arc::clone(&gate);
            handles.push(std::thread::spawn(move || {
                (0..1000).filter(|_| gate.record().is_some()).count()
            }));
        }
        let lines: usize = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .sum();
        assert_eq!(lines, 1);
    }

    /// Clears racing records must keep every occurrence accounted for: each
    /// cleared episode was announced by a line, no line reports zero, and the
    /// cleared totals plus the open episode add up to every record.
    #[test]
    fn racing_clear_and_record_keep_episodes_consistent() {
        const RECORDERS: u64 = 4;
        const RECORDS_EACH: u64 = 5_000;

        let gate = std::sync::Arc::new(EventGate::new(Duration::from_secs(3600)));
        let done = std::sync::Arc::new(AtomicBool::new(false));

        let clearer = {
            let gate = std::sync::Arc::clone(&gate);
            let done = std::sync::Arc::clone(&done);
            std::thread::spawn(move || {
                let mut cleared = Vec::new();
                while !done.load(Ordering::Relaxed) {
                    if let Some(episode) = gate.clear() {
                        cleared.push(episode.occurrences);
                    }
                    std::thread::yield_now();
                }
                cleared
            })
        };
        let mut recorders = Vec::new();
        for _ in 0..RECORDERS {
            let gate = std::sync::Arc::clone(&gate);
            recorders.push(std::thread::spawn(move || {
                let mut counts = Vec::new();
                for _ in 0..RECORDS_EACH {
                    if let Some(count) = gate.record() {
                        counts.push(count);
                    }
                }
                counts
            }));
        }
        let counts: Vec<u64> = recorders
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect();
        done.store(true, Ordering::Relaxed);
        let cleared = clearer.join().unwrap();
        let open = gate.clear().map_or(0, |episode| episode.occurrences);

        assert!(counts.iter().all(|&count| count > 0), "no zero-count line");
        assert!(cleared.iter().all(|&occurrences| occurrences > 0));
        // With an hour-long window, each episode logs exactly its first line.
        let episodes = cleared.len() + usize::from(open > 0);
        assert_eq!(counts.len(), episodes, "every episode announced once");
        assert_eq!(
            cleared.iter().sum::<u64>() + open,
            RECORDERS * RECORDS_EACH,
            "every occurrence belongs to exactly one episode"
        );
    }
}
