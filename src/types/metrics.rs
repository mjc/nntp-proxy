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

    #[test]
    fn test_bytes_transferred_basic() {
        assert_eq!(BytesTransferred::new(1024).as_u64(), 1024);
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
        assert_eq!(metrics.total(), 3072);
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
