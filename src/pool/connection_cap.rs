//! Adaptive per-backend connection cap.
//!
//! `max_connections` is what the operator believes the provider account
//! allows, but the real limit moves: another client shares the account, the
//! provider lowers it, or a session the provider still counts is slow to
//! close. A pool that always regrows to `max_connections` keeps tripping
//! "too many connections", and [login backoff](super::login_backoff) only
//! spaces those failures out in time.
//!
//! With `connection_cap = "adaptive"` the pool probes for the provider's real
//! limit and then holds just under it:
//!
//! - **Probe.** Until the provider first refuses a login, the cap grows by
//!   one every [`PROBE_STEP`] while the pool is full and every connection is
//!   in use. A cap raised while nothing tested it would let the next burst
//!   open a batch of connections the provider never allowed.
//! - **Back off.** A connection-limit login refusal (see
//!   [`crate::connection_error::LoginRefusal`]) sets the cap to the
//!   connections the provider had accepted at that moment. A bad password
//!   or any other refusal leaves it alone: fewer connections would not fix it.
//! - **Climb again.** After a refusal the cap holds for [`CLIMB_STEP`], then
//!   grows by one every [`CLIMB_STEP`] until the next refusal. Below
//!   `max_connections` it climbs whatever the demand; at or above it, only
//!   while the pool is saturated.
//!
//! There is no ceiling above `max_connections` other than the provider's own
//! refusal, which brings the cap straight back down. A provider that never
//! refuses ends up with as many connections as the load keeps busy.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::time::Instant;

/// Time between steps while probing for the limit, before the provider
/// has refused any login.
pub const PROBE_STEP: Duration = Duration::from_secs(60);

/// After a refusal, time the cap holds before climbing, and then between
/// steps. Each step past the last refusal is one login the provider may
/// refuse again.
pub const CLIMB_STEP: Duration = Duration::from_secs(5 * 60);

/// How often a backend's cap is re-evaluated.
pub const TICK: Duration = Duration::from_secs(15);

/// Whether a backend's pool size follows login refusals.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConnectionCapPolicy {
    /// The pool may always grow to `max_connections` (default).
    #[default]
    Fixed,
    /// Probe upward while saturated until the provider refuses a login,
    /// drop to the accepted count, wait, then climb one step at a time.
    Adaptive,
}

impl ConnectionCapPolicy {
    #[must_use]
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Fixed)
    }

    /// Get the configuration spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fixed => "fixed",
            Self::Adaptive => "adaptive",
        }
    }
}

impl std::fmt::Display for ConnectionCapPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ConnectionCapPolicy {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "fixed" => Ok(Self::Fixed),
            "adaptive" => Ok(Self::Adaptive),
            other => anyhow::bail!("expected \"fixed\" or \"adaptive\", got \"{other}\""),
        }
    }
}

/// Whether the pool is using everything the cap allows right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolDemand {
    /// Every allowed connection is open and checked out.
    Saturated,
    /// Some allowed connections are idle or not opened.
    Headroom,
}

/// A change the caller should apply to the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapChange {
    Lowered { from: usize, to: usize },
    Raised { from: usize, to: usize },
}

/// When the cap last moved and whether the provider has refused a login
/// yet, which picks the step interval.
#[derive(Debug, Clone, Copy)]
struct CapClock {
    at: Instant,
    refused: bool,
}

/// Current cap and when it last moved, shared by every clone of a provider.
#[derive(Debug)]
pub struct ConnectionCap {
    policy: ConnectionCapPolicy,
    configured: usize,
    cap: AtomicUsize,
    /// When the cap last moved (or the cap was created). Guarded so a refusal
    /// and a tick cannot both move the cap from the same reading.
    last_change: Mutex<CapClock>,
    reductions: AtomicU64,
    /// Held while computing and applying a pool size, so a resize computed
    /// from an older cap or cooldown count can never land after a newer one.
    resize: Mutex<()>,
}

impl ConnectionCap {
    #[must_use]
    pub fn new(policy: ConnectionCapPolicy, configured: usize) -> Self {
        Self {
            policy,
            configured,
            cap: AtomicUsize::new(configured),
            last_change: Mutex::new(CapClock {
                at: Instant::now(),
                refused: false,
            }),
            reductions: AtomicU64::new(0),
            resize: Mutex::new(()),
        }
    }

