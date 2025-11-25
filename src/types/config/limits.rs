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
    use proptest::prelude::*;

    // ============================================================================
    // Property Tests - MaxConnections
    // ============================================================================

    proptest! {
        /// Property: Any non-zero usize round-trips through MaxConnections
        #[test]
        fn prop_max_connections_valid_range(value in 1usize..=10000) {
            let max = MaxConnections::new(value).unwrap();
            prop_assert_eq!(max.get(), value);
        }

        /// Property: Display shows exact value
        #[test]
        fn prop_max_connections_display(value in 1usize..=10000) {
            let max = MaxConnections::new(value).unwrap();
            prop_assert_eq!(max.to_string(), value.to_string());
        }

        /// Property: JSON serialization round-trips correctly
        #[test]
        fn prop_max_connections_serde_json(value in 1usize..=10000) {
            let max = MaxConnections::new(value).unwrap();
            let json = serde_json::to_string(&max).unwrap();
            let parsed: MaxConnections = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(parsed.get(), value);
        }

        /// Property: Clone creates identical copy
        #[test]
        fn prop_max_connections_clone(value in 1usize..=10000) {
            let max = MaxConnections::new(value).unwrap();
            let cloned = max;
            prop_assert_eq!(max, cloned);
        }
    }

    // Edge Cases - MaxConnections
    #[test]
    fn test_max_connections_zero_rejected() {
        assert!(MaxConnections::new(0).is_none());
    }

    #[test]
    fn test_max_connections_default() {
        assert_eq!(MaxConnections::DEFAULT.get(), 10);
    }

    #[test]
    fn test_max_connections_serde_json_zero_rejected() {
        assert!(serde_json::from_str::<MaxConnections>("0").is_err());
    }

    // ============================================================================
    // Property Tests - MaxErrors
    // ============================================================================

    proptest! {
        /// Property: Any non-zero u32 round-trips through MaxErrors
        #[test]
        fn prop_max_errors_valid_range(value in 1u32..=1000) {
            let max = MaxErrors::new(value).unwrap();
            prop_assert_eq!(max.get(), value);
        }

        /// Property: Display shows exact value
        #[test]
        fn prop_max_errors_display(value in 1u32..=1000) {
            let max = MaxErrors::new(value).unwrap();
            prop_assert_eq!(max.to_string(), value.to_string());
        }

        /// Property: JSON serialization round-trips correctly
        #[test]
        fn prop_max_errors_serde_json(value in 1u32..=1000) {
            let max = MaxErrors::new(value).unwrap();
            let json = serde_json::to_string(&max).unwrap();
            let parsed: MaxErrors = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(parsed.get(), value);
        }

        /// Property: Clone creates identical copy
        #[test]
        fn prop_max_errors_clone(value in 1u32..=1000) {
            let max = MaxErrors::new(value).unwrap();
            let cloned = max;
            prop_assert_eq!(max, cloned);
        }
    }

    // Edge Cases - MaxErrors
    #[test]
    fn test_max_errors_zero_rejected() {
        assert!(MaxErrors::new(0).is_none());
    }

    #[test]
    fn test_max_errors_default() {
        assert_eq!(MaxErrors::DEFAULT.get(), 3);
    }

    #[test]
    fn test_max_errors_serde_json_zero_rejected() {
        assert!(serde_json::from_str::<MaxErrors>("0").is_err());
    }

    // ============================================================================
    // Property Tests - ThreadCount
    // ============================================================================

    proptest! {
        /// Property: Any non-zero usize round-trips through ThreadCount
        #[test]
        fn prop_thread_count_valid_range(value in 1usize..=128) {
            let threads = ThreadCount::new(value).unwrap();
            prop_assert_eq!(threads.get(), value);
        }

        /// Property: from_value handles non-zero values correctly
        #[test]
        fn prop_thread_count_from_value(value in 1usize..=128) {
            let threads = ThreadCount::from_value(value).unwrap();
            prop_assert_eq!(threads.get(), value);
        }

        /// Property: Display shows exact value
        #[test]
        fn prop_thread_count_display(value in 1usize..=128) {
            let threads = ThreadCount::new(value).unwrap();
            prop_assert_eq!(threads.to_string(), value.to_string());
        }

        /// Property: Into<usize> conversion works
        #[test]
        fn prop_thread_count_into_usize(value in 1usize..=128) {
            let threads = ThreadCount::new(value).unwrap();
            let converted: usize = threads.into();
            prop_assert_eq!(converted, value);
        }

        /// Property: FromStr parses valid numbers
        #[test]
        fn prop_thread_count_from_str(value in 1usize..=128) {
            let s = value.to_string();
            let threads: ThreadCount = s.parse().unwrap();
            prop_assert_eq!(threads.get(), value);
        }

        /// Property: JSON serialization round-trips correctly
        #[test]
        fn prop_thread_count_serde_json(value in 1usize..=128) {
            let threads = ThreadCount::new(value).unwrap();
            let json = serde_json::to_string(&threads).unwrap();
            let parsed: ThreadCount = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(parsed.get(), value);
        }

        /// Property: Clone creates identical copy
        #[test]
        fn prop_thread_count_clone(value in 1usize..=128) {
            let threads = ThreadCount::new(value).unwrap();
            let cloned = threads;
            prop_assert_eq!(threads, cloned);
        }
    }

    // Edge Cases - ThreadCount
    #[test]
    fn test_thread_count_default() {
        assert_eq!(ThreadCount::default().get(), 1);
    }

    #[test]
    fn test_thread_count_new_zero_rejected() {
        // new() can't call num_cpus() in const context, so returns None for 0
        assert!(ThreadCount::new(0).is_none());
    }

    #[test]
    fn test_thread_count_from_value_zero_auto_detects() {
        // from_value(0) should auto-detect CPU count
        let threads = ThreadCount::from_value(0).unwrap();
        assert!(threads.get() >= 1);
    }

    #[test]
    fn test_thread_count_from_str_zero_auto_detects() {
        // Parsing "0" should auto-detect CPU count
        let threads: ThreadCount = "0".parse().unwrap();
        assert!(threads.get() >= 1);
    }

    #[test]
    fn test_thread_count_from_str_invalid() {
        assert!("not_a_number".parse::<ThreadCount>().is_err());
    }

    #[test]
    fn test_thread_count_serde_json_zero_auto_detects() {
        // Deserializing 0 should auto-detect CPU count
        let parsed: ThreadCount = serde_json::from_str("0").unwrap();
        assert!(parsed.get() >= 1);
    }
}
