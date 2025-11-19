//! Connection and error limit configuration types

use std::num::{NonZeroU32, NonZeroUsize};

nonzero_newtype! {
    /// A non-zero maximum connections limit
    ///
    /// Ensures connection pools always have at least 1 connection allowed.
    ///
    /// # Examples
    /// ```
    /// use nntp_proxy::types::MaxConnections;
    ///
    /// let max = MaxConnections::new(10).unwrap();
    /// assert_eq!(max.get(), 10);
    ///
    /// // Zero connections is invalid
    /// assert!(MaxConnections::new(0).is_none());
    /// ```
    #[doc(alias = "pool_size")]
    #[doc(alias = "connection_limit")]
    pub struct MaxConnections(NonZeroUsize: usize, serialize as serialize_u64);
}

impl MaxConnections {
    /// Default maximum connections per backend
    pub const DEFAULT: Self = Self(NonZeroUsize::new(10).unwrap());
}

nonzero_newtype! {
    /// A non-zero maximum errors threshold.
    ///
    /// Used to specify the maximum number of errors allowed before taking action.
    ///
    /// This type is used in two primary contexts:
    /// - **Health check error thresholds:** Ensures that health checks require at least one error before marking a service as unhealthy.
    /// - **Retry logic:** Specifies the maximum number of retry attempts after errors.
    ///
    /// By enforcing a non-zero value, this type ensures that both health check and retry thresholds are always meaningful (at least 1 error required).
    ///
    /// # Examples
    /// ```
    /// use nntp_proxy::types::MaxErrors;
    ///
    /// let max = MaxErrors::new(3).unwrap();
    /// assert_eq!(max.get(), 3);
    ///
    /// // Zero errors is invalid
    /// assert!(MaxErrors::new(0).is_none());
    /// ```
    pub struct MaxErrors(NonZeroU32: u32, serialize as serialize_u32);
}

impl MaxErrors {
    /// Default maximum errors threshold
    pub const DEFAULT: Self = Self(NonZeroU32::new(3).unwrap());
}

/// A non-zero thread count
///
/// Ensures thread pools always have at least 1 thread.
/// Special handling: passing 0 to `new()` returns the number of CPU cores.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadCount(NonZeroUsize);

impl ThreadCount {
    /// Default thread count (single-threaded)
    pub const DEFAULT: Self = Self(NonZeroUsize::new(1).unwrap());

    /// Create a new ThreadCount
    ///
    /// - If value is 0, returns the number of CPU cores
    /// - Otherwise returns the specified value
    /// - Always returns Some (never None)
    #[must_use]
    pub const fn new(value: usize) -> Option<Self> {
        if value == 0 {
            // 0 means auto-detect CPU count (handled at runtime)
            // We can't call num_cpus() in const context, so return None
            // and the caller should use from_value() instead
            None
        } else {
            match NonZeroUsize::new(value) {
                Some(nz) => Some(Self(nz)),
                None => None,
            }
        }
    }

    /// Create a new ThreadCount from a value
    ///
    /// - If value is 0, returns the number of CPU cores
    /// - Otherwise returns the specified value
    /// - Always returns Some (never None)
    #[must_use]
    pub fn from_value(value: usize) -> Option<Self> {
        if value == 0 {
            Some(Self::num_cpus())
        } else {
            NonZeroUsize::new(value).map(Self)
        }
    }

    /// Get the inner value
    #[must_use]
    #[inline]
    pub const fn get(&self) -> usize {
        self.0.get()
    }

    /// Get the number of available CPU cores (private helper)
    fn num_cpus() -> Self {
        let count = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(1);
        // SAFETY: available_parallelism always returns at least 1
        Self(NonZeroUsize::new(count).unwrap())
    }
}

impl Default for ThreadCount {
    /// Default to single-threaded (1 thread)
    fn default() -> Self {
        Self(NonZeroUsize::new(1).unwrap())
    }
}

impl std::fmt::Display for ThreadCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl From<ThreadCount> for usize {
    fn from(val: ThreadCount) -> Self {
        val.get()
    }
}

impl serde::Serialize for ThreadCount {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.get() as _)
    }
}

impl<'de> serde::Deserialize<'de> for ThreadCount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = usize::deserialize(deserializer)?;
        Self::from_value(value).ok_or_else(|| {
            serde::de::Error::custom("ThreadCount must be a positive integer (0 means auto-detect)")
        })
    }
}

impl std::str::FromStr for ThreadCount {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let value = s.parse::<usize>()?;
        Ok(Self::from_value(value).unwrap_or_else(Self::default))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // MaxConnections tests
    #[test]
    fn test_max_connections_new_valid() {
        let max = MaxConnections::new(10).unwrap();
        assert_eq!(max.get(), 10);
    }

    #[test]
    fn test_max_connections_new_zero_returns_none() {
        assert!(MaxConnections::new(0).is_none());
    }

    #[test]
    fn test_max_connections_default() {
        let max = MaxConnections::DEFAULT;
        assert_eq!(max.get(), 10);
    }

    #[test]
    fn test_max_connections_new_one() {
        let max = MaxConnections::new(1).unwrap();
        assert_eq!(max.get(), 1);
    }

    #[test]
    fn test_max_connections_new_large() {
        let max = MaxConnections::new(1000).unwrap();
        assert_eq!(max.get(), 1000);
    }

    #[test]
    fn test_max_connections_display() {
        let max = MaxConnections::new(15).unwrap();
        assert_eq!(max.to_string(), "15");
    }

