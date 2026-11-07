//! Backoff after a backend refuses to log a connection in.
//!
//! A provider that answers a login with `502 connection limit reached` (or any
//! other refusal) gives the same answer to the next login until another
//! session on the account goes away. Without a gate, every request that falls
//! through to that backend opens a fresh TLS session, authenticates, fails,
//! and drops the socket. That hammers the provider, piles up closing sockets,
//! and can trip account-wide failed-login blocking.
//!
//! Replacement cooldown does not cover this case: it shrinks the pool after a
//! live connection is retired, while a refused login never produces a pooled
//! connection to retire. The gate instead makes pool creation fail locally for
//! a growing interval, so routing moves on without touching the network.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::time::Instant;

/// First wait after a refused login.
pub(crate) const INITIAL_DELAY: Duration = Duration::from_secs(5);

/// Longest wait between logins while the backend keeps refusing.
pub(crate) const MAX_DELAY: Duration = Duration::from_secs(60);

/// How long callers that lose the race for the probe are told to wait.
pub(crate) const PROBE_RETRY: Duration = Duration::from_secs(1);

/// Whether a caller may log in right now.
#[derive(Debug)]
pub(crate) enum Admission<'a> {
    /// Log in, then report the outcome on the attempt.
    Proceed(LoginAttempt<'a>),
    /// Do not touch the network; try again after this long.
    Wait(Duration),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeSlot {
    Free,
    Taken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttemptRole {
    /// No refusal is on record.
    Normal,
    /// The window closed after refusals; this login alone decides whether to resume.
    Probe,
}

#[derive(Debug)]
struct State {
    /// Refused windows in a row. Zero means the gate is open.
    refusals: u32,
    /// No login may start before this instant.
    retry_not_before: Option<Instant>,
    probe: ProbeSlot,
}

impl Default for State {
    fn default() -> Self {
        Self {
            refusals: 0,
            retry_not_before: None,
            probe: ProbeSlot::Free,
        }
    }
}

/// Login gate shared by every pool slot of one backend.
#[derive(Debug, Default)]
pub(crate) struct LoginBackoff {
    state: Mutex<State>,
}

impl LoginBackoff {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Decide whether a login may start now.
    ///
    /// While refusals are on record, only one caller at a time is admitted as
    /// the probe once the window closes. Otherwise every pool slot waiting on
    /// the backend would log in together at each window boundary and bring the
    /// failed-login storm straight back.
    pub(crate) fn admit(&self) -> Admission<'_> {
        let now = Instant::now();
        let mut state = self.lock();
        if let Some(left) = state
            .retry_not_before
            .map(|deadline| deadline.saturating_duration_since(now))
            .filter(|left| !left.is_zero())
        {
            return Admission::Wait(left);
        }
        if state.refusals == 0 {
            return Admission::Proceed(LoginAttempt::new(self, AttemptRole::Normal));
        }
        if state.probe == ProbeSlot::Taken {
            return Admission::Wait(PROBE_RETRY);
        }
        state.probe = ProbeSlot::Taken;
        Admission::Proceed(LoginAttempt::new(self, AttemptRole::Probe))
    }

    /// Attempts already in flight when a window opens fail together; only the
    /// first of them moves the schedule, so a pool of N concurrent creators
    /// does not jump N steps.
    fn record_refusal(&self, role: AttemptRole) -> Duration {
        let now = Instant::now();
        let mut state = self.lock();
        if role == AttemptRole::Probe {
            state.probe = ProbeSlot::Free;
        }
        if let Some(deadline) = state.retry_not_before
            && now < deadline
        {
            return deadline - now;
        }
        let delay = delay_after(state.refusals);
        state.refusals = state.refusals.saturating_add(1);
        state.retry_not_before = Some(now + delay);
        delay
    }

    fn record_success(&self) {
        *self.lock() = State::default();
    }

    /// Close the current window without waiting it out, keeping the refusals
    /// on record so the next login is a probe.
    #[cfg(test)]
    pub(crate) fn expire_window(&self) {
        self.lock().retry_not_before = None;
    }

    fn release_probe(&self) {
        self.lock().probe = ProbeSlot::Free;
    }
}

fn delay_after(refusals: u32) -> Duration {
    INITIAL_DELAY
        .saturating_mul(1u32.checked_shl(refusals).unwrap_or(u32::MAX))
        .min(MAX_DELAY)
}

/// One admitted login.
///
/// Dropping it without an outcome (a network error, a setup timeout, or a
/// cancelled `create()` future) says nothing about the account, so a probe
/// slot is handed to the next caller instead of leaving the gate shut.
#[derive(Debug)]
pub(crate) struct LoginAttempt<'a> {
    gate: &'a LoginBackoff,
    role: AttemptRole,
    outcome: Outcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Pending,
    Reported,
}

impl<'a> LoginAttempt<'a> {
    const fn new(gate: &'a LoginBackoff, role: AttemptRole) -> Self {
        Self {
            gate,
            role,
            outcome: Outcome::Pending,
        }
    }

