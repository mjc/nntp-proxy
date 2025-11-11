//! Connection pool metric newtypes
//!
//! This module provides type-safe wrappers for connection pool metrics
//! to prevent accidentally mixing different pool statistics.

use std::fmt;

/// Number of available connections in the pool
///
/// Represents connections that are idle and ready to be used.
/// This value should always be ≤ MaxPoolSize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AvailableConnections(usize);

impl AvailableConnections {
    /// Create a new available connections count
    #[inline]
    pub const fn new(count: usize) -> Self {
        Self(count)
    }

    /// Get the raw value
    #[inline]
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }

    /// Create zero available connections
    #[inline]
    pub const fn zero() -> Self {
        Self(0)
    }
}

impl fmt::Display for AvailableConnections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<usize> for AvailableConnections {
    fn from(value: usize) -> Self {
        Self(value)
    }
}

/// Maximum size of the connection pool
///
/// Represents the configured maximum number of connections
/// that can exist in the pool simultaneously.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MaxPoolSize(usize);

impl MaxPoolSize {
    /// Create a new maximum pool size
    #[inline]
    pub const fn new(size: usize) -> Self {
        Self(size)
    }

    /// Get the raw value
    #[inline]
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl fmt::Display for MaxPoolSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<usize> for MaxPoolSize {
    fn from(value: usize) -> Self {
        Self(value)
    }
}

/// Total number of connections created in the pool's lifetime
///
/// This is a monotonically increasing counter that tracks all connections
/// created since the pool was initialized. Useful for monitoring connection
/// churn and pool efficiency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CreatedConnections(usize);

impl CreatedConnections {
    /// Create a new created connections count
    #[inline]
    pub const fn new(count: usize) -> Self {
        Self(count)
    }

    /// Get the raw value
    #[inline]
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }

    /// Create zero created connections (initial state)
    #[inline]
    pub const fn zero() -> Self {
        Self(0)
    }
}

impl fmt::Display for CreatedConnections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<usize> for CreatedConnections {
    fn from(value: usize) -> Self {
        Self(value)
    }
}

/// Number of connections currently in use
///
/// Calculated as: max_size - available
/// Represents connections that are actively being used by clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct InUseConnections(usize);

impl InUseConnections {
    /// Create a new in-use connections count
    #[inline]
    pub const fn new(count: usize) -> Self {
        Self(count)
    }

    /// Get the raw value
    #[inline]
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }

    /// Calculate from pool capacity and availability
    #[inline]
    pub fn from_pool_stats(max: MaxPoolSize, available: AvailableConnections) -> Self {
        Self(max.get().saturating_sub(available.get()))
    }

    /// Create zero in-use connections
    #[inline]
    pub const fn zero() -> Self {
        Self(0)
    }
}

impl fmt::Display for InUseConnections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<usize> for InUseConnections {
    fn from(value: usize) -> Self {
        Self(value)
    }
}

/// Pool utilization as a percentage (0-100)
///
/// Calculated as: (in_use / max_size) * 100
/// Useful for monitoring pool health and load.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PoolUtilization(f64);

impl PoolUtilization {
    /// Create a new pool utilization percentage
    ///
    /// # Panics
    /// Panics if percentage is not in range [0.0, 100.0]
    #[inline]
    pub fn new(percentage: f64) -> Self {
        assert!(
            (0.0..=100.0).contains(&percentage),
            "Utilization must be 0-100%, got {}",
            percentage
        );
        Self(percentage)
    }

    /// Calculate utilization from pool stats
    #[inline]
    pub fn from_pool_stats(max: MaxPoolSize, available: AvailableConnections) -> Self {
        let max_size = max.get();
        if max_size == 0 {
            return Self(0.0);
        }

        let in_use = max_size.saturating_sub(available.get());
        let utilization = (in_use as f64 / max_size as f64) * 100.0;
        Self(utilization)
    }

    /// Get the percentage value
    #[inline]
    #[must_use]
    pub fn as_percentage(self) -> f64 {
        self.0
    }

