//! Type-safe domain values for TUI

use std::fmt;
use std::num::NonZeroUsize;
use std::time::Instant;

/// Type-safe history size (non-zero)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HistorySize(NonZeroUsize);

impl HistorySize {
    /// Default history size: 60 points = 1 minute at 1 update/sec
    pub const DEFAULT: Self = Self(NonZeroUsize::new(60).unwrap());

    /// Create a new history size
    ///
    /// # Panics
    /// Panics if size is zero
    #[must_use]
    pub const fn new(size: usize) -> Self {
        match NonZeroUsize::new(size) {
            Some(non_zero) => Self(non_zero),
            None => panic!("HistorySize must be non-zero"),
        }
    }

    /// Get the raw value
    #[must_use]
    #[inline]
    pub const fn get(&self) -> usize {
        self.0.get()
    }
}

impl Default for HistorySize {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Type-safe throughput in bytes per second
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct BytesPerSecond(f64);

impl BytesPerSecond {
    /// Create from raw value
    #[must_use]
    #[inline]
    pub const fn new(bps: f64) -> Self {
        Self(bps)
    }

    /// Zero throughput
    #[must_use]
    #[inline]
    pub const fn zero() -> Self {
        Self(0.0)
    }

    /// Get raw value
    #[must_use]
    #[inline]
    pub const fn get(&self) -> f64 {
        self.0
    }

    /// Format as human-readable string
    #[must_use]
    pub fn format(&self) -> String {
        if self.0 > 1_000_000.0 {
            format!("{:.2} MB/s", self.0 / 1_000_000.0)
        } else if self.0 > 1_000.0 {
            format!("{:.2} KB/s", self.0 / 1_000.0)
        } else {
            format!("{:.0} B/s", self.0)
        }
    }
}

impl Default for BytesPerSecond {
    fn default() -> Self {
        Self::zero()
    }
}

impl fmt::Display for BytesPerSecond {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.format())
    }
}

impl From<f64> for BytesPerSecond {
    fn from(bps: f64) -> Self {
        Self::new(bps)
    }
}

/// Type-safe command rate in commands per second
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct CommandsPerSecond(f64);

impl CommandsPerSecond {
    /// Create from raw value
    #[must_use]
    #[inline]
    pub const fn new(cps: f64) -> Self {
        Self(cps)
    }

    /// Zero command rate
    #[must_use]
    #[inline]
    pub const fn zero() -> Self {
        Self(0.0)
    }

    /// Get raw value
    #[must_use]
    #[inline]
    pub const fn get(&self) -> f64 {
        self.0
    }
}

impl Default for CommandsPerSecond {
    fn default() -> Self {
        Self::zero()
    }
}

impl fmt::Display for CommandsPerSecond {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.1}", self.0)
    }
}

impl From<f64> for CommandsPerSecond {
    fn from(cps: f64) -> Self {
        Self::new(cps)
    }
}

/// Type-safe timestamp wrapper
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Timestamp(Instant);

impl Timestamp {
    /// Create from Instant
    #[must_use]
    #[inline]
    pub const fn new(instant: Instant) -> Self {
        Self(instant)
    }

    /// Current timestamp
    #[must_use]
    #[inline]
    pub fn now() -> Self {
        Self(Instant::now())
    }

    /// Get the inner Instant
    #[must_use]
    #[inline]
    pub const fn into_inner(self) -> Instant {
        self.0
    }

    /// Duration since this timestamp
    #[must_use]
    #[inline]
    pub fn elapsed(&self) -> std::time::Duration {
        self.0.elapsed()
    }

    /// Duration between two timestamps
    #[must_use]
    #[inline]
    pub fn duration_since(&self, earlier: Timestamp) -> std::time::Duration {
        self.0.duration_since(earlier.0)
    }
}

impl From<Instant> for Timestamp {
    fn from(instant: Instant) -> Self {
        Self::new(instant)
    }
}

impl From<Timestamp> for Instant {
    fn from(ts: Timestamp) -> Self {
        ts.into_inner()
    }
}

/// Type-safe connection count
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionCount(usize);

impl ConnectionCount {
    /// Create from raw count
    #[must_use]
    #[inline]
    pub const fn new(count: usize) -> Self {
        Self(count)
    }

    /// Zero connections
    #[must_use]
    #[inline]
    pub const fn zero() -> Self {
        Self(0)
    }

    /// Get raw count
    #[must_use]
    #[inline]
    pub const fn get(&self) -> usize {
        self.0
    }
}

impl Default for ConnectionCount {
    fn default() -> Self {
        Self::zero()
    }
}

