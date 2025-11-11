//! Type-safe metrics and measurement types

use std::fmt;
use std::ops::{Add, AddAssign};

/// Type-safe byte transfer counter
///
/// Provides compile-time safety for byte counting operations,
/// preventing accidental mixing of different metric types.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct BytesTransferred(u64);

impl BytesTransferred {
    /// Create a new BytesTransferred counter starting at zero
    #[must_use]
    pub const fn zero() -> Self {
        Self(0)
    }

    /// Create a BytesTransferred from a u64 value
    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    /// Get the raw byte count
    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    /// Add bytes to the counter (for usize values)
    #[inline]
    pub fn add(&mut self, bytes: usize) {
        self.0 += bytes as u64;
    }

    /// Add bytes to the counter (for u64 values)
    #[inline]
    pub fn add_u64(&mut self, bytes: u64) {
        self.0 += bytes;
    }
}

impl From<u64> for BytesTransferred {
    #[inline]
    fn from(bytes: u64) -> Self {
        Self(bytes)
    }
}

impl From<BytesTransferred> for u64 {
    #[inline]
    fn from(bytes: BytesTransferred) -> Self {
        bytes.0
    }
}

impl std::ops::Deref for BytesTransferred {
    type Target = u64;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Add for BytesTransferred {
    type Output = Self;

    #[inline]
    fn add(self, other: Self) -> Self {
        Self(self.0 + other.0)
    }
}

impl AddAssign for BytesTransferred {
    #[inline]
    fn add_assign(&mut self, other: Self) {
        self.0 += other.0;
    }
}

impl fmt::Display for BytesTransferred {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0)
    }
}

/// Transfer statistics for a session
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferMetrics {
    /// Bytes sent from client to backend
    pub client_to_backend: BytesTransferred,
    /// Bytes sent from backend to client
    pub backend_to_client: BytesTransferred,
}

impl TransferMetrics {
    /// Create new transfer metrics with zero bytes
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            client_to_backend: BytesTransferred::zero(),
            backend_to_client: BytesTransferred::zero(),
        }
    }

    /// Create new transfer metrics from raw byte counts
    #[must_use]
    pub const fn new(client_to_backend: u64, backend_to_client: u64) -> Self {
        Self {
            client_to_backend: BytesTransferred::new(client_to_backend),
            backend_to_client: BytesTransferred::new(backend_to_client),
        }
    }

    /// Get total bytes transferred in both directions
    #[must_use]
    pub fn total(&self) -> BytesTransferred {
        self.client_to_backend + self.backend_to_client
    }

    /// Convert to a tuple of (client_to_backend, backend_to_client)
    #[must_use]
    pub fn as_tuple(&self) -> (u64, u64) {
        (
            self.client_to_backend.as_u64(),
            self.backend_to_client.as_u64(),
        )
    }
}

impl From<(u64, u64)> for TransferMetrics {
    fn from((c2b, b2c): (u64, u64)) -> Self {
        Self::new(c2b, b2c)
    }
}

impl fmt::Display for TransferMetrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "sent: {}, received: {}",
            self.client_to_backend, self.backend_to_client
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bytes_transferred_basic() {
        let bytes = BytesTransferred::new(1024);
        assert_eq!(bytes.as_u64(), 1024);
        assert_eq!(BytesTransferred::zero().as_u64(), 0);
    }

    #[test]
    fn test_bytes_transferred_add() {
        let mut bytes = BytesTransferred::new(100);
        BytesTransferred::add(&mut bytes, 50);
        assert_eq!(bytes.as_u64(), 150);
    }

    #[test]
    fn test_transfer_metrics_basic() {
        let metrics = TransferMetrics::new(1024, 2048);
        assert_eq!(metrics.client_to_backend.as_u64(), 1024);
        assert_eq!(metrics.backend_to_client.as_u64(), 2048);
        assert_eq!(metrics.total().as_u64(), 3072);
    }
}
