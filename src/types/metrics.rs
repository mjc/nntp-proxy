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
    }

    #[test]
    fn test_bytes_transferred_zero() {
        let bytes = BytesTransferred::zero();
        assert_eq!(bytes.as_u64(), 0);
    }

    #[test]
    fn test_bytes_transferred_default() {
        let bytes = BytesTransferred::default();
        assert_eq!(bytes.as_u64(), 0);
    }

    #[test]
    fn test_bytes_transferred_add() {
        let mut bytes = BytesTransferred::new(100);
        BytesTransferred::add(&mut bytes, 50);
        assert_eq!(bytes.as_u64(), 150);
    }

    #[test]
    fn test_bytes_transferred_add_multiple() {
        let mut bytes = BytesTransferred::zero();
        BytesTransferred::add(&mut bytes, 100);
        BytesTransferred::add(&mut bytes, 200);
        BytesTransferred::add(&mut bytes, 300);
        assert_eq!(bytes.as_u64(), 600);
    }

    #[test]
    fn test_bytes_transferred_add_zero() {
        let mut bytes = BytesTransferred::new(100);
        BytesTransferred::add(&mut bytes, 0);
        assert_eq!(bytes.as_u64(), 100);
    }

    #[test]
    fn test_bytes_transferred_add_assign() {
        let mut bytes = BytesTransferred::new(100);
        bytes += BytesTransferred::new(50);
        assert_eq!(bytes.as_u64(), 150);
    }

    #[test]
    fn test_bytes_transferred_add_trait() {
        let bytes1 = BytesTransferred::new(100);
        let bytes2 = BytesTransferred::new(50);
        let result = bytes1 + bytes2;
        assert_eq!(result.as_u64(), 150);
    }

    #[test]
    fn test_bytes_transferred_add_trait_chain() {
        let bytes1 = BytesTransferred::new(100);
        let bytes2 = BytesTransferred::new(50);
        let bytes3 = BytesTransferred::new(25);
        let result = bytes1 + bytes2 + bytes3;
        assert_eq!(result.as_u64(), 175);
    }

    #[test]
    fn test_bytes_transferred_from_u64() {
        let bytes: BytesTransferred = 1024u64.into();
        assert_eq!(bytes.as_u64(), 1024);
    }

    #[test]
    fn test_bytes_transferred_into_u64() {
        let bytes = BytesTransferred::new(2048);
        let value: u64 = bytes.into();
        assert_eq!(value, 2048);
    }

    #[test]
    fn test_bytes_transferred_display() {
        let bytes = BytesTransferred::new(1024);
        assert_eq!(format!("{}", bytes), "1024 bytes");
    }

    #[test]
    fn test_bytes_transferred_display_zero() {
        let bytes = BytesTransferred::zero();
        assert_eq!(format!("{}", bytes), "0 bytes");
    }

    #[test]
    fn test_bytes_transferred_ordering() {
        let bytes1 = BytesTransferred::new(100);
        let bytes2 = BytesTransferred::new(200);
        assert!(bytes1 < bytes2);
        assert!(bytes2 > bytes1);
        assert_eq!(bytes1, bytes1);
    }

    #[test]
    fn test_bytes_transferred_clone() {
        let bytes1 = BytesTransferred::new(100);
        let bytes2 = bytes1;
        assert_eq!(bytes1, bytes2);
    }

    #[test]
    fn test_bytes_transferred_copy() {
        let bytes1 = BytesTransferred::new(100);
        #[allow(clippy::clone_on_copy)]
        let bytes2 = bytes1.clone();
        assert_eq!(bytes1, bytes2);
    }

    #[test]
    fn test_bytes_transferred_large_values() {
        let bytes = BytesTransferred::new(u64::MAX);
        assert_eq!(bytes.as_u64(), u64::MAX);
    }

    #[test]
    fn test_transfer_metrics_new() {
        let metrics = TransferMetrics::new(1000, 2000);
        assert_eq!(metrics.client_to_backend.as_u64(), 1000);
        assert_eq!(metrics.backend_to_client.as_u64(), 2000);
    }

    #[test]
    fn test_transfer_metrics_zero() {
        let metrics = TransferMetrics::zero();
        assert_eq!(metrics.client_to_backend.as_u64(), 0);
        assert_eq!(metrics.backend_to_client.as_u64(), 0);
    }

    #[test]
    fn test_transfer_metrics_total() {
        let metrics = TransferMetrics::new(1000, 2000);
        assert_eq!(metrics.total().as_u64(), 3000);
    }

    #[test]
    fn test_transfer_metrics_total_zero() {
        let metrics = TransferMetrics::zero();
        assert_eq!(metrics.total().as_u64(), 0);
    }

    #[test]
    fn test_transfer_metrics_as_tuple() {
        let metrics = TransferMetrics::new(1000, 2000);
        assert_eq!(metrics.as_tuple(), (1000, 2000));
    }

    #[test]
    fn test_transfer_metrics_from_tuple() {
        let metrics: TransferMetrics = (1000u64, 2000u64).into();
        assert_eq!(metrics.client_to_backend.as_u64(), 1000);
        assert_eq!(metrics.backend_to_client.as_u64(), 2000);
    }

    #[test]
    fn test_transfer_metrics_display() {
        let metrics = TransferMetrics::new(1024, 2048);
        let display = format!("{}", metrics);
        assert!(display.contains("1024"));
        assert!(display.contains("2048"));
        assert!(display.contains("sent"));
        assert!(display.contains("received"));
    }

    #[test]
    fn test_transfer_metrics_clone() {
        let metrics1 = TransferMetrics::new(100, 200);
        #[allow(clippy::clone_on_copy)]
        let metrics2 = metrics1.clone();
        assert_eq!(metrics1, metrics2);
    }

    #[test]
    fn test_transfer_metrics_equality() {
        let metrics1 = TransferMetrics::new(100, 200);
        let metrics2 = TransferMetrics::new(100, 200);
        let metrics3 = TransferMetrics::new(100, 201);
        assert_eq!(metrics1, metrics2);
        assert_ne!(metrics1, metrics3);
    }

    #[test]
    fn test_transfer_metrics_asymmetric() {
        let metrics = TransferMetrics::new(5000, 100000);
        assert!(metrics.backend_to_client > metrics.client_to_backend);
        assert_eq!(metrics.total().as_u64(), 105000);
    }

    #[test]
    fn test_transfer_metrics_large_values() {
        let metrics = TransferMetrics::new(u64::MAX / 2, u64::MAX / 2);
        let total = metrics.total();
        assert_eq!(total.as_u64(), u64::MAX - 1);
    }

    #[test]
    fn test_bytes_transferred_const_new() {
        const BYTES: BytesTransferred = BytesTransferred::new(1024);
        assert_eq!(BYTES.as_u64(), 1024);
    }

    #[test]
    fn test_bytes_transferred_const_zero() {
        const ZERO: BytesTransferred = BytesTransferred::zero();
        assert_eq!(ZERO.as_u64(), 0);
    }

    #[test]
    fn test_transfer_metrics_const_new() {
        const METRICS: TransferMetrics = TransferMetrics::new(100, 200);
        assert_eq!(METRICS.client_to_backend.as_u64(), 100);
        assert_eq!(METRICS.backend_to_client.as_u64(), 200);
    }

    #[test]
    fn test_transfer_metrics_const_zero() {
        const ZERO: TransferMetrics = TransferMetrics::zero();
        assert_eq!(ZERO.client_to_backend.as_u64(), 0);
        assert_eq!(ZERO.backend_to_client.as_u64(), 0);
    }
}
