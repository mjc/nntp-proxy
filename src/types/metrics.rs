//! Type-safe metrics and measurement types

use std::fmt;
use std::ops::{Add, AddAssign};

/// Type-safe byte transfer counter
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct BytesTransferred(u64);

impl BytesTransferred {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn zero() -> Self {
        Self::ZERO
    }

    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    #[inline]
    pub fn add(&mut self, bytes: usize) {
        self.0 += bytes as u64;
    }

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

/// Client traffic metrics (Client ↔ Proxy)
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ClientBytes(u64);

impl ClientBytes {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    #[must_use]
    #[inline]
    pub const fn saturating_sub(self, other: Self) -> u64 {
        self.0.saturating_sub(other.0)
    }
}

impl From<u64> for ClientBytes {
    #[inline]
    fn from(bytes: u64) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for ClientBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0)
    }
}

/// Client → Backend traffic (request bytes)
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ClientToBackendBytes(u64);

impl ClientToBackendBytes {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn zero() -> Self {
        Self::ZERO
    }

    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    #[inline]
    pub fn add(&mut self, bytes: usize) {
        self.0 += bytes as u64;
    }

    #[inline]
    pub fn add_u64(&mut self, bytes: u64) {
        self.0 += bytes;
    }

    #[must_use]
    #[inline]
    pub const fn saturating_sub(self, other: Self) -> u64 {
        self.0.saturating_sub(other.0)
    }
}

impl From<u64> for ClientToBackendBytes {
    #[inline]
    fn from(bytes: u64) -> Self {
        Self(bytes)
    }
}

impl From<BytesTransferred> for ClientToBackendBytes {
    #[inline]
    fn from(bytes: BytesTransferred) -> Self {
        Self(bytes.as_u64())
    }
}

impl Add for ClientToBackendBytes {
    type Output = Self;
    #[inline]
    fn add(self, other: Self) -> Self {
        Self(self.0 + other.0)
    }
}

impl AddAssign for ClientToBackendBytes {
    #[inline]
    fn add_assign(&mut self, other: Self) {
        self.0 += other.0;
    }
}

impl fmt::Display for ClientToBackendBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0)
    }
}

/// Backend → Client traffic (response bytes)
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct BackendToClientBytes(u64);

impl BackendToClientBytes {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn zero() -> Self {
        Self::ZERO
    }

    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    #[inline]
    pub fn add(&mut self, bytes: usize) {
        self.0 += bytes as u64;
    }

    #[inline]
    pub fn add_u64(&mut self, bytes: u64) {
        self.0 += bytes;
    }

    #[must_use]
    #[inline]
    pub const fn saturating_sub(self, other: Self) -> u64 {
        self.0.saturating_sub(other.0)
    }
}

impl From<u64> for BackendToClientBytes {
    #[inline]
    fn from(bytes: u64) -> Self {
        Self(bytes)
    }
}

impl From<BytesTransferred> for BackendToClientBytes {
    #[inline]
    fn from(bytes: BytesTransferred) -> Self {
        Self(bytes.as_u64())
    }
}

impl Add for BackendToClientBytes {
    type Output = Self;
    #[inline]
    fn add(self, other: Self) -> Self {
        Self(self.0 + other.0)
    }
}

impl AddAssign for BackendToClientBytes {
    #[inline]
    fn add_assign(&mut self, other: Self) {
        self.0 += other.0;
    }
}

impl fmt::Display for BackendToClientBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0)
    }
}

/// Legacy type alias for backwards compatibility during refactor
#[deprecated(note = "Use ClientToBackendBytes or BackendToClientBytes instead")]
pub type BackendBytes = ClientToBackendBytes;

/// Transfer statistics for a session
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferMetrics {
    pub client_to_backend: ClientToBackendBytes,
    pub backend_to_client: BackendToClientBytes,
}