    #[test]
    fn test_max_connections_serde_json() {
        let max = MaxConnections::new(20).unwrap();
        let json = serde_json::to_string(&max).unwrap();
        assert_eq!(json, "20");

        let parsed: MaxConnections = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.get(), 20);
    }

    #[test]
    fn test_max_connections_serde_json_zero_rejected() {
        let result = serde_json::from_str::<MaxConnections>("0");
        assert!(result.is_err());
    }

    #[test]
    fn test_max_connections_equality() {
        let max1 = MaxConnections::new(10).unwrap();
        let max2 = MaxConnections::new(10).unwrap();
        let max3 = MaxConnections::new(20).unwrap();

        assert_eq!(max1, max2);
        assert_ne!(max1, max3);
    }

    #[test]
    fn test_max_connections_clone() {
        let max = MaxConnections::new(15).unwrap();
        let cloned = max.clone();
        assert_eq!(max, cloned);
    }

    // MaxErrors tests
    #[test]
    fn test_max_errors_new_valid() {
        let max = MaxErrors::new(3).unwrap();
        assert_eq!(max.get(), 3);
    }

    #[test]
    fn test_max_errors_new_zero_returns_none() {
        assert!(MaxErrors::new(0).is_none());
    }

    #[test]
    fn test_max_errors_default() {
        let max = MaxErrors::DEFAULT;
        assert_eq!(max.get(), 3);
    }

    #[test]
    fn test_max_errors_new_one() {
        let max = MaxErrors::new(1).unwrap();
        assert_eq!(max.get(), 1);
    }

    #[test]
    fn test_max_errors_new_large() {
        let max = MaxErrors::new(100).unwrap();
        assert_eq!(max.get(), 100);
    }

    #[test]
    fn test_max_errors_display() {
        let max = MaxErrors::new(7).unwrap();
        assert_eq!(max.to_string(), "7");
    }

    #[test]
    fn test_max_errors_serde_json() {
        let max = MaxErrors::new(5).unwrap();
        let json = serde_json::to_string(&max).unwrap();
        assert_eq!(json, "5");

        let parsed: MaxErrors = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.get(), 5);
    }

    #[test]
    fn test_max_errors_serde_json_zero_rejected() {
        let result = serde_json::from_str::<MaxErrors>("0");
        assert!(result.is_err());
    }

    #[test]
    fn test_max_errors_equality() {
        let max1 = MaxErrors::new(3).unwrap();
        let max2 = MaxErrors::new(3).unwrap();
        let max3 = MaxErrors::new(5).unwrap();

        assert_eq!(max1, max2);
        assert_ne!(max1, max3);
    }

    #[test]
    fn test_max_errors_clone() {
        let max = MaxErrors::new(10).unwrap();
        let cloned = max.clone();
        assert_eq!(max, cloned);
    }

    // ThreadCount tests
    #[test]
    fn test_thread_count_default() {
        let threads = ThreadCount::default();
        assert_eq!(threads.get(), 1);
    }

    #[test]
    fn test_thread_count_from_value_auto() {
        let threads = ThreadCount::from_value(0).unwrap();
        // Should return CPU count (at least 1)
        assert!(threads.get() >= 1);
    }

    #[test]
    fn test_thread_count_from_value_explicit() {
        let threads = ThreadCount::from_value(4).unwrap();
        assert_eq!(threads.get(), 4);
    }

    #[test]
    fn test_thread_count_new_returns_none_for_zero() {
        // new() can't call num_cpus() in const context, so returns None for 0
        assert!(ThreadCount::new(0).is_none());
    }

    #[test]
    fn test_thread_count_new_valid() {
        let threads = ThreadCount::new(8).unwrap();
        assert_eq!(threads.get(), 8);
    }

    #[test]
    fn test_thread_count_display() {
        let threads = ThreadCount::from_value(2).unwrap();
        assert_eq!(threads.to_string(), "2");
    }

    #[test]
    fn test_thread_count_from() {
        let threads = ThreadCount::from_value(6).unwrap();
        let value: usize = threads.into();
        assert_eq!(value, 6);
    }

    #[test]
    fn test_thread_count_from_str_valid() {
        let threads: ThreadCount = "4".parse().unwrap();
        assert_eq!(threads.get(), 4);
    }

    #[test]
    fn test_thread_count_from_str_zero() {
        let threads: ThreadCount = "0".parse().unwrap();
        // Should auto-detect CPU count
        assert!(threads.get() >= 1);
    }

    #[test]
    fn test_thread_count_from_str_invalid() {
        let result = "not_a_number".parse::<ThreadCount>();
        assert!(result.is_err());
    }

    #[test]
    fn test_thread_count_serde_json() {
        let threads = ThreadCount::from_value(3).unwrap();
        let json = serde_json::to_string(&threads).unwrap();
        assert_eq!(json, "3");

        let parsed: ThreadCount = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.get(), 3);
    }

    #[test]
    fn test_thread_count_serde_json_auto() {
        let json = "0";
        let parsed: ThreadCount = serde_json::from_str(json).unwrap();
        // Should auto-detect CPU count
        assert!(parsed.get() >= 1);
    }

    #[test]
    fn test_thread_count_equality() {
        let t1 = ThreadCount::from_value(4).unwrap();
        let t2 = ThreadCount::from_value(4).unwrap();
        let t3 = ThreadCount::from_value(8).unwrap();

        assert_eq!(t1, t2);
        assert_ne!(t1, t3);
    }

    #[test]
    fn test_thread_count_clone() {
        let threads = ThreadCount::from_value(2).unwrap();
        let cloned = threads.clone();
        assert_eq!(threads, cloned);
    }
}
