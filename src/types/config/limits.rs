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
        Self::from_value(value).ok_or_else(|| serde::de::Error::custom("ThreadCount cannot be 0"))
    }
}

impl std::str::FromStr for ThreadCount {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let value = s.parse::<usize>()?;
        Ok(Self::from_value(value).unwrap_or(Self::DEFAULT))
    }
}