impl TransferMetrics {
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            client_to_backend: ClientToBackendBytes::ZERO,
            backend_to_client: BackendToClientBytes::ZERO,
        }
    }

    #[must_use]
    pub const fn new(client_to_backend: u64, backend_to_client: u64) -> Self {
        Self {
            client_to_backend: ClientToBackendBytes(client_to_backend),
            backend_to_client: BackendToClientBytes(backend_to_client),
        }
    }

    #[must_use]
    #[inline]
    pub fn total(&self) -> u64 {
        self.client_to_backend.as_u64() + self.backend_to_client.as_u64()
    }

    #[must_use]
    #[inline]
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
    use proptest::prelude::*;

    // ============================================================================
    // Property Tests - BytesTransferred
    // ============================================================================

    proptest! {
        /// Property: new() and as_u64() round-trip correctly
        #[test]
        fn prop_bytes_transferred_roundtrip(value in 0u64..=u64::MAX / 2) {
            let bytes = BytesTransferred::new(value);
            prop_assert_eq!(bytes.as_u64(), value);
        }

        /// Property: add() increments correctly (using explicit method call)
        #[test]
        fn prop_bytes_transferred_add(initial in 0u64..1000000, increment in 0usize..1000000) {
            let mut bytes = BytesTransferred::new(initial);
            BytesTransferred::add(&mut bytes, increment);
            prop_assert_eq!(bytes.as_u64(), initial + increment as u64);
        }

        /// Property: add_u64() increments correctly
        #[test]
        fn prop_bytes_transferred_add_u64(initial in 0u64..1000000, increment in 0u64..1000000) {
            let mut bytes = BytesTransferred::new(initial);
            bytes.add_u64(increment);
            prop_assert_eq!(bytes.as_u64(), initial + increment);
        }

        /// Property: Addition operator is commutative
        #[test]
        fn prop_bytes_transferred_add_commutative(a in 0u64..1000000, b in 0u64..1000000) {
            let bytes_a = BytesTransferred::new(a);
            let bytes_b = BytesTransferred::new(b);
            prop_assert_eq!(bytes_a + bytes_b, bytes_b + bytes_a);
        }

        /// Property: From<u64> conversion
        #[test]
        fn prop_bytes_transferred_from_u64(value in 0u64..=u64::MAX / 2) {
            let bytes = BytesTransferred::from(value);
            prop_assert_eq!(bytes.as_u64(), value);
        }
    }

    // Edge cases for BytesTransferred
    #[test]
    fn test_bytes_transferred_zero() {
        assert_eq!(BytesTransferred::zero().as_u64(), 0);
        assert_eq!(BytesTransferred::ZERO.as_u64(), 0);
    }

    #[test]
    fn test_bytes_transferred_add_assign() {
        let mut bytes = BytesTransferred::new(100);
        bytes += BytesTransferred::new(50);
        assert_eq!(bytes.as_u64(), 150);
    }

    // ============================================================================
    // Property Tests - ClientBytes
    // ============================================================================

    proptest! {
        /// Property: saturating_sub never underflows
        #[test]
        fn prop_client_bytes_saturating_sub(a in 0u64..1000000, b in 0u64..1000000) {
            let bytes_a = ClientBytes::new(a);
            let bytes_b = ClientBytes::new(b);
            let result = bytes_a.saturating_sub(bytes_b);

            if a >= b {
                prop_assert_eq!(result, a - b);
            } else {
                prop_assert_eq!(result, 0);
            }
        }

        /// Property: ClientBytes round-trip
        #[test]
        fn prop_client_bytes_roundtrip(value in 0u64..=u64::MAX / 2) {
            let bytes = ClientBytes::new(value);
            prop_assert_eq!(bytes.as_u64(), value);
        }
    }

    #[test]
    fn test_client_bytes_zero() {
        assert_eq!(ClientBytes::ZERO.as_u64(), 0);
    }

    // ============================================================================
    // Property Tests - ClientToBackendBytes
    // ============================================================================

    proptest! {
        /// Property: add() works correctly
        #[test]
        fn prop_client_to_backend_add(initial in 0u64..1000000, increment in 0usize..1000000) {
            let mut bytes = ClientToBackendBytes::new(initial);
            ClientToBackendBytes::add(&mut bytes, increment);
            prop_assert_eq!(bytes.as_u64(), initial + increment as u64);
        }

        /// Property: add_u64() works correctly
        #[test]
        fn prop_client_to_backend_add_u64(initial in 0u64..1000000, increment in 0u64..1000000) {
            let mut bytes = ClientToBackendBytes::new(initial);
            bytes.add_u64(increment);
            prop_assert_eq!(bytes.as_u64(), initial + increment);
        }

        /// Property: Addition operators work
        #[test]
        fn prop_client_to_backend_operators(a in 0u64..1000000, b in 0u64..1000000) {
            let bytes_a = ClientToBackendBytes::new(a);
            let bytes_b = ClientToBackendBytes::new(b);

            // Test + operator
            let sum1 = bytes_a + bytes_b;
            prop_assert_eq!(sum1.as_u64(), a + b);

            // Test += operator
            let mut sum2 = bytes_a;
            sum2 += bytes_b;
            prop_assert_eq!(sum2.as_u64(), a + b);
        }

        /// Property: saturating_sub never underflows
        #[test]
        fn prop_client_to_backend_saturating_sub(a in 0u64..1000000, b in 0u64..1000000) {
            let bytes_a = ClientToBackendBytes::new(a);
            let bytes_b = ClientToBackendBytes::new(b);
            let result = bytes_a.saturating_sub(bytes_b);
            prop_assert_eq!(result, a.saturating_sub(b));
        }
    }

    #[test]
    fn test_client_to_backend_zero() {
        assert_eq!(ClientToBackendBytes::zero().as_u64(), 0);
    }

    // ============================================================================
    // Property Tests - BackendToClientBytes
    // ============================================================================

    proptest! {
        /// Property: add() works correctly
        #[test]
        fn prop_backend_to_client_add(initial in 0u64..1000000, increment in 0usize..1000000) {
            let mut bytes = BackendToClientBytes::new(initial);
            BackendToClientBytes::add(&mut bytes, increment);
            prop_assert_eq!(bytes.as_u64(), initial + increment as u64);
        }

        /// Property: Addition operators work
        #[test]
        fn prop_backend_to_client_operators(a in 0u64..1000000, b in 0u64..1000000) {
            let bytes_a = BackendToClientBytes::new(a);
            let bytes_b = BackendToClientBytes::new(b);

            let sum1 = bytes_a + bytes_b;
            prop_assert_eq!(sum1.as_u64(), a + b);

            let mut sum2 = bytes_a;
            sum2 += bytes_b;
            prop_assert_eq!(sum2.as_u64(), a + b);
        }

        /// Property: saturating_sub never underflows
        #[test]
        fn prop_backend_to_client_saturating_sub(a in 0u64..1000000, b in 0u64..1000000) {
            let bytes_a = BackendToClientBytes::new(a);
            let bytes_b = BackendToClientBytes::new(b);
            prop_assert_eq!(bytes_a.saturating_sub(bytes_b), a.saturating_sub(b));
        }
    }

    // ============================================================================
    // Property Tests - BytesSent & BytesReceived
    // ============================================================================

    proptest! {
        /// Property: BytesSent saturating_sub
        #[test]
        fn prop_bytes_sent_saturating_sub(a in 0u64..1000000, b in 0u64..1000000) {
            let bytes_a = BytesSent::new(a);
            let bytes_b = BytesSent::new(b);
            prop_assert_eq!(bytes_a.saturating_sub(bytes_b), a.saturating_sub(b));
        }

        /// Property: BytesReceived saturating_sub
        #[test]
        fn prop_bytes_received_saturating_sub(a in 0u64..1000000, b in 0u64..1000000) {
            let bytes_a = BytesReceived::new(a);
            let bytes_b = BytesReceived::new(b);
            prop_assert_eq!(bytes_a.saturating_sub(bytes_b), a.saturating_sub(b));
        }
    }

    // ============================================================================
    // Property Tests - From Conversions
    // ============================================================================

    proptest! {
        /// Property: All From<u64> conversions work
        #[test]
        fn prop_from_u64_conversions(value in 0u64..1000000) {
            prop_assert_eq!(BytesTransferred::from(value).as_u64(), value);
            prop_assert_eq!(ClientBytes::from(value).as_u64(), value);
            prop_assert_eq!(ClientToBackendBytes::from(value).as_u64(), value);
            prop_assert_eq!(BackendToClientBytes::from(value).as_u64(), value);
            prop_assert_eq!(BytesSent::from(value).as_u64(), value);
            prop_assert_eq!(BytesReceived::from(value).as_u64(), value);
            prop_assert_eq!(TotalConnections::from(value).get(), value);
            prop_assert_eq!(BytesPerSecondRate::from(value).get(), value);
            prop_assert_eq!(ArticleBytesTotal::from(value).get(), value);
            prop_assert_eq!(TimingMeasurementCount::from(value).get(), value);
        }

        /// Property: Into<u64> conversion
        #[test]
        fn prop_into_u64(value in 0u64..1000000) {
            let bytes = BytesTransferred::new(value);
            let converted: u64 = bytes.into();
            prop_assert_eq!(converted, value);
        }
    }

    // ============================================================================
    // Property Tests - Display Implementations
    // ============================================================================

    proptest! {
        /// Property: Display implementations contain the value
        #[test]
        fn prop_display_implementations(value in 0u64..10000) {
            let value_str = value.to_string();

            prop_assert!(BytesTransferred::new(value).to_string().contains(&value_str));
            prop_assert!(ClientBytes::new(value).to_string().contains(&value_str));
            prop_assert!(ClientToBackendBytes::new(value).to_string().contains(&value_str));
            prop_assert!(BackendToClientBytes::new(value).to_string().contains(&value_str));
            prop_assert!(BytesSent::new(value).to_string().contains(&value_str));
            prop_assert!(BytesReceived::new(value).to_string().contains(&value_str));
            prop_assert!(TotalConnections::new(value).to_string().contains(&value_str));
            prop_assert!(ArticleBytesTotal::new(value).to_string().contains(&value_str));
        }
    }

    // ============================================================================
    // Edge Cases & Cross-Type Conversions
    // ============================================================================

    #[test]
    fn test_bytes_transferred_from_conversion() {
        let transferred = BytesTransferred::new(100);
        let client_to_backend = ClientToBackendBytes::from(transferred);
        assert_eq!(client_to_backend.as_u64(), 100);

        let backend_to_client = BackendToClientBytes::from(transferred);
        assert_eq!(backend_to_client.as_u64(), 100);
    }

    #[test]
    fn test_zero_constants() {
        assert_eq!(BytesSent::ZERO.as_u64(), 0);
        assert_eq!(BytesReceived::ZERO.as_u64(), 0);
        assert_eq!(TotalConnections::ZERO.get(), 0);
        assert_eq!(BytesPerSecondRate::ZERO.get(), 0);
        assert_eq!(ArticleBytesTotal::ZERO.get(), 0);
        assert_eq!(TimingMeasurementCount::ZERO.get(), 0);
    }
}

