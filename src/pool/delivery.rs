//! How fast a backend actually delivers articles, and what else holds its
//! connections.
//!
//! Routing and pipelining decisions need the provider's per-connection
//! delivery rate, but the obvious estimators are biased:
//!
//! - Averaging per-exchange `bytes / time` overstates slow, erratic backends
//!   (a 1 s and a 30 s transfer of the same article average to half the 1 s
//!   rate, while the true rate is 1/15 of it). This module keeps decayed
//!   *sums* of bytes and busy seconds and divides them, which weights each
//!   exchange by its duration.
//! - A slow client makes every backend look slow, so time spent blocked
//!   writing to the client is measured separately and subtracted.
//! - Only `ARTICLE`/`BODY` deliveries are recorded; `430`s, `STAT`s, `HEAD`s
//!   and overview replies would mix in a different population.
//!
//! Two horizons are kept: a short one that follows a single download (a
//! provider is fast for one release and slow for the next) and a long one
//! that remembers a backend that has not been used recently.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Decay constant of the short horizon.
pub const SHORT_HORIZON: Duration = Duration::from_secs(5);
/// Decay constant of the long horizon.
pub const LONG_HORIZON: Duration = Duration::from_secs(300);

/// Exchanges shorter than this are not used: their timing is dominated by
/// scheduling noise and would divide by almost nothing.
const MIN_BUSY: Duration = Duration::from_millis(1);

/// A horizon's estimate is reported only once it holds this many busy
/// seconds of (decayed) samples.
const MIN_BUSY_SECS_FOR_ESTIMATE: f64 = 0.5;

/// Weight 1/8 for the first-byte average, as for the service-time average.
const TTFB_EWMA_SHIFT: u32 = 3;

#[derive(Debug, Clone, Copy, Default)]
struct DecayedSums {
    bytes: f64,
    busy_secs: f64,
}

impl DecayedSums {
    fn decay(&mut self, elapsed: Duration, horizon: Duration) {
        let factor = (-elapsed.as_secs_f64() / horizon.as_secs_f64()).exp();
        self.bytes *= factor;
        self.busy_secs *= factor;
    }

    fn rate(self) -> Option<f64> {
        (self.busy_secs >= MIN_BUSY_SECS_FOR_ESTIMATE)
            .then(|| self.bytes / self.busy_secs)
            .filter(|rate| rate.is_finite())
    }
}

#[derive(Debug)]
struct DeliveryState {
    short: DecayedSums,
    long: DecayedSums,
    last_sample: Option<Instant>,
}

/// One horizon pair as of a moment, for export.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DeliverySnapshot {
    /// Bytes per busy second per connection over the short horizon.
    pub short_bytes_per_sec: Option<f64>,
    /// Bytes per busy second per connection over the long horizon.
    pub long_bytes_per_sec: Option<f64>,
    /// Moving average of time to first response, in seconds.
    pub ttfb_secs: Option<f64>,
    /// Total time article writes spent blocked on the client.
    pub client_stall_secs: f64,
    /// Connections held by client leases between commands.
    pub leased_idle: usize,
    /// Article deliveries observed, including ones too short to use.
    pub observed: u64,
}

/// Delivery measurements for one backend, shared by every clone of its
/// provider.
#[derive(Debug)]
pub struct DeliveryStats {
    state: Mutex<DeliveryState>,
    /// First-response time average in microseconds; zero means no sample.
    ttfb_micros: AtomicU64,
    client_stall_micros: AtomicU64,
    leased_idle: AtomicUsize,
    /// Article deliveries observed, including ones too short to use.
    observed: AtomicU64,
}

impl Default for DeliveryStats {
    fn default() -> Self {
        Self {
            state: Mutex::new(DeliveryState {
                short: DecayedSums::default(),
                long: DecayedSums::default(),
                last_sample: None,
            }),
            ttfb_micros: AtomicU64::new(0),
            client_stall_micros: AtomicU64::new(0),
            leased_idle: AtomicUsize::new(0),
            observed: AtomicU64::new(0),
        }
    }
}