    #[must_use]
    pub const fn policy(&self) -> ConnectionCapPolicy {
        self.policy
    }

    /// The configured `max_connections`.
    #[must_use]
    pub const fn configured(&self) -> usize {
        self.configured
    }

    /// Connections the pool may hold now, before replacement cooldowns.
    #[must_use]
    pub fn current(&self) -> usize {
        self.cap.load(Ordering::Acquire)
    }

    /// Serialize pool resizes. Callers read the cap and cooldowns and resize
    /// while holding this, so the last resize always reflects the latest state.
    pub fn lock_resizes(&self) -> std::sync::MutexGuard<'_, ()> {
        self.resize
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Times a connection-limit login refusal lowered the cap.
    #[must_use]
    pub fn reductions(&self) -> u64 {
        self.reductions.load(Ordering::Relaxed)
    }

    /// A login was refused while the provider had accepted `open`
    /// connections from this pool. Lowers the cap to that count (never below
    /// one) and restarts the climb timer.
    pub fn on_login_refused(&self, open: usize, now: Instant) -> Option<CapChange> {
        if self.policy == ConnectionCapPolicy::Fixed {
            return None;
        }
        let mut last_change = self
            .last_change
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Any refusal restarts the climb, even one that does not lower the cap.
        *last_change = CapClock {
            at: now,
            refused: true,
        };
        let from = self.current();
        let to = open.max(1);
        if to >= from {
            return None;
        }
        self.cap.store(to, Ordering::Release);
        self.reductions.fetch_add(1, Ordering::Relaxed);
        Some(CapChange::Lowered { from, to })
    }