// ============================================================================
// Per-Backend/User Byte Counters
// ============================================================================

/// Bytes sent by a backend or user
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct BytesSent(u64);

impl BytesSent {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    #[must_use]
    #[inline]
    pub const fn saturating_sub(self, other: Self) -> u64 {
        self.0.saturating_sub(other.0)
    }
}

impl From<u64> for BytesSent {
    #[inline]
    fn from(bytes: u64) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for BytesSent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0)
    }
}

/// Bytes received by a backend or user
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct BytesReceived(u64);

impl BytesReceived {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        self.0
    }

    #[must_use]
    #[inline]
    pub const fn saturating_sub(self, other: Self) -> u64 {
        self.0.saturating_sub(other.0)
    }
}

impl From<u64> for BytesReceived {
    #[inline]
    fn from(bytes: u64) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for BytesReceived {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0)
    }
}

/// Total connections count
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct TotalConnections(u64);

impl TotalConnections {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(count: u64) -> Self {
        Self(count)
    }

    #[must_use]
    pub const fn get(&self) -> u64 {
        self.0
    }
}

impl From<u64> for TotalConnections {
    #[inline]
    fn from(count: u64) -> Self {
        Self(count)
    }
}

impl fmt::Display for TotalConnections {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} connections", self.0)
    }
}

