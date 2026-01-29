//! Type-safe metrics and measurement types
//!
//! Uses phantom types to provide zero-cost newtype wrappers with compile-time direction tracking.
//! All byte counter types share the same implementation via `ByteCounter<D>` where `D` is a
//! direction marker type (ClientToBackend, BackendToClient, etc.).

use std::fmt;
use std::marker::PhantomData;
use std::ops::{Add, AddAssign};

// ============================================================================
// Direction Marker Types (zero-sized)
// ============================================================================

/// Marker: Client → Backend traffic
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ClientToBackend;

/// Marker: Backend → Client traffic
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct BackendToClient;

/// Marker: Generic client traffic
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Client;

/// Marker: Bytes sent
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Sent;

/// Marker: Bytes received
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Received;

// ============================================================================
// Generic ByteCounter with Phantom Type Direction
// ============================================================================

/// Generic byte counter with compile-time direction tracking via phantom types
///
/// This zero-cost abstraction provides type-safe byte counting where the direction
/// is encoded in the type system. The `PhantomData<D>` has zero size at runtime.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ByteCounter<D> {
    bytes: u64,
    _direction: PhantomData<D>,
}

impl<D> ByteCounter<D> {
    pub const ZERO: Self = Self {
        bytes: 0,
        _direction: PhantomData,
    };

    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self {
            bytes,
            _direction: PhantomData,
        }
    }

    #[must_use]
    pub const fn zero() -> Self {
        Self::ZERO
    }

    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        self.bytes
    }

    #[must_use]
    #[inline]
    pub const fn add(self, bytes: usize) -> Self {
        Self {
            bytes: self.bytes + bytes as u64,
            _direction: PhantomData,
        }
    }

    #[must_use]
    #[inline]
    pub const fn add_u64(self, bytes: u64) -> Self {
        Self {
            bytes: self.bytes + bytes,
            _direction: PhantomData,
        }
    }

    #[must_use]
    #[inline]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self {
            bytes: self.bytes.saturating_sub(other.bytes),
            _direction: PhantomData,
        }
    }
}

impl<D> From<u64> for ByteCounter<D> {
    #[inline]
    fn from(bytes: u64) -> Self {
        Self::new(bytes)
    }
}

impl<D> From<ByteCounter<D>> for u64 {
    #[inline]
    fn from(counter: ByteCounter<D>) -> Self {
        counter.bytes
    }
}

impl<D> Add for ByteCounter<D> {
    type Output = Self;
    #[inline]
    fn add(self, other: Self) -> Self {
        Self {
            bytes: self.bytes + other.bytes,
            _direction: PhantomData,
        }
    }
}

impl<D> AddAssign for ByteCounter<D> {
    #[inline]
    fn add_assign(&mut self, other: Self) {
        self.bytes += other.bytes;
    }
}

impl<D> fmt::Display for ByteCounter<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} bytes", self.bytes)
    }
}

// ============================================================================
// Type Aliases for Direction-Specific Counters
// ============================================================================

/// Client traffic metrics (Client ↔ Proxy)
pub type ClientBytes = ByteCounter<Client>;

/// Client → Backend traffic (request bytes)
pub type ClientToBackendBytes = ByteCounter<ClientToBackend>;

/// Backend → Client traffic (response bytes)
pub type BackendToClientBytes = ByteCounter<BackendToClient>;

/// Bytes sent by a backend or user
pub type BytesSent = ByteCounter<Sent>;