    /// Check if pool is fully utilized (100%)
    #[inline]
    #[must_use]
    pub fn is_full(self) -> bool {
        self.0 >= 100.0
    }

    /// Check if pool is empty (0% utilization)
    #[inline]
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0.0
    }

    /// Check if pool is under high load (>= 80%)
    #[inline]
    #[must_use]
    pub fn is_high_load(self) -> bool {
        self.0 >= 80.0
    }
}

impl fmt::Display for PoolUtilization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1}%", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_available_connections() {
        let available = AvailableConnections::new(5);
        assert_eq!(available.get(), 5);
        assert_eq!(format!("{}", available), "5");

        let zero = AvailableConnections::zero();
        assert_eq!(zero.get(), 0);
    }

    #[test]
    fn test_max_pool_size() {
        let max = MaxPoolSize::new(10);
        assert_eq!(max.get(), 10);
        assert_eq!(format!("{}", max), "10");
    }

    #[test]
    fn test_created_connections() {
        let created = CreatedConnections::new(25);
        assert_eq!(created.get(), 25);
        assert_eq!(format!("{}", created), "25");

        let zero = CreatedConnections::zero();
        assert_eq!(zero.get(), 0);
    }

    #[test]
    fn test_in_use_connections() {
        let max = MaxPoolSize::new(10);
        let available = AvailableConnections::new(3);
        let in_use = InUseConnections::from_pool_stats(max, available);
        assert_eq!(in_use.get(), 7);
    }

    #[test]
    fn test_in_use_saturating() {
        // Test saturating_sub when available > max (shouldn't happen but handles it gracefully)
        let max = MaxPoolSize::new(5);
        let available = AvailableConnections::new(10);
        let in_use = InUseConnections::from_pool_stats(max, available);
        assert_eq!(in_use.get(), 0);
    }

    #[test]
    fn test_pool_utilization() {
        let max = MaxPoolSize::new(10);
        let available = AvailableConnections::new(3);
        let utilization = PoolUtilization::from_pool_stats(max, available);
        assert_eq!(utilization.as_percentage(), 70.0);
        assert_eq!(format!("{}", utilization), "70.0%");
    }

    #[test]
    fn test_pool_utilization_full() {
        let max = MaxPoolSize::new(10);
        let available = AvailableConnections::new(0);
        let utilization = PoolUtilization::from_pool_stats(max, available);
        assert!(utilization.is_full());
        assert!(utilization.is_high_load());
        assert!(!utilization.is_empty());
    }

    #[test]
    fn test_pool_utilization_empty() {
        let max = MaxPoolSize::new(10);
        let available = AvailableConnections::new(10);
        let utilization = PoolUtilization::from_pool_stats(max, available);
        assert!(utilization.is_empty());
        assert!(!utilization.is_full());
        assert!(!utilization.is_high_load());
    }

    #[test]
    fn test_pool_utilization_high_load() {
        let max = MaxPoolSize::new(10);
        let available = AvailableConnections::new(1);
        let utilization = PoolUtilization::from_pool_stats(max, available);
        assert!(utilization.is_high_load());
        assert!(!utilization.is_full());
    }

    #[test]
    fn test_pool_utilization_zero_max() {
        // Edge case: zero-sized pool
        let max = MaxPoolSize::new(0);
        let available = AvailableConnections::new(0);
        let utilization = PoolUtilization::from_pool_stats(max, available);
        assert_eq!(utilization.as_percentage(), 0.0);
    }

    #[test]
    #[should_panic(expected = "Utilization must be 0-100%")]
    fn test_pool_utilization_invalid() {
        PoolUtilization::new(150.0);
    }

    #[test]
    fn test_ordering() {
        let small = AvailableConnections::new(1);
        let large = AvailableConnections::new(10);
        assert!(small < large);
        assert_eq!(small, small);
    }

    #[test]
    fn test_from_conversions() {
        let available: AvailableConnections = 5usize.into();
        assert_eq!(available.get(), 5);

        let max: MaxPoolSize = 10usize.into();
        assert_eq!(max.get(), 10);

        let created: CreatedConnections = 25usize.into();
        assert_eq!(created.get(), 25);
    }
}
