//! Connection and error limit configuration types

use serde::{Deserialize, Serialize};
use std::fmt;
use std::num::{NonZeroU32, NonZeroUsize};

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
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MaxConnections(NonZeroUsize);

impl MaxConnections {
    /// Create a new MaxConnections, returning None if value is 0
    #[must_use]
    pub const fn new(value: usize) -> Option<Self> {
        match NonZeroUsize::new(value) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    /// Get the value as usize
    #[must_use]
    #[inline]
    pub const fn get(&self) -> usize {
        self.0.get()
    }

    /// Default maximum connections per backend
    pub const DEFAULT: Self = Self(NonZeroUsize::new(10).unwrap());
}

impl fmt::Display for MaxConnections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl From<MaxConnections> for usize {
    fn from(max: MaxConnections) -> Self {
        max.get()
    }
}

impl Serialize for MaxConnections {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.get() as u64)
    }
}

impl<'de> Deserialize<'de> for MaxConnections {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = usize::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| serde::de::Error::custom("max_connections cannot be 0"))
    }
}

/// A non-zero maximum errors threshold
///
/// Ensures health check thresholds are meaningful (at least 1 error required).
///
/// Used for health checks and retry logic.
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
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MaxErrors(NonZeroU32);

impl MaxErrors {
    /// Create a new MaxErrors, returning None if value is 0
    #[must_use]
    pub const fn new(value: u32) -> Option<Self> {
        match NonZeroU32::new(value) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    /// Get the value as u32
    #[must_use]
    #[inline]
    pub const fn get(&self) -> u32 {
        self.0.get()
    }

    /// Default retry attempts
    pub const DEFAULT: Self = Self(NonZeroU32::new(3).unwrap());
}

impl fmt::Display for MaxErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl From<MaxErrors> for u32 {
    fn from(max: MaxErrors) -> Self {
        max.get()
    }
}

impl Serialize for MaxErrors {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u32(self.get())
    }
}

impl<'de> Deserialize<'de> for MaxErrors {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u32::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| serde::de::Error::custom("max_errors cannot be 0"))
    }
}
