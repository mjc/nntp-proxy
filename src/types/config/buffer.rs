//! Buffer and window size configuration types

use serde::{Deserialize, Serialize};
use std::fmt;
use std::num::{NonZeroU64, NonZeroUsize};

/// A non-zero window size for health check tracking
///
/// Ensures health check windows track at least 1 request
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WindowSize(NonZeroU64);

impl WindowSize {
    /// Create a new WindowSize, returning None if value is 0
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    /// Get the value as u64
    #[must_use]
    pub const fn get(&self) -> u64 {
        self.0.get()
    }

    /// Default window size (typically 100)
    pub const DEFAULT: Self = Self(NonZeroU64::new(100).unwrap());
}

impl fmt::Display for WindowSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl From<WindowSize> for u64 {
    fn from(size: WindowSize) -> Self {
        size.get()
    }
}

impl Serialize for WindowSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.get())
    }
}

impl<'de> Deserialize<'de> for WindowSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = u64::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| serde::de::Error::custom("window_size cannot be 0"))
    }
}

/// A non-zero buffer size
///
/// Ensures buffers always have at least 1 byte
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BufferSize(NonZeroUsize);

impl BufferSize {
    /// Create a new BufferSize, returning None if value is 0
    #[must_use]
    pub const fn new(value: usize) -> Option<Self> {
        match NonZeroUsize::new(value) {
            Some(nz) => Some(Self(nz)),
            None => None,
        }
    }

    /// Get the value as usize
    #[must_use]
    pub const fn get(&self) -> usize {
        self.0.get()
    }

    /// Standard NNTP command buffer (512 bytes)
    pub const COMMAND: Self = Self(NonZeroUsize::new(512).unwrap());

    /// Default buffer size (8KB)
    pub const DEFAULT: Self = Self(NonZeroUsize::new(8192).unwrap());

    /// Medium buffer size (256KB)
    pub const MEDIUM: Self = Self(NonZeroUsize::new(256 * 1024).unwrap());

    /// Large buffer size (4MB)
    pub const LARGE: Self = Self(NonZeroUsize::new(4 * 1024 * 1024).unwrap());
}

impl fmt::Display for BufferSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.get())
    }
}

impl From<BufferSize> for usize {
    fn from(size: BufferSize) -> Self {
        size.get()
    }
}

impl Serialize for BufferSize {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u64(self.get() as u64)
    }
}

impl<'de> Deserialize<'de> for BufferSize {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = usize::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| serde::de::Error::custom("buffer size cannot be 0"))
    }
}