    /// Re-evaluate the cap. Grows it by one when the step interval has
    /// passed since the last change: [`PROBE_STEP`] before any refusal,
    /// [`CLIMB_STEP`] after one. At or above `max_connections` it grows only
    /// while the pool is saturated.
    pub fn on_tick(&self, demand: PoolDemand, now: Instant) -> Option<CapChange> {
        if self.policy == ConnectionCapPolicy::Fixed {
            return None;
        }
        let mut last_change = self
            .last_change
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let from = self.current();
        if from >= self.configured && demand != PoolDemand::Saturated {
            return None;
        }
        let interval = if last_change.refused {
            CLIMB_STEP
        } else {
            PROBE_STEP
        };
        if now.saturating_duration_since(last_change.at) < interval {
            return None;
        }
        let to = from.saturating_add(1);
        self.cap.store(to, Ordering::Release);
        last_change.at = now;
        Some(CapChange::Raised { from, to })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adaptive(configured: usize) -> (ConnectionCap, Instant) {
        let cap = ConnectionCap::new(ConnectionCapPolicy::Adaptive, configured);
        let start = cap.last_change.lock().unwrap().at;
        (cap, start)
    }

    #[test]
    fn fixed_never_moves() {
        let cap = ConnectionCap::new(ConnectionCapPolicy::Fixed, 80);
        let now = Instant::now();
        assert_eq!(cap.on_login_refused(50, now), None);
        assert_eq!(
            cap.on_tick(PoolDemand::Saturated, now + Duration::from_secs(3600)),
            None
        );
        assert_eq!(cap.current(), 80);
    }

    #[test]
    fn refusal_drops_to_the_accepted_count_at_once() {
        let (cap, start) = adaptive(80);
        assert_eq!(
            cap.on_login_refused(57, start),
            Some(CapChange::Lowered { from: 80, to: 57 })
        );
        assert_eq!(cap.current(), 57);
        assert_eq!(cap.reductions(), 1);

        // A later refusal at a higher count than the cap changes nothing.
        assert_eq!(cap.on_login_refused(60, start), None);
        // One at a lower count (another client grew) drops further.
        assert_eq!(
            cap.on_login_refused(40, start),
            Some(CapChange::Lowered { from: 57, to: 40 })
        );
        // Never to zero: one connection is always allowed to probe.
        assert_eq!(
            cap.on_login_refused(0, start),
            Some(CapChange::Lowered { from: 40, to: 1 })
        );
    }

    #[test]
    fn probes_up_one_a_minute_while_saturated_before_any_refusal() {
        let (cap, start) = adaptive(80);
        let almost = start + PROBE_STEP - Duration::from_secs(1);
        assert_eq!(cap.on_tick(PoolDemand::Saturated, almost), None);

        let mut now = start;
        for expected in 81..=83 {
            now += PROBE_STEP;
            assert_eq!(
                cap.on_tick(PoolDemand::Saturated, now),
                Some(CapChange::Raised {
                    from: expected - 1,
                    to: expected
                })
            );
        }
        // An unsaturated pool never probes: nothing would test the new slot.
        now += CLIMB_STEP;
        assert_eq!(cap.on_tick(PoolDemand::Headroom, now), None);
        assert_eq!(cap.current(), 83);
    }

    #[test]
    fn after_a_refusal_holds_five_minutes_then_climbs_one_every_five() {
        let (cap, start) = adaptive(80);
        let mut now = start;
        for _ in 0..5 {
            now += PROBE_STEP;
            cap.on_tick(PoolDemand::Saturated, now);
        }
        assert_eq!(cap.current(), 85);

        // The provider refuses the 84th login with 83 open.
        assert_eq!(
            cap.on_login_refused(83, now),
            Some(CapChange::Lowered { from: 85, to: 83 })
        );
        // Probing speed is over: a minute later nothing moves...
        assert_eq!(cap.on_tick(PoolDemand::Saturated, now + PROBE_STEP), None);
        assert_eq!(
            cap.on_tick(
                PoolDemand::Saturated,
                now + CLIMB_STEP - Duration::from_secs(1)
            ),
            None
        );
        // ...and after the hold it climbs one step per interval.
        let first = now + CLIMB_STEP;
        assert_eq!(
            cap.on_tick(PoolDemand::Saturated, first),
            Some(CapChange::Raised { from: 83, to: 84 })
        );
        assert_eq!(cap.on_tick(PoolDemand::Saturated, first + PROBE_STEP), None);
        assert_eq!(
            cap.on_tick(PoolDemand::Saturated, first + CLIMB_STEP),
            Some(CapChange::Raised { from: 84, to: 85 })
        );
    }

    #[test]
    fn a_refusal_at_the_cap_ends_probing_without_lowering_it() {
        let (cap, start) = adaptive(80);
        let now = start + PROBE_STEP;
        cap.on_tick(PoolDemand::Saturated, now);
        assert_eq!(cap.current(), 81);
        // The 82nd login is refused with 81 open: the cap is already right.
        assert_eq!(cap.on_login_refused(81, now), None);
        assert_eq!(cap.reductions(), 0);
        // But the climb now waits the full back-off step, not the probe step.
        assert_eq!(cap.on_tick(PoolDemand::Saturated, now + PROBE_STEP), None);
        assert_eq!(
            cap.on_tick(PoolDemand::Saturated, now + CLIMB_STEP),
            Some(CapChange::Raised { from: 81, to: 82 })
        );
    }

    #[test]
    fn below_the_configured_size_climbs_back_without_demand() {
        let (cap, start) = adaptive(80);
        cap.on_login_refused(78, start);
        let mut now = start;
        for expected in 79..=80 {
            now += CLIMB_STEP;
            assert_eq!(
                cap.on_tick(PoolDemand::Headroom, now),
                Some(CapChange::Raised {
                    from: expected - 1,
                    to: expected
                })
            );
        }
        now += CLIMB_STEP;
        assert_eq!(cap.on_tick(PoolDemand::Headroom, now), None);
        assert_eq!(cap.current(), 80);
    }

    #[test]
    fn a_refusal_restarts_the_climb() {
        let (cap, start) = adaptive(80);
        cap.on_login_refused(50, start);
        let later = start + CLIMB_STEP - Duration::from_secs(5);
        cap.on_login_refused(55, later);
        assert_eq!(
            cap.on_tick(PoolDemand::Headroom, start + CLIMB_STEP),
            None,
            "the second refusal restarted the hold"
        );
        assert_eq!(
            cap.on_tick(PoolDemand::Headroom, later + CLIMB_STEP),
            Some(CapChange::Raised { from: 50, to: 51 })
        );
    }

    #[test]
    fn policy_parses_both_spellings() {
        assert_eq!(
            "Adaptive".parse::<ConnectionCapPolicy>().unwrap(),
            ConnectionCapPolicy::Adaptive
        );
        assert_eq!(
            "fixed".parse::<ConnectionCapPolicy>().unwrap(),
            ConnectionCapPolicy::Fixed
        );
        assert!("auto".parse::<ConnectionCapPolicy>().is_err());
    }
}
