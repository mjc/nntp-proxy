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

nonzero_newtype! {
    /// A non-zero thread count
    ///
    /// Ensures thread pools always have at least 1 thread.
    pub struct ThreadCount(NonZeroUsize: usize, serialize as serialize_u64);
}

impl ThreadCount {
    /// Default thread count (typically num_cpus)
    pub const DEFAULT: Self = Self(NonZeroUsize::new(4).unwrap());
}

impl std::str::FromStr for ThreadCount {
    type Err = std::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let value = s.parse::<usize>()?;
        Ok(Self::new(value).unwrap_or(Self::DEFAULT))
    }
}