impl DeliveryStats {
    /// Record one delivered article: `bytes` written to the client over
    /// `busy` (command sent to response complete), of which `client_stall`
    /// was spent blocked on the client.
    pub fn record_article(&self, bytes: u64, busy: Duration, client_stall: Duration, now: Instant) {
        self.observed.fetch_add(1, Ordering::Relaxed);
        self.client_stall_micros.fetch_add(
            u64::try_from(client_stall.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        let provider_busy = busy.saturating_sub(client_stall);
        if provider_busy < MIN_BUSY || bytes == 0 {
            return;
        }
        #[allow(clippy::cast_precision_loss)] // A rate estimate; exact bytes are not needed.
        let bytes = bytes as f64;
        let busy_secs = provider_busy.as_secs_f64();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(last) = state.last_sample {
            let elapsed = now.saturating_duration_since(last);
            state.short.decay(elapsed, SHORT_HORIZON);
            state.long.decay(elapsed, LONG_HORIZON);
        }
        let DeliveryState { short, long, .. } = &mut *state;
        for sums in [short, long] {
            sums.bytes += bytes;
            sums.busy_secs += busy_secs;
        }
        state.last_sample = Some(state.last_sample.map_or(now, |last| last.max(now)));
    }

    /// Add time a write spent blocked on the client, for a transfer that did
    /// not complete (and so is not a delivery sample).
    pub fn record_client_stall(&self, client_stall: Duration) {
        self.client_stall_micros.fetch_add(
            u64::try_from(client_stall.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    /// Fold one time-to-first-response sample into the average.
    pub fn record_ttfb(&self, ttfb: Duration) {
        let sample = u64::try_from(ttfb.as_micros()).unwrap_or(u64::MAX).max(1);
        let current = self.ttfb_micros.load(Ordering::Relaxed);
        let next = if current == 0 {
            sample
        } else if sample >= current {
            current + ((sample - current) >> TTFB_EWMA_SHIFT)
        } else {
            current - ((current - sample) >> TTFB_EWMA_SHIFT)
        };
        self.ttfb_micros.store(next.max(1), Ordering::Relaxed);
    }

    /// A client lease started holding one of this backend's connections idle.
    pub fn lease_started(&self) {
        self.leased_idle.fetch_add(1, Ordering::Relaxed);
    }

    /// A client lease stopped holding a connection idle.
    pub fn lease_ended(&self) {
        self.leased_idle.fetch_sub(1, Ordering::Relaxed);
    }

    /// Estimates as of `now`. Decay cancels in the bytes/seconds ratio, so
    /// the rates reflect the last samples; a stale short horizon is reported
    /// as unknown so it is not mistaken for current speed.
    #[must_use]
    pub fn snapshot(&self, now: Instant) -> DeliverySnapshot {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let age = state
            .last_sample
            .map(|last| now.saturating_duration_since(last));
        let short_fresh = age.is_some_and(|age| age <= SHORT_HORIZON * 2);
        #[allow(clippy::cast_precision_loss)]
        let ttfb_secs = match self.ttfb_micros.load(Ordering::Relaxed) {
            0 => None,
            micros => Some(micros as f64 / 1_000_000.0),
        };
        #[allow(clippy::cast_precision_loss)]
        let client_stall_secs =
            self.client_stall_micros.load(Ordering::Relaxed) as f64 / 1_000_000.0;
        DeliverySnapshot {
            short_bytes_per_sec: short_fresh.then(|| state.short.rate()).flatten(),
            long_bytes_per_sec: state.long.rate(),
            ttfb_secs,
            client_stall_secs,
            leased_idle: self.leased_idle.load(Ordering::Relaxed),
            observed: self.observed.load(Ordering::Relaxed),
        }
    }
}

/// Counts a client lease as holding a connection idle for as long as it
/// lives.
#[derive(Debug)]
pub struct IdleLease {
    stats: std::sync::Arc<super::admission::BackendLoad>,
}

impl IdleLease {
    #[must_use]
    pub fn new(stats: std::sync::Arc<super::admission::BackendLoad>) -> Self {
        stats.delivery().lease_started();
        Self { stats }
    }
}

impl Drop for IdleLease {
    fn drop(&mut self) {
        self.stats.delivery().lease_ended();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1_000_000;

    #[test]
    fn rate_is_bytes_over_busy_time_not_a_mean_of_ratios() {
        let stats = DeliveryStats::default();
        let t0 = Instant::now();
        // The same 1 MB article in 1 s and in 30 s: the backend moved 2 MB
        // in 31 s, not "half of 1 MB/s".
        stats.record_article(MB, Duration::from_secs(1), Duration::ZERO, t0);
        stats.record_article(MB, Duration::from_secs(30), Duration::ZERO, t0);
        let rate = stats.snapshot(t0).long_bytes_per_sec.unwrap();
        assert!((rate - 2.0 * MB as f64 / 31.0).abs() < 1.0, "{rate}");
    }

    #[test]
    fn client_stall_is_excluded_from_provider_speed() {
        let stats = DeliveryStats::default();
        let t0 = Instant::now();
        // 2 s busy, 1 s of it waiting on the client: the provider did 1 MB/s.
        stats.record_article(MB, Duration::from_secs(2), Duration::from_secs(1), t0);
        let snap = stats.snapshot(t0);
        assert!((snap.long_bytes_per_sec.unwrap() - MB as f64).abs() < 1.0);
        assert!((snap.client_stall_secs - 1.0).abs() < 1e-6);
    }

    #[test]
    fn short_horizon_follows_a_speed_change_the_long_one_remembers() {
        let stats = DeliveryStats::default();
        let mut now = Instant::now();
        for _ in 0..600 {
            now += Duration::from_millis(500);
            stats.record_article(MB, Duration::from_secs(1), Duration::ZERO, now);
        }
        // The provider slows to a tenth for 10 s.
        for _ in 0..20 {
            now += Duration::from_millis(500);
            stats.record_article(MB, Duration::from_secs(10), Duration::ZERO, now);
        }
        let snap = stats.snapshot(now);
        let short = snap.short_bytes_per_sec.unwrap();
        let long = snap.long_bytes_per_sec.unwrap();
        assert!(
            short < 0.15 * MB as f64,
            "short horizon tracked the drop: {short}"
        );
        assert!(
            long > 0.3 * MB as f64,
            "long horizon still remembers: {long}"
        );
    }

    #[test]
    fn stale_short_horizon_is_unknown_and_tiny_or_empty_samples_are_ignored() {
        let stats = DeliveryStats::default();
        let t0 = Instant::now();
        assert_eq!(stats.snapshot(t0), DeliverySnapshot::default());
        stats.record_article(MB, Duration::from_micros(10), Duration::ZERO, t0);
        stats.record_article(0, Duration::from_secs(1), Duration::ZERO, t0);
        assert_eq!(stats.snapshot(t0).long_bytes_per_sec, None);

        stats.record_article(MB, Duration::from_secs(1), Duration::ZERO, t0);
        assert!(stats.snapshot(t0).short_bytes_per_sec.is_some());
        let later = t0 + SHORT_HORIZON * 3;
        assert_eq!(stats.snapshot(later).short_bytes_per_sec, None);
        assert!(stats.snapshot(later).long_bytes_per_sec.is_some());
    }

    #[test]
    fn ttfb_average_and_lease_gauge() {
        let stats = DeliveryStats::default();
        stats.record_ttfb(Duration::from_millis(400));
        assert!((stats.snapshot(Instant::now()).ttfb_secs.unwrap() - 0.4).abs() < 1e-3);
        stats.lease_started();
        stats.lease_started();
        stats.lease_ended();
        assert_eq!(stats.snapshot(Instant::now()).leased_idle, 1);
    }
}