/// Bytes per second (rate)
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct BytesPerSecondRate(u64);

impl BytesPerSecondRate {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(rate: u64) -> Self {
        Self(rate)
    }

    #[must_use]
    pub const fn get(&self) -> u64 {
        self.0
    }
}

impl From<u64> for BytesPerSecondRate {
    #[inline]
    fn from(rate: u64) -> Self {
        Self(rate)
    }
}

impl fmt::Display for BytesPerSecondRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} B/s", self.0)
    }
}

/// Article bytes total (cumulative)
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ArticleBytesTotal(u64);

impl ArticleBytesTotal {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn get(&self) -> u64 {
        self.0
    }
}

impl From<u64> for ArticleBytesTotal {
    #[inline]
    fn from(bytes: u64) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for ArticleBytesTotal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.0)
    }
}

/// Timing measurement count (for averaging TTFB/send/recv times)
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct TimingMeasurementCount(u64);

impl TimingMeasurementCount {
    pub const ZERO: Self = Self(0);

    #[must_use]
    pub const fn new(count: u64) -> Self {
        Self(count)
    }

    #[must_use]
    pub const fn get(&self) -> u64 {
        self.0
    }
}

impl From<u64> for TimingMeasurementCount {
    #[inline]
    fn from(count: u64) -> Self {
        Self(count)
    }
}
