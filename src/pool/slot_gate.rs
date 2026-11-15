//! Hard limit on one backend's open connections.
//!
//! Deadpool 0.13.1 `Pool::resize` does not take a slot away when it shrinks a
//! pool that has not opened every slot, and gives slots back for connections
//! still checked out when it grows. A shrunk pool can therefore still open its
//! old maximum, or more. Shrinking is how the adaptive connection cap and the
//! replacement cooldown keep a backend under its provider limit, so the pool
//! size alone cannot be trusted as that limit.
//!
//! The gate counts every connection from the moment its setup starts until
//! its socket is destroyed, whether idle, checked out or retiring, and refuses
//! to start a setup at the target. In steady state it never refuses: deadpool
//! only creates when it has a slot, and with no slot left over from a shrink
//! that means the gate has room too. It only refuses the creations a stale
//! deadpool slot would have let through, and those checkouts wait here for a
//! returned or closed connection instead.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Longest a refused checkout waits between re-checks. A wake can be missed
/// in a narrow race between a return reading the waiter count and a waiter
/// registering; this bounds what such a miss costs.
const RECHECK: Duration = Duration::from_millis(50);

/// Open-connection count and limit for one backend, shared by its manager,
/// its provider and every connection it has opened.
#[derive(Debug)]
pub(crate) struct SlotGate {
    target: AtomicUsize,
    live: AtomicUsize,
    waiters: AtomicUsize,
    notify: tokio::sync::Notify,
}

impl Default for SlotGate {
    /// Unlimited until the pool that owns the manager sets its size.
    fn default() -> Self {
        Self {
            target: AtomicUsize::new(usize::MAX),
            live: AtomicUsize::new(0),
            waiters: AtomicUsize::new(0),
            notify: tokio::sync::Notify::new(),
        }
    }
}

impl SlotGate {
    /// Set how many connections may be open. Raising it wakes waiting
    /// checkouts; lowering it closes nothing, it only stops new setups.
    pub(crate) fn set_target(&self, target: usize) {
        let previous = self.target.swap(target, Ordering::SeqCst);
        if target > previous {
            self.wake();
        }
    }

    /// Claim a slot for a connection about to be set up, or `None` at the
    /// target. The claim lasts until the returned reservation drops.
    pub(crate) fn try_reserve(self: &Arc<Self>) -> Option<SlotReservation> {
        let target = self.target.load(Ordering::SeqCst);
        self.live
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |live| {
                (live < target).then_some(live + 1)
            })
            .ok()
            .map(|_| SlotReservation(Arc::clone(self)))
    }

    /// Checkouts currently waiting for a slot, which deadpool's own waiter
    /// count cannot see.
    pub(crate) fn waiters(&self) -> usize {
        self.waiters.load(Ordering::SeqCst)
    }

    /// Open connections, including ones still being set up.
    #[cfg(test)]
    pub(crate) fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    /// Wake waiting checkouts if there are any. One atomic load otherwise,
    /// because it runs on every connection return.
    pub(crate) fn wake(&self) {
        if self.waiters.load(Ordering::SeqCst) > 0 {
            self.notify.notify_waiters();
        }
    }

    /// Wait until a connection is returned or closed or the target rises,
    /// at most `limit` (and at most [`RECHECK`]), so the caller can retry.
    pub(crate) async fn wait(&self, limit: Duration) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let _waiting = Waiting::new(&self.waiters);
        let _ = tokio::time::timeout(limit.min(RECHECK), notified).await;
    }
}

/// Counts one waiting checkout until dropped, so a cancelled checkout cannot
/// leave the waiter count, and the backlog estimate built on it, raised.
struct Waiting<'a>(&'a AtomicUsize);

impl<'a> Waiting<'a> {
    fn new(waiters: &'a AtomicUsize) -> Self {
        waiters.fetch_add(1, Ordering::SeqCst);
        Self(waiters)
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// One open (or opening) connection's claim on its backend's gate.
#[derive(Debug)]
pub(crate) struct SlotReservation(Arc<SlotGate>);

impl SlotReservation {
    /// The gate, but only when a checkout is waiting on it, so a return can
    /// wake it after the connection is back in the pool without paying for a
    /// reference count on every return.
    pub(crate) fn gate_if_waiting(&self) -> Option<Arc<SlotGate>> {
        (self.0.waiters() > 0).then(|| Arc::clone(&self.0))
    }
}

impl Drop for SlotReservation {
    fn drop(&mut self) {
        self.0.live.fetch_sub(1, Ordering::SeqCst);
        self.0.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_stop_at_the_target_and_free_on_drop() {
        let gate = Arc::new(SlotGate::default());
        gate.set_target(2);
        let a = gate.try_reserve().unwrap();
        let _b = gate.try_reserve().unwrap();
        assert!(gate.try_reserve().is_none());
        drop(a);
        assert!(gate.try_reserve().is_some());
    }

    #[test]
    fn lowering_the_target_keeps_open_connections_but_blocks_new_ones() {
        let gate = Arc::new(SlotGate::default());
        gate.set_target(3);
        let mut held: Vec<_> = (0..3).map(|_| gate.try_reserve().unwrap()).collect();
        gate.set_target(1);
        assert_eq!(gate.live(), 3);
        held.truncate(1);
        assert_eq!(gate.live(), 1);
        assert!(
            gate.try_reserve().is_none(),
            "one still open fills a target of one"
        );
        drop(held);
        assert!(gate.try_reserve().is_some());
    }

    #[tokio::test]
    async fn a_cancelled_wait_stops_counting_as_a_waiter() {
        let gate = Arc::new(SlotGate::default());
        let waiter = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.wait(Duration::from_secs(10)).await })
        };
        while gate.waiters() == 0 {
            tokio::task::yield_now().await;
        }
        waiter.abort();
        let _ = waiter.await;
        assert_eq!(gate.waiters(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_raised_target_wakes_a_waiter_without_waiting_for_the_recheck() {
        let gate = Arc::new(SlotGate::default());
        gate.set_target(0);
        let waiter = {
            let gate = Arc::clone(&gate);
            tokio::spawn(async move { gate.wait(Duration::from_secs(10)).await })
        };
        while gate.waiters() == 0 {
            tokio::task::yield_now().await;
        }
        gate.set_target(1);
        for _ in 0..100 {
            if waiter.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert!(waiter.is_finished(), "woken with the clock stopped");
    }
}
