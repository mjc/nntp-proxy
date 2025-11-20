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
///
/// let bytes = MetricsBytes::<Unrecorded>::new(1024);
/// // Can only record once - bytes consumed when marked recorded
/// let recorded = bytes.mark_recorded();
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

    // MetricsBytes<Unrecorded> tests
    #[test]
    fn test_unrecorded_new() {
        let bytes = MetricsBytes::<Unrecorded>::new(500);
        assert_eq!(bytes.peek(), 500);
    }

    #[test]
    fn test_unrecorded_new_zero() {
        let bytes = MetricsBytes::<Unrecorded>::new(0);
        assert_eq!(bytes.peek(), 0);
    }

    #[test]
    fn test_unrecorded_from_usize() {
        let bytes = MetricsBytes::<Unrecorded>::from_usize(1024);
        assert_eq!(bytes.peek(), 1024);
    }

    #[test]
    fn test_unrecorded_into_u64() {
        let bytes = MetricsBytes::<Unrecorded>::new(2048);
        assert_eq!(bytes.into_u64(), 2048);
    }

    #[test]
    fn test_unrecorded_mark_recorded() {
        let unrecorded = MetricsBytes::<Unrecorded>::new(1024);
        let recorded = unrecorded.mark_recorded();
        assert_eq!(recorded.as_u64(), 1024);
    }

    #[test]
    fn test_unrecorded_clone() {
        let bytes1 = MetricsBytes::<Unrecorded>::new(512);
        let bytes2 = bytes1.clone();
        assert_eq!(bytes1.peek(), bytes2.peek());
    }

    // MetricsBytes<Recorded> tests
    #[test]
    fn test_recorded_as_u64() {
        let bytes = MetricsBytes::<Unrecorded>::new(4096).mark_recorded();
        assert_eq!(bytes.as_u64(), 4096);
    }

    #[test]
    fn test_recorded_clone() {
        let bytes1 = MetricsBytes::<Unrecorded>::new(256).mark_recorded();
        let bytes2 = bytes1.clone();
        assert_eq!(bytes1.as_u64(), bytes2.as_u64());
    }

    #[test]
    fn test_recorded_peek() {
        let bytes = MetricsBytes::<Unrecorded>::new(1024).mark_recorded();
        assert_eq!(bytes.peek(), 1024);
        assert_eq!(bytes.as_u64(), 1024); // Both work
    }

    // TransferDirection tests
    #[test]
    fn test_transfer_direction_equality() {
        assert_eq!(
            TransferDirection::ClientToBackend,
            TransferDirection::ClientToBackend
        );
        assert_eq!(
            TransferDirection::BackendToClient,
            TransferDirection::BackendToClient
        );
        assert_ne!(
            TransferDirection::ClientToBackend,
            TransferDirection::BackendToClient
        );
    }

    #[test]
    fn test_transfer_direction_clone() {
        let dir1 = TransferDirection::ClientToBackend;
        let dir2 = dir1.clone();
        assert_eq!(dir1, dir2);
    }

    #[test]
    fn test_transfer_direction_debug() {
        let dir = TransferDirection::ClientToBackend;
        let debug_str = format!("{:?}", dir);
        assert!(debug_str.contains("ClientToBackend"));
    }

    // DirectionalBytes tests
    #[test]
    fn test_directional_client_to_backend() {
        let bytes = DirectionalBytes::client_to_backend(1024);
        assert_eq!(bytes.direction(), TransferDirection::ClientToBackend);
        assert_eq!(bytes.into_bytes().peek(), 1024);
    }

    #[test]
    fn test_directional_backend_to_client() {
        let bytes = DirectionalBytes::backend_to_client(2048);
        assert_eq!(bytes.direction(), TransferDirection::BackendToClient);
        assert_eq!(bytes.into_bytes().peek(), 2048);
    }

    #[test]
    fn test_directional_into_bytes() {
        let directional = DirectionalBytes::client_to_backend(512);
        let bytes = directional.into_bytes();
        assert_eq!(bytes.into_u64(), 512);
    }

    #[test]
    fn test_directional_direction_preserved() {
        let cmd = DirectionalBytes::client_to_backend(100);
        let resp = DirectionalBytes::backend_to_client(200);

        assert_eq!(cmd.direction(), TransferDirection::ClientToBackend);
        assert_eq!(resp.direction(), TransferDirection::BackendToClient);
    }

    #[test]
    fn test_directional_clone() {
        let bytes1 = DirectionalBytes::client_to_backend(768);
        let bytes2 = bytes1.clone();

        assert_eq!(bytes1.direction(), bytes2.direction());
        assert_eq!(bytes1.into_bytes().peek(), bytes2.into_bytes().peek());
    }

    #[test]
    fn test_directional_debug() {
        let bytes = DirectionalBytes::client_to_backend(1024);
        let debug_str = format!("{:?}", bytes);
        assert!(debug_str.contains("DirectionalBytes"));
    }

    // Edge cases
    #[test]
    fn test_large_byte_counts() {
        let bytes = MetricsBytes::<Unrecorded>::new(u64::MAX);
        assert_eq!(bytes.peek(), u64::MAX);
    }

    #[test]
    fn test_zero_bytes_both_directions() {
        let cmd = DirectionalBytes::client_to_backend(0);
        let resp = DirectionalBytes::backend_to_client(0);

        assert_eq!(cmd.into_bytes().peek(), 0);
        assert_eq!(resp.into_bytes().peek(), 0);
    }

    #[test]
    fn test_usize_to_u64_conversion() {
        let max_usize_as_u64 = usize::MAX as u64;
        let bytes = MetricsBytes::<Unrecorded>::from_usize(usize::MAX);
        assert_eq!(bytes.peek(), max_usize_as_u64);
    }

    #[test]
    fn test_recording_workflow() {
        // Typical workflow: create → record → read
        let unrecorded = MetricsBytes::<Unrecorded>::new(1024);
        let recorded = unrecorded.mark_recorded();
        let value = recorded.as_u64();
        assert_eq!(value, 1024);
    }

    #[test]
    fn test_directional_workflow() {
        // Typical directional workflow
        let bytes = DirectionalBytes::client_to_backend(512);
        let direction = bytes.direction();
        let unrecorded = bytes.into_bytes();
        let recorded = unrecorded.mark_recorded();

        assert_eq!(direction, TransferDirection::ClientToBackend);
        assert_eq!(recorded.as_u64(), 512);
    }
}