/// Bytes received by a backend or user
pub type BytesReceived = ByteCounter<Received>;

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
            client_to_backend: ClientToBackendBytes::new(client_to_backend),
            backend_to_client: BackendToClientBytes::new(backend_to_client),
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

    /// Saturating subtraction of two transfer metrics
    #[must_use]
    pub fn saturating_sub(self, other: Self) -> Self {
        Self {
            client_to_backend: self
                .client_to_backend
                .saturating_sub(other.client_to_backend),
            backend_to_client: self
                .backend_to_client
                .saturating_sub(other.backend_to_client),
        }
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
    // Critical Property Tests - Arithmetic Operations
    // ============================================================================

    proptest! {
        /// Property: ByteCounter arithmetic works correctly across all direction types
        #[test]
        fn prop_byte_counter_arithmetic(initial in 0u64..1000000, increment in 0usize..1000000, b in 0u64..1000000) {
            // Test add() method
            let c2b = ClientToBackendBytes::new(initial).add(increment);
            let b2c = BackendToClientBytes::new(initial).add(increment);
            prop_assert_eq!(c2b.as_u64(), initial + increment as u64);
            prop_assert_eq!(b2c.as_u64(), initial + increment as u64);

            // Test Add trait
            let sum1 = ClientToBackendBytes::new(initial) + ClientToBackendBytes::new(b);
            let sum2 = BackendToClientBytes::new(initial) + BackendToClientBytes::new(b);
            prop_assert_eq!(sum1.as_u64(), initial + b);
            prop_assert_eq!(sum2.as_u64(), initial + b);

            // Test saturating_sub
            prop_assert_eq!(
                ClientToBackendBytes::new(initial).saturating_sub(ClientToBackendBytes::new(b)).as_u64(),
                initial.saturating_sub(b)
            );
            prop_assert_eq!(
                BackendToClientBytes::new(initial).saturating_sub(BackendToClientBytes::new(b)).as_u64(),
                initial.saturating_sub(b)
            );
        }
    }

    #[test]
    fn test_phantom_type_zero_size() {
        use std::mem::size_of;
        // Verify PhantomData has zero runtime cost
        assert_eq!(size_of::<ClientToBackendBytes>(), size_of::<u64>());
        assert_eq!(size_of::<BackendToClientBytes>(), size_of::<u64>());
        assert_eq!(size_of::<BytesSent>(), size_of::<u64>());
    }

    // ============================================================================
    // TransferMetrics Tests
    // ============================================================================

    #[test]
    fn test_transfer_metrics_total() {
        let metrics = TransferMetrics::new(100, 200);
        assert_eq!(metrics.total(), 300);
        assert_eq!(metrics.as_tuple(), (100, 200));
    }

    proptest! {
        /// Property: total() equals sum of components
        #[test]
        fn prop_transfer_metrics_total(c2b in 0u64..1000000, b2c in 0u64..1000000) {
            let metrics = TransferMetrics::new(c2b, b2c);
            prop_assert_eq!(metrics.total(), c2b + b2c);
        }

        /// Property: saturating_sub works on TransferMetrics
        #[test]
        fn prop_transfer_metrics_saturating_sub(a1 in 0u64..1000000, a2 in 0u64..1000000, b1 in 0u64..1000000, b2 in 0u64..1000000) {
            let metrics1 = TransferMetrics::new(a1, a2);
            let metrics2 = TransferMetrics::new(b1, b2);
            let diff = metrics1.saturating_sub(metrics2);

            prop_assert_eq!(diff.client_to_backend.as_u64(), a1.saturating_sub(b1));
            prop_assert_eq!(diff.backend_to_client.as_u64(), a2.saturating_sub(b2));
        }
    }

    // ========================================================================
    // define_counter! macro tests — Display with unit and empty unit
    // ========================================================================

    #[test]
    fn test_counter_display_with_unit() {
        let c = TotalConnections::new(42);
        assert_eq!(format!("{}", c), "42 connections");
    }

    #[test]
    fn test_counter_display_with_unit_zero() {
        let c = TotalConnections::new(0);
        assert_eq!(format!("{}", c), "0 connections");
    }

    #[test]
    fn test_counter_display_bytes_per_second() {
        let c = BytesPerSecondRate::new(1500);
        assert_eq!(format!("{}", c), "1500 B/s");
    }

    #[test]
    fn test_counter_display_article_bytes() {
        let c = ArticleBytesTotal::new(999999);
        assert_eq!(format!("{}", c), "999999 bytes");
    }

    #[test]
    fn test_counter_display_empty_unit() {
        // TimingMeasurementCount uses empty unit — should NOT have trailing space
        let c = TimingMeasurementCount::new(7);
        assert_eq!(format!("{}", c), "7");
    }

    #[test]
    fn test_counter_display_empty_unit_zero() {
        let c = TimingMeasurementCount::new(0);
        assert_eq!(format!("{}", c), "0");
    }

    #[test]
    fn test_counter_display_empty_unit_large() {
        let c = TimingMeasurementCount::new(u64::MAX);
        assert_eq!(format!("{}", c), format!("{}", u64::MAX));
    }

    #[test]
    fn test_timing_measurement_count_new_and_get() {
        let c = TimingMeasurementCount::new(123);
        assert_eq!(c.get(), 123);
    }

    #[test]
    fn test_timing_measurement_count_zero_constant() {
        assert_eq!(TimingMeasurementCount::ZERO.get(), 0);
    }

    #[test]
    fn test_timing_measurement_count_from_u64() {
        let c = TimingMeasurementCount::from(55u64);
        assert_eq!(c.get(), 55);
    }

    #[test]
    fn test_timing_measurement_count_default() {
        let c = TimingMeasurementCount::default();
        assert_eq!(c.get(), 0);
    }

    #[test]
    fn test_timing_measurement_count_eq() {
        assert_eq!(
            TimingMeasurementCount::new(10),
            TimingMeasurementCount::new(10)
        );
        assert_ne!(
            TimingMeasurementCount::new(10),
            TimingMeasurementCount::new(11)
        );
    }

    #[test]
    fn test_timing_measurement_count_ord() {
        assert!(TimingMeasurementCount::new(5) < TimingMeasurementCount::new(10));
        assert!(TimingMeasurementCount::new(10) > TimingMeasurementCount::new(5));
    }

    #[test]
    fn test_timing_measurement_count_clone_copy() {
        let a = TimingMeasurementCount::new(42);
        let b = a; // Copy
        assert_eq!(a, b);
    }

    #[test]
    fn test_timing_measurement_count_debug() {
        let c = TimingMeasurementCount::new(99);
        let dbg = format!("{:?}", c);
        assert!(dbg.contains("99"));
    }

    #[test]
    fn test_counter_repr_transparent() {
        use std::mem::size_of;
        // All counter types should be exactly u64-sized due to #[repr(transparent)]
        assert_eq!(size_of::<TotalConnections>(), size_of::<u64>());
        assert_eq!(size_of::<BytesPerSecondRate>(), size_of::<u64>());
        assert_eq!(size_of::<ArticleBytesTotal>(), size_of::<u64>());
        assert_eq!(size_of::<TimingMeasurementCount>(), size_of::<u64>());
    }
}

// ============================================================================
// Macro for Display-Oriented Counter Types (with unit strings)
// ============================================================================

/// Define a counter type with a unit string for display formatting.
///
/// Unlike `metrics::types::counter_type!` which is for internal counting operations
/// with `increment()` and `saturating_sub()`, this macro creates types focused on
/// display with a unit suffix (e.g., "42 connections").
macro_rules! define_counter {
    ($name:ident, $unit:expr) => {
        #[repr(transparent)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
        pub struct $name(u64);

        impl $name {
            pub const ZERO: Self = Self(0);

            #[must_use]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn get(&self) -> u64 {
                self.0
            }
        }

        impl From<u64> for $name {
            #[inline]
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let unit: &str = $unit;
                if unit.is_empty() {
                    write!(f, "{}", self.0)
                } else {
                    write!(f, "{} {}", self.0, unit)
                }
            }
        }
    };
}

// ============================================================================
// Specific Counter Types
// ============================================================================

define_counter!(TotalConnections, "connections");
define_counter!(BytesPerSecondRate, "B/s");
define_counter!(ArticleBytesTotal, "bytes");

define_counter!(TimingMeasurementCount, "");