    #[cfg(test)]
    pub(crate) const fn role(&self) -> AttemptRole {
        self.role
    }

    /// The backend let this session in: forget earlier refusals.
    pub(crate) fn succeeded(mut self) {
        self.outcome = Outcome::Reported;
        self.gate.record_success();
    }

    /// The backend refused this login. Returns how long new logins now wait.
    pub(crate) fn refused(mut self) -> Duration {
        self.outcome = Outcome::Reported;
        self.gate.record_refusal(self.role)
    }
}

impl Drop for LoginAttempt<'_> {
    fn drop(&mut self) {
        if self.outcome == Outcome::Pending && self.role == AttemptRole::Probe {
            self.gate.release_probe();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait_time(gate: &LoginBackoff) -> Option<Duration> {
        match gate.admit() {
            Admission::Wait(left) => Some(left),
            Admission::Proceed(_) => None,
        }
    }

    fn refuse(gate: &LoginBackoff) -> Duration {
        match gate.admit() {
            Admission::Proceed(attempt) => attempt.refused(),
            Admission::Wait(left) => panic!("gate closed for {left:?}"),
        }
    }

    fn expect_role(gate: &LoginBackoff, role: AttemptRole) -> LoginAttempt<'_> {
        match gate.admit() {
            Admission::Proceed(attempt) if attempt.role() == role => attempt,
            other => panic!("expected {role:?} attempt, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn open_for_everyone_until_a_refusal() {
        let gate = LoginBackoff::default();
        let attempts: Vec<_> = (0..50)
            .map(|_| expect_role(&gate, AttemptRole::Normal))
            .collect();
        assert_eq!(attempts.len(), 50);
    }

    #[tokio::test(start_paused = true)]
    async fn refusal_gates_logins_until_the_window_closes() {
        let gate = LoginBackoff::default();
        assert_eq!(refuse(&gate), INITIAL_DELAY);
        assert_eq!(wait_time(&gate), Some(INITIAL_DELAY));

        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(wait_time(&gate), Some(Duration::from_secs(3)));

        tokio::time::advance(Duration::from_secs(3)).await;
        drop(expect_role(&gate, AttemptRole::Probe));
    }

    #[tokio::test(start_paused = true)]
    async fn delay_doubles_per_refused_window_and_caps() {
        let gate = LoginBackoff::default();
        let mut seen = Vec::new();
        for _ in 0..8 {
            let delay = refuse(&gate);
            seen.push(delay.as_secs());
            tokio::time::advance(delay).await;
        }
        assert_eq!(seen, [5, 10, 20, 40, 60, 60, 60, 60]);
    }

    #[tokio::test(start_paused = true)]
    async fn refusals_in_flight_together_count_once() {
        let gate = LoginBackoff::default();
        let first = expect_role(&gate, AttemptRole::Normal);
        let second = expect_role(&gate, AttemptRole::Normal);
        let third = expect_role(&gate, AttemptRole::Normal);

        assert_eq!(first.refused(), INITIAL_DELAY);
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(second.refused(), Duration::from_secs(4));
        assert_eq!(third.refused(), Duration::from_secs(4));

        tokio::time::advance(Duration::from_secs(4)).await;
        assert_eq!(
            refuse(&gate),
            Duration::from_secs(10),
            "the next window is the second step, not the fourth"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn only_one_probe_per_window() {
        let gate = LoginBackoff::default();
        tokio::time::advance(refuse(&gate)).await;

        let probe = expect_role(&gate, AttemptRole::Probe);
        for _ in 0..10 {
            assert_eq!(wait_time(&gate), Some(PROBE_RETRY));
        }

        assert_eq!(probe.refused(), Duration::from_secs(10));
        assert_eq!(wait_time(&gate), Some(Duration::from_secs(10)));
    }

    #[tokio::test(start_paused = true)]
    async fn successful_probe_reopens_and_resets_the_schedule() {
        let gate = LoginBackoff::default();
        for _ in 0..3 {
            tokio::time::advance(refuse(&gate)).await;
        }

        expect_role(&gate, AttemptRole::Probe).succeeded();
        let a = expect_role(&gate, AttemptRole::Normal);
        let b = expect_role(&gate, AttemptRole::Normal);
        drop((a, b));
        assert_eq!(refuse(&gate), INITIAL_DELAY);
    }

    #[tokio::test(start_paused = true)]
    async fn abandoned_probe_hands_over_to_the_next_caller() {
        let gate = LoginBackoff::default();
        tokio::time::advance(refuse(&gate)).await;

        drop(expect_role(&gate, AttemptRole::Probe));
        drop(expect_role(&gate, AttemptRole::Probe));
    }

    #[test]
    fn huge_refusal_counts_do_not_overflow() {
        assert_eq!(delay_after(u32::MAX), MAX_DELAY);
        assert_eq!(delay_after(40), MAX_DELAY);
    }
}
