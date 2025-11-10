//! Cache-related configuration types

use serde::{Deserialize, Serialize};
use std::fmt;
use std::num::NonZeroUsize;
use std::str::FromStr;

use crate::types::ValidationError;

/// A non-zero cache capacity
///
/// Ensures caches always have at least 1 slot available
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheCapacity(NonZeroUsize);

impl CacheCapacity {
    /// Create a new CacheCapacity, returning None if value is 0
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

    /// Default pool size
    pub const DEFAULT: Self = Self(NonZeroUsize::new(1000).unwrap());
}

impl fmt::Display for CacheCapacity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl FromStr for CacheCapacity {
    type Err = ValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let value = s.parse::<usize>().map_err(|_| {
            ValidationError::InvalidHostName(format!("invalid cache capacity: {}", s))
        })?;
        Self::new(value).ok_or_else(|| {
            ValidationError::InvalidHostName("cache capacity cannot be 0".to_string())
        })
    }
}

impl From<CacheCapacity> for usize {
    fn from(cap: CacheCapacity) -> Self {
        cap.get()
    }
}

impl Serialize for CacheCapacity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.get() as u64)
    }
}

impl<'de> Deserialize<'de> for CacheCapacity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = usize::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| serde::de::Error::custom("cache capacity cannot be 0"))
    }
}
