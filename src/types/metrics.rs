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

    // BytesTransferred tests
    #[test]
    fn test_bytes_transferred_basic() {
        assert_eq!(BytesTransferred::new(1024).as_u64(), 1024);
        assert_eq!(BytesTransferred::zero().as_u64(), 0);
        assert_eq!(BytesTransferred::ZERO.as_u64(), 0);
    }

    #[test]
    fn test_bytes_transferred_add() {
        let mut bytes = BytesTransferred::new(100);
        BytesTransferred::add(&mut bytes, 50);
        assert_eq!(bytes.as_u64(), 150);

        bytes.add_u64(200);
        assert_eq!(bytes.as_u64(), 350);
    }

    #[test]
    fn test_bytes_transferred_operators() {
        let a = BytesTransferred::new(100);
        let b = BytesTransferred::new(50);
        assert_eq!((a + b).as_u64(), 150);

        let mut c = BytesTransferred::new(100);
        c += BytesTransferred::new(50);
        assert_eq!(c.as_u64(), 150);
    }

    // ClientBytes tests
    #[test]
    fn test_client_bytes() {
        let bytes = ClientBytes::new(1024);
        assert_eq!(bytes.as_u64(), 1024);
        assert_eq!(ClientBytes::ZERO.as_u64(), 0);
    }

    #[test]
    fn test_client_bytes_saturating_sub() {
        let a = ClientBytes::new(100);
        let b = ClientBytes::new(30);
        assert_eq!(a.saturating_sub(b), 70);

        // Underflow protection
        assert_eq!(b.saturating_sub(a), 0);
    }

    // ClientToBackendBytes tests
    #[test]
    fn test_client_to_backend_bytes() {
        let bytes = ClientToBackendBytes::new(512);
        assert_eq!(bytes.as_u64(), 512);
        assert_eq!(ClientToBackendBytes::zero().as_u64(), 0);
    }

    #[test]
    fn test_client_to_backend_add() {
        let mut bytes = ClientToBackendBytes::new(100);
        ClientToBackendBytes::add(&mut bytes, 50);
        assert_eq!(bytes.as_u64(), 150);

        ClientToBackendBytes::add_u64(&mut bytes, 25);
        assert_eq!(bytes.as_u64(), 175);
    }

    #[test]
    fn test_client_to_backend_operators() {
        let a = ClientToBackendBytes::new(100);
        let b = ClientToBackendBytes::new(50);
        assert_eq!((a + b).as_u64(), 150);

        let mut c = ClientToBackendBytes::new(100);
        c += ClientToBackendBytes::new(50);
        assert_eq!(c.as_u64(), 150);
    }

    #[test]
    fn test_client_to_backend_saturating_sub() {
        let a = ClientToBackendBytes::new(100);
        let b = ClientToBackendBytes::new(30);
        assert_eq!(a.saturating_sub(b), 70);
        assert_eq!(b.saturating_sub(a), 0);
    }

    // BackendToClientBytes tests
    #[test]
    fn test_backend_to_client_bytes() {
        let bytes = BackendToClientBytes::new(2048);
        assert_eq!(bytes.as_u64(), 2048);
        assert_eq!(BackendToClientBytes::zero().as_u64(), 0);
    }

    #[test]
    fn test_backend_to_client_add() {
        let mut bytes = BackendToClientBytes::new(100);
        BackendToClientBytes::add(&mut bytes, 50);
        assert_eq!(bytes.as_u64(), 150);

        BackendToClientBytes::add_u64(&mut bytes, 25);
        assert_eq!(bytes.as_u64(), 175);
    }

    #[test]
    fn test_backend_to_client_operators() {
        let a = BackendToClientBytes::new(200);
        let b = BackendToClientBytes::new(100);
        assert_eq!((a + b).as_u64(), 300);

        let mut c = BackendToClientBytes::new(200);
        c += BackendToClientBytes::new(100);
        assert_eq!(c.as_u64(), 300);
    }

    #[test]
    fn test_backend_to_client_saturating_sub() {
        let a = BackendToClientBytes::new(200);
        let b = BackendToClientBytes::new(50);
        assert_eq!(a.saturating_sub(b), 150);
        assert_eq!(b.saturating_sub(a), 0);
    }

    // TransferMetrics tests
    #[test]
    fn test_transfer_metrics_basic() {
        let metrics = TransferMetrics::new(1024, 2048);
        assert_eq!(metrics.client_to_backend.as_u64(), 1024);
        assert_eq!(metrics.backend_to_client.as_u64(), 2048);
        assert_eq!(metrics.total(), 3072);
    }

    #[test]
    fn test_transfer_metrics_zero() {
        let metrics = TransferMetrics::zero();
        assert_eq!(metrics.total(), 0);
    }

    // BytesSent tests
    #[test]
    fn test_bytes_sent() {
        let bytes = BytesSent::new(512);
        assert_eq!(bytes.as_u64(), 512);
        assert_eq!(BytesSent::ZERO.as_u64(), 0);
    }

    #[test]
    fn test_bytes_sent_saturating_sub() {
        let a = BytesSent::new(100);
        let b = BytesSent::new(30);
        assert_eq!(a.saturating_sub(b), 70);
        assert_eq!(b.saturating_sub(a), 0);
    }

    // BytesReceived tests
    #[test]
    fn test_bytes_received() {
        let bytes = BytesReceived::new(1024);
        assert_eq!(bytes.as_u64(), 1024);
        assert_eq!(BytesReceived::ZERO.as_u64(), 0);
    }

    #[test]
    fn test_bytes_received_saturating_sub() {
        let a = BytesReceived::new(200);
        let b = BytesReceived::new(50);
        assert_eq!(a.saturating_sub(b), 150);
        assert_eq!(b.saturating_sub(a), 0);
    }

    // TotalConnections tests
    #[test]
    fn test_total_connections() {
        let conn = TotalConnections::new(42);
        assert_eq!(conn.get(), 42);
        assert_eq!(TotalConnections::ZERO.get(), 0);
    }

    // BytesPerSecondRate tests
    #[test]
    fn test_bytes_per_second_rate() {
        let rate = BytesPerSecondRate::new(1024);
        assert_eq!(rate.get(), 1024);
        assert_eq!(BytesPerSecondRate::ZERO.get(), 0);
    }

    // ArticleBytesTotal tests
    #[test]
    fn test_article_bytes_total() {
        let bytes = ArticleBytesTotal::new(5120);
        assert_eq!(bytes.get(), 5120);
        assert_eq!(ArticleBytesTotal::ZERO.get(), 0);
    }

    // TimingMeasurementCount tests
    #[test]
    fn test_timing_measurement_count() {
        let count = TimingMeasurementCount::new(100);
        assert_eq!(count.get(), 100);
        assert_eq!(TimingMeasurementCount::ZERO.get(), 0);
    }

    // Display trait tests
    #[test]
    fn test_display_implementations() {
        assert_eq!(BytesTransferred::new(1024).to_string(), "1024 bytes");
        assert_eq!(ClientBytes::new(512).to_string(), "512 bytes");
        assert_eq!(ClientToBackendBytes::new(256).to_string(), "256 bytes");
        assert_eq!(BackendToClientBytes::new(128).to_string(), "128 bytes");
        assert_eq!(BytesSent::new(64).to_string(), "64 bytes");
        assert_eq!(BytesReceived::new(32).to_string(), "32 bytes");
        assert_eq!(TotalConnections::new(10).to_string(), "10 connections");
        assert_eq!(BytesPerSecondRate::new(1024).to_string(), "1024 B/s");
        assert_eq!(ArticleBytesTotal::new(2048).to_string(), "2048 bytes");
    }

    // From/Into conversions
    #[test]
    fn test_from_conversions() {
        assert_eq!(BytesTransferred::from(100u64).as_u64(), 100);
        assert_eq!(ClientBytes::from(200u64).as_u64(), 200);
        assert_eq!(ClientToBackendBytes::from(300u64).as_u64(), 300);
        assert_eq!(BackendToClientBytes::from(400u64).as_u64(), 400);
        assert_eq!(BytesSent::from(500u64).as_u64(), 500);
        assert_eq!(BytesReceived::from(600u64).as_u64(), 600);
        assert_eq!(TotalConnections::from(10u64).get(), 10);
        assert_eq!(BytesPerSecondRate::from(1024u64).get(), 1024);
        assert_eq!(ArticleBytesTotal::from(2048u64).get(), 2048);
        assert_eq!(TimingMeasurementCount::from(50u64).get(), 50);
    }

    #[test]
    fn test_into_u64() {
        let bytes = BytesTransferred::new(1024);
        let value: u64 = bytes.into();
        assert_eq!(value, 1024);
    }

    #[test]
    fn test_bytes_transferred_from_conversion() {
        let transferred = BytesTransferred::new(100);
        let client_to_backend = ClientToBackendBytes::from(transferred);
        assert_eq!(client_to_backend.as_u64(), 100);

        let backend_to_client = BackendToClientBytes::from(transferred);
        assert_eq!(backend_to_client.as_u64(), 100);
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
