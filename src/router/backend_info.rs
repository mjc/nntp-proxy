//! Backend connection information and load tracking types
//!
//! Contains `BackendInfo` (the per-backend metadata) and its supporting
//! atomic counter types for load balancing.

use derive_more::{AsRef, Deref, Display, From};
use std::cmp::Ordering as CmpOrdering;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::pool::DeadpoolConnectionProvider;
use crate::router::backend_queue::BackendQueue;
use crate::types::{BackendId, ServerName};

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

#[inline]
fn new_counter() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

#[inline]
fn counter_value(counter: &Arc<AtomicUsize>) -> usize {
    counter.load(Ordering::Relaxed)
}

fn try_acquire_below(counter: &AtomicUsize, max: usize) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        if current >= max {
            return false;
        }

        match counter.compare_exchange_weak(
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

fn release_if_nonzero(counter: &AtomicUsize) -> Result<usize, usize> {
    counter.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        if current == 0 {
            None
        } else {
            Some(current - 1)
        }
    })
}

macro_rules! atomic_count_type {
    (
        $(#[$meta:meta])*
        $name:ident,
        $display_name:literal,
        impl { $($methods:item)* }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Display, From, AsRef, Deref)]
        #[display("{}({})", $display_name, _0.load(Ordering::Relaxed))]
        pub struct $name(Arc<AtomicUsize>);

        impl PartialEq for $name {
            fn eq(&self, other: &Self) -> bool {
                self.get() == other.get()
            }
        }

        impl PartialEq<usize> for $name {
            fn eq(&self, other: &usize) -> bool {
                self.get() == *other
            }
        }

        impl Eq for $name {}

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl $name {
            #[inline]
            #[must_use]
            pub fn new() -> Self {
                Self(new_counter())
            }

            #[inline]
            #[must_use]
            pub fn get(&self) -> usize {
                counter_value(&self.0)
            }

            $($methods)*
        }
    };
}

atomic_count_type!(
    /// Atomic counter for pending requests on a backend
    PendingCount,
    "PendingCount",
    impl {
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
    }
);

atomic_count_type!(
    /// Atomic counter for stateful connections on a backend
    StatefulCount,
    "StatefulCount",
    impl {
        /// Try to acquire a stateful slot (compare-exchange loop)
        ///
        /// Returns true if successfully incremented below max_stateful limit
        pub fn try_acquire(&self, max_stateful: usize) -> bool {
            try_acquire_below(&self.0, max_stateful)
        }

        /// Release a stateful slot (decrement if > 0)
        ///
        /// Returns Ok(previous_value) if successfully decremented, Err(0) if already zero
        pub fn release(&self) -> Result<usize, usize> {
            release_if_nonzero(&self.0)
        }
    }
);

/// Backend connection information
#[derive(Debug, Clone)]
pub(super) struct BackendInfo {
    /// Backend identifier
    pub(super) id: BackendId,
    /// Server name for logging
    pub(super) name: ServerName,
    /// Connection provider for this backend
    pub(super) provider: DeadpoolConnectionProvider,
    /// Number of pending requests on this backend (for load balancing)
    pub(super) pending_count: PendingCount,
    /// Number of connections in stateful mode (for hybrid routing reservation)
    pub(super) stateful_count: StatefulCount,
    /// Server tier for prioritization (lower = higher priority)
    pub(super) tier: u8,
    /// Pipeline queue for request multiplexing (None if pipelining disabled)
    pub(super) pipeline_queue: Option<Arc<BackendQueue>>,
}

impl BackendInfo {
    /// Calculate load ratio (pending requests / max connections)
    ///
    /// Lower ratios indicate less loaded backends.
    #[must_use]
    pub(super) fn load_ratio(&self) -> LoadRatio {
        let max_conns = self.provider.max_size() as f64;
        if max_conns > 0.0 {
            let pending = self.pending_count.get() as f64;
            LoadRatio::new(pending / max_conns)
        } else {
            LoadRatio::MAX
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn make_backend_info(max_size: usize) -> BackendInfo {
        BackendInfo {
            id: BackendId::from_index(0),
            name: ServerName::try_new(format!("backend-{max_size}")).unwrap(),
            provider: DeadpoolConnectionProvider::new(
                "localhost".to_string(),
                119,
                format!("provider-{max_size}"),
                max_size,
                None,
                None,
            ),
            pending_count: PendingCount::new(),
            stateful_count: StatefulCount::new(),
            tier: 0,
            pipeline_queue: None,
        }
    }

    #[test]
    fn load_ratio_orders_as_expected() {
        let low = LoadRatio::new(0.25);
        let high = LoadRatio::new(0.75);

        assert_eq!(LoadRatio::MIN.get(), 0.0);
        assert_eq!(LoadRatio::MAX.get(), f64::MAX);
        assert!(low < high);
        assert!(high < LoadRatio::MAX);
    }

    #[test]
    fn pending_count_shares_state_across_clones() {
        let pending = PendingCount::new();
        let clone = pending.clone();

        pending.increment();
        pending.increment();
        assert_eq!(clone.get(), 2);
        assert_eq!(pending, 2);

        clone.decrement();
        assert_eq!(pending.get(), 1);
        assert_eq!(format!("{pending}"), "PendingCount(1)");
        assert_eq!(pending, clone);
    }

    #[test]
    fn pending_count_default_starts_at_zero() {
        let pending = PendingCount::default();
        assert_eq!(pending.get(), 0);
        assert_eq!(pending, 0);
    }

    #[test]
    fn stateful_count_try_acquire_enforces_limit() {
        let count = StatefulCount::new();

        assert!(count.try_acquire(2));
        assert!(count.try_acquire(2));
        assert!(!count.try_acquire(2));
        assert_eq!(count.get(), 2);
        assert_eq!(format!("{count}"), "StatefulCount(2)");
    }

    #[test]
    fn stateful_count_release_reports_previous_value() {
        let count = StatefulCount::new();
        assert_eq!(count.release(), Err(0));

        assert!(count.try_acquire(3));
        assert!(count.try_acquire(3));
        assert_eq!(count.release(), Ok(2));
        assert_eq!(count.get(), 1);
        assert_eq!(count.release(), Ok(1));
        assert_eq!(count.release(), Err(0));
    }

    #[test]
    fn stateful_count_concurrent_acquire_caps_successes() {
        let count = Arc::new(StatefulCount::new());
        let barrier = Arc::new(Barrier::new(8));
        let mut threads = Vec::new();

        for _ in 0..8 {
            let count = count.clone();
            let barrier = barrier.clone();
            threads.push(thread::spawn(move || {
                barrier.wait();
                count.try_acquire(3)
            }));
        }

        let successes = threads
            .into_iter()
            .map(|handle| handle.join().unwrap() as usize)
            .sum::<usize>();

        assert_eq!(successes, 3);
        assert_eq!(count.get(), 3);
    }

    #[test]
    fn backend_info_load_ratio_uses_pending_count_and_capacity() {
        let backend = make_backend_info(4);
        backend.pending_count.increment();
        backend.pending_count.increment();

        assert_eq!(backend.load_ratio(), LoadRatio::new(0.5));
    }

    #[test]
    fn backend_info_zero_capacity_reports_max_load_ratio() {
        let backend = make_backend_info(0);
        backend.pending_count.increment();

        assert_eq!(backend.load_ratio(), LoadRatio::MAX);
    }
}