impl fmt::Display for ConnectionCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<usize> for ConnectionCount {
    fn from(count: usize) -> Self {
        Self::new(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // HistorySize tests
    #[test]
    fn test_history_size() {
        let size = HistorySize::new(60);
        assert_eq!(size.get(), 60);
        assert_eq!(HistorySize::DEFAULT.get(), 60);
        assert_eq!(HistorySize::default().get(), 60);
    }

    #[test]
    #[should_panic(expected = "HistorySize must be non-zero")]
    fn test_history_size_zero_panics() {
        let _ = HistorySize::new(0);
    }

    #[test]
    fn test_history_size_custom_values() {
        assert_eq!(HistorySize::new(100).get(), 100);
        assert_eq!(HistorySize::new(1).get(), 1);
        assert_eq!(HistorySize::new(1000).get(), 1000);
    }

    // BytesPerSecond tests
    #[test]
    fn test_bytes_per_second() {
        let bps = BytesPerSecond::new(1_500_000.0);
        assert_eq!(bps.format(), "1.50 MB/s");

        let bps2 = BytesPerSecond::new(2_500.0);
        assert_eq!(bps2.format(), "2.50 KB/s");

        let bps3 = BytesPerSecond::new(500.0);
        assert_eq!(bps3.format(), "500 B/s");

        assert_eq!(BytesPerSecond::zero().get(), 0.0);
    }

    #[test]
    fn test_bytes_per_second_format_boundaries() {
        // Just over 1 MB/s
        assert_eq!(BytesPerSecond::new(1_000_001.0).format(), "1.00 MB/s");
        // Just under 1 MB/s
        assert_eq!(BytesPerSecond::new(999_999.0).format(), "1000.00 KB/s");
        // Just over 1 KB/s
        assert_eq!(BytesPerSecond::new(1_001.0).format(), "1.00 KB/s");
        // Just under 1 KB/s
        assert_eq!(BytesPerSecond::new(999.0).format(), "999 B/s");
        // Zero
        assert_eq!(BytesPerSecond::zero().format(), "0 B/s");
    }

    #[test]
    fn test_bytes_per_second_default() {
        let bps = BytesPerSecond::default();
        assert_eq!(bps.get(), 0.0);
    }

    #[test]
    fn test_bytes_per_second_display() {
        assert_eq!(BytesPerSecond::new(1_500_000.0).to_string(), "1.50 MB/s");
        assert_eq!(BytesPerSecond::new(2_500.0).to_string(), "2.50 KB/s");
        assert_eq!(BytesPerSecond::new(500.0).to_string(), "500 B/s");
    }

    #[test]
    fn test_bytes_per_second_from_f64() {
        let bps = BytesPerSecond::from(1234.5);
        assert_eq!(bps.get(), 1234.5);
    }

    // CommandsPerSecond tests
    #[test]
    fn test_commands_per_second() {
        let cps = CommandsPerSecond::new(123.456);
        assert_eq!(cps.to_string(), "123.5");

        assert_eq!(CommandsPerSecond::zero().get(), 0.0);
    }

    #[test]
    fn test_commands_per_second_default() {
        let cps = CommandsPerSecond::default();
        assert_eq!(cps.get(), 0.0);
    }

    #[test]
    fn test_commands_per_second_display() {
        assert_eq!(CommandsPerSecond::new(0.0).to_string(), "0.0");
        assert_eq!(CommandsPerSecond::new(1.5).to_string(), "1.5");
        assert_eq!(CommandsPerSecond::new(99.99).to_string(), "100.0");
    }

    #[test]
    fn test_commands_per_second_from_f64() {
        let cps = CommandsPerSecond::from(42.7);
        assert_eq!(cps.get(), 42.7);
    }

    // Timestamp tests
    #[test]
    fn test_timestamp() {
        let ts = Timestamp::now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(ts.elapsed() >= std::time::Duration::from_millis(10));
    }

    #[test]
    fn test_timestamp_duration_since() {
        let ts1 = Timestamp::now();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let ts2 = Timestamp::now();

        let duration = ts2.duration_since(ts1);
        assert!(duration >= std::time::Duration::from_millis(10));
    }

    #[test]
    fn test_timestamp_from_instant() {
        let instant = Instant::now();
        let ts = Timestamp::from(instant);
        assert_eq!(ts.into_inner(), instant);
    }

    #[test]
    fn test_timestamp_into_instant() {
        let instant = Instant::now();
        let ts = Timestamp::new(instant);
        let instant2: Instant = ts.into();
        assert_eq!(instant, instant2);
    }

    // ConnectionCount tests
    #[test]
    fn test_connection_count() {
        let count = ConnectionCount::new(42);
        assert_eq!(count.to_string(), "42");
        assert_eq!(ConnectionCount::zero().get(), 0);
    }

    #[test]
    fn test_connection_count_default() {
        let count = ConnectionCount::default();
        assert_eq!(count.get(), 0);
    }

    #[test]
    fn test_connection_count_display() {
        assert_eq!(ConnectionCount::new(0).to_string(), "0");
        assert_eq!(ConnectionCount::new(1).to_string(), "1");
        assert_eq!(ConnectionCount::new(1000).to_string(), "1000");
    }

    #[test]
    fn test_connection_count_from_usize() {
        let count = ConnectionCount::from(123);
        assert_eq!(count.get(), 123);
    }
}
