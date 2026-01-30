//! Backend connection information and load tracking types
//!
//! Contains `BackendInfo` (the per-backend metadata) and its supporting
//! atomic counter types for load balancing.

use derive_more::{AsRef, Deref, Display, From};
use std::cmp::Ordering as CmpOrdering;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::pool::DeadpoolConnectionProvider;
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

/// Atomic counter for pending requests on a backend
#[derive(Debug, Clone, Display, From, AsRef, Deref)]
#[display("PendingCount({})", "_0.load(Ordering::Relaxed)")]
pub struct PendingCount(Arc<AtomicUsize>);

// Manual PartialEq because Arc<AtomicUsize> doesn't auto-derive
impl PartialEq for PendingCount {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl PartialEq<usize> for PendingCount {
    fn eq(&self, other: &usize) -> bool {
        self.get() == *other
    }
}

impl Eq for PendingCount {}

impl PendingCount {
    /// Create a new pending count initialized to zero
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

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

    /// Get the current pending count
    #[inline]
    #[must_use]
    pub fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

impl Default for PendingCount {
    fn default() -> Self {
        Self::new()
    }
}

/// Atomic counter for stateful connections on a backend
#[derive(Debug, Clone, Display, From, AsRef, Deref)]
#[display("StatefulCount({})", "_0.load(Ordering::Relaxed)")]
pub struct StatefulCount(Arc<AtomicUsize>);

// Manual PartialEq because Arc<AtomicUsize> doesn't auto-derive
impl PartialEq for StatefulCount {
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl PartialEq<usize> for StatefulCount {
    fn eq(&self, other: &usize) -> bool {
        self.get() == *other
    }
}

impl Eq for StatefulCount {}

impl StatefulCount {
    /// Create a new stateful count initialized to zero
    #[inline]
    #[must_use]
    pub fn new() -> Self {
        Self(Arc::new(AtomicUsize::new(0)))
    }

    /// Get the current stateful count
    #[inline]
    #[must_use]
    pub fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    /// Try to acquire a stateful slot (compare-exchange loop)
    ///
    /// Returns true if successfully incremented below max_stateful limit
    pub fn try_acquire(&self, max_stateful: usize) -> bool {
        let mut current = self.0.load(Ordering::Acquire);
        loop {
            if current >= max_stateful {
                return false;
            }

            match self.0.compare_exchange_weak(
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

    /// Release a stateful slot (decrement if > 0)
    ///
    /// Returns Ok(previous_value) if successfully decremented, Err(0) if already zero
    pub fn release(&self) -> Result<usize, usize> {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                if current == 0 {
                    None
                } else {
                    Some(current - 1)
                }
            })
    }
}

impl Default for StatefulCount {
    fn default() -> Self {
        Self::new()
    }
}

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
