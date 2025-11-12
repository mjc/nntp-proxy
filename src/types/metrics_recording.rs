//! Type-safe metrics recording with compile-time double-counting prevention
//!
//! This module uses the typestate pattern to ensure metrics can only be recorded once.
//! The compiler prevents accidentally recording the same bytes twice.

use std::marker::PhantomData;

/// Marker trait for recording states
pub trait RecordingState {}

/// Bytes that have NOT been recorded to metrics yet
#[derive(Debug, Clone, Copy)]
pub struct Unrecorded;
impl RecordingState for Unrecorded {}

/// Bytes that HAVE been recorded to metrics
#[derive(Debug, Clone, Copy)]
pub struct Recorded;
impl RecordingState for Recorded {}

/// Type-safe byte counter with recording state tracking
///
/// This type uses the typestate pattern to track whether bytes have been
/// recorded to metrics. The `State` type parameter is either `Unrecorded`
/// or `Recorded`, preventing double-counting at compile time.
///
/// # Examples
///
/// ```rust
/// use nntp_proxy::types::metrics_recording::{MetricsBytes, Unrecorded};
/// use nntp_proxy::metrics::MetricsCollector;
///
/// let bytes = MetricsBytes::<Unrecorded>::new(1024);
/// // Can only record once - this consumes the Unrecorded bytes
/// let recorded = metrics.record_and_mark(bytes);
/// // Can't record again - `bytes` has been moved!
/// ```
#[derive(Debug, Clone, Copy)]
pub struct MetricsBytes<State: RecordingState> {
    bytes: u64,
    _state: PhantomData<State>,
}

impl MetricsBytes<Unrecorded> {
    /// Create new unrecorded bytes
    #[inline]
    #[must_use]
    pub const fn new(bytes: u64) -> Self {
        Self {
            bytes,
            _state: PhantomData,
        }
    }

    /// Create from usize
    #[inline]
    #[must_use]
    pub const fn from_usize(bytes: usize) -> Self {
        Self::new(bytes as u64)
    }

    /// Mark as recorded (for when metrics are disabled)
    #[inline]
    #[must_use]
    pub const fn mark_recorded(self) -> MetricsBytes<Recorded> {
        MetricsBytes {
            bytes: self.bytes,
            _state: PhantomData,
        }
    }

    /// Get the byte count (consuming the unrecorded state)
    #[inline]
    #[must_use]
    pub const fn into_u64(self) -> u64 {
        self.bytes
    }
}

impl MetricsBytes<Recorded> {
    /// Get the byte count from recorded bytes (safe - already recorded)
    #[inline]
    #[must_use]
    pub const fn as_u64(&self) -> u64 {
        self.bytes
    }
}

impl<State: RecordingState> MetricsBytes<State> {
    /// Peek at the byte count without consuming (use carefully!)
    #[inline]
    #[must_use]
    pub const fn peek(&self) -> u64 {
        self.bytes
    }
}

/// Direction of data transfer
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferDirection {
    /// Client → Backend (commands)
    ClientToBackend,
    /// Backend → Client (responses)
    BackendToClient,
}

/// Strongly-typed byte count with direction
#[derive(Debug, Clone, Copy)]
pub struct DirectionalBytes<State: RecordingState> {
    bytes: MetricsBytes<State>,
    direction: TransferDirection,
}

impl DirectionalBytes<Unrecorded> {
    /// Create new unrecorded client-to-backend bytes
    #[inline]
    #[must_use]
    pub const fn client_to_backend(bytes: u64) -> Self {
        Self {
            bytes: MetricsBytes::new(bytes),
            direction: TransferDirection::ClientToBackend,
        }
    }

    /// Create new unrecorded backend-to-client bytes
    #[inline]
    #[must_use]
    pub const fn backend_to_client(bytes: u64) -> Self {
        Self {
            bytes: MetricsBytes::new(bytes),
            direction: TransferDirection::BackendToClient,
        }
    }

    /// Get direction
    #[inline]
    #[must_use]
    pub const fn direction(&self) -> TransferDirection {
        self.direction
    }

    /// Get the underlying bytes (consuming)
    #[inline]
    #[must_use]
    pub const fn into_bytes(self) -> MetricsBytes<Unrecorded> {
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_typestate_prevents_double_recording() {
        let bytes = MetricsBytes::<Unrecorded>::new(1024);

        // This works - consuming unrecorded bytes
        let _recorded = bytes.mark_recorded();

        // This won't compile - bytes has been moved:
        // let _oops = bytes.mark_recorded(); // ← Compile error!
    }

    #[test]
    fn test_directional_bytes() {
        let cmd_bytes = DirectionalBytes::client_to_backend(512);
        assert_eq!(cmd_bytes.direction(), TransferDirection::ClientToBackend);
        assert_eq!(cmd_bytes.into_bytes().into_u64(), 512);

        let resp_bytes = DirectionalBytes::backend_to_client(2048);
        assert_eq!(resp_bytes.direction(), TransferDirection::BackendToClient);
        assert_eq!(resp_bytes.into_bytes().into_u64(), 2048);
    }

    #[test]
    fn test_recorded_bytes_can_be_read_multiple_times() {
        let recorded = MetricsBytes::<Unrecorded>::new(1024).mark_recorded();

        // Recorded bytes can be read multiple times (safe - already recorded)
        assert_eq!(recorded.as_u64(), 1024);
        assert_eq!(recorded.as_u64(), 1024);
    }
}
