//! Type-safe metrics types using the newtype pattern
//!
//! All metric values are wrapped in newtypes to prevent:
//! - Mixing up different kinds of counts (commands vs errors vs articles)
//! - Mixing up different time units (microseconds vs milliseconds)
//! - Mixing up different rate types (bytes/sec vs commands/sec)
//!
//! This provides compile-time guarantees that we're not accidentally
//! adding apples to oranges.

use std::num::NonZeroU64;

// ============================================================================
// Macros to reduce boilerplate
// ============================================================================

/// Define a simple u64-based counter newtype with standard operations
macro_rules! counter_type {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
        pub struct $name(u64);

        impl $name {
            #[inline]
            pub const fn new(value: u64) -> Self {
                Self(value)
            }

            #[inline]
            pub const fn get(self) -> u64 {
                self.0
            }

            #[inline]
            pub fn increment(&mut self) {
                self.0 += 1;
            }

            #[inline]
            pub const fn saturating_sub(self, other: Self) -> Self {
                Self(self.0.saturating_sub(other.0))
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

/// Define a microseconds-based timing newtype that can average to milliseconds
macro_rules! timing_type {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
        pub struct $name(u64);

        impl $name {
            #[inline]
            pub const fn new(micros: u64) -> Self {
                Self(micros)
            }

            #[inline]
            pub const fn get(self) -> u64 {
                self.0
            }

            #[inline]
            pub fn add(&mut self, other: Self) {
                self.0 += other.0;
            }

            #[must_use]
            pub fn average(total: Self, count: NonZeroU64) -> Milliseconds {
                let avg_micros = total.0 as f64 / count.get() as f64;
                Milliseconds::from_micros(avg_micros)
            }
        }
    };
}

/// Define a f64-based rate/measurement newtype
macro_rules! f64_type {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
        pub struct $name(f64);

        impl $name {
            #[inline]
            pub const fn new(value: f64) -> Self {
                Self(value)
            }

            #[inline]
            pub const fn get(self) -> f64 {
                self.0
            }
        }
    };
}

// ============================================================================
// Health Status
// ============================================================================

/// Backend health status
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// Backend is healthy and responding normally
    Healthy,
    /// Backend is degraded (high error rate or slow)
    Degraded,
    /// Backend is down or unreachable
    Down,
}

impl From<u8> for HealthStatus {
    fn from(value: u8) -> Self {
        match value {
            0 => Self::Healthy,
            1 => Self::Degraded,
            2 => Self::Down,
            _ => Self::Healthy,
        }
    }
}

impl From<HealthStatus> for u8 {
    fn from(status: HealthStatus) -> Self {
        match status {
            HealthStatus::Healthy => 0,
            HealthStatus::Degraded => 1,
            HealthStatus::Down => 2,
        }
    }
}

// ============================================================================
// Counts - Different types of things we count
// ============================================================================

counter_type!(CommandCount);
counter_type!(FailureCount);

/// Number of errors encountered
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ErrorCount(u64);

impl ErrorCount {
    #[inline]
    pub const fn new(count: u64) -> Self {
        Self(count)
    }

    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[inline]
    pub fn increment(&mut self) {
        self.0 += 1;
    }

    #[inline]
    pub fn add(&mut self, other: ErrorCount) {
        self.0 += other.0;
    }

    #[inline]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    #[must_use]
    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl std::fmt::Display for ErrorCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Number of articles retrieved
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ArticleCount(u64);

impl ArticleCount {
    #[inline]
    pub const fn new(count: u64) -> Self {
        Self(count)
    }

    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[inline]
    pub fn increment(&mut self) {
        self.0 += 1;
    }

    /// Calculate average bytes per article
    #[must_use]
    pub fn average_bytes(self, total_bytes: u64) -> Option<u64> {
        if self.0 > 0 {
            Some(total_bytes / self.0)
        } else {
            None
        }
    }
}

impl std::fmt::Display for ArticleCount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Number of active connections (non-zero validated)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ActiveConnections(usize);

impl ActiveConnections {
    #[inline]
    pub const fn new(count: usize) -> Self {
        Self(count)
    }

    #[inline]
    pub const fn get(self) -> usize {
        self.0
    }
}

impl std::fmt::Display for ActiveConnections {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ============================================================================
// Time measurements - Different units and types of timing
// ============================================================================

timing_type!(TtfbMicros);
timing_type!(SendMicros);
timing_type!(RecvMicros);

/// Time in microseconds (for precision timing)
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Microseconds(u64);

impl Microseconds {
    #[inline]
    pub const fn new(micros: u64) -> Self {
        Self(micros)
    }

    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[inline]
    pub fn add(&mut self, other: Microseconds) {
        self.0 += other.0;
    }

    #[inline]
    #[must_use]
    pub fn as_millis_f64(self) -> f64 {
        self.0 as f64 / 1000.0
    }
}

f64_type!(Milliseconds);

impl Milliseconds {
    #[inline]
    pub fn from_micros(micros: f64) -> Self {
        Self(micros / 1000.0)
    }
}

f64_type!(OverheadMillis);

impl OverheadMillis {
    /// Calculate overhead from component times
    #[must_use]
    pub fn from_components(ttfb: Milliseconds, send: Milliseconds, recv: Milliseconds) -> Self {
        Self(ttfb.0 - send.0 - recv.0)
    }
}

// ============================================================================
// Rates - Different types of throughput measurements
// ============================================================================

/// Bytes per second transfer rate
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default)]
pub struct BytesPerSecond(u64);

impl BytesPerSecond {
    #[inline]
    pub const fn new(bps: u64) -> Self {
        Self(bps)
    }

    #[inline]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub fn from_delta(bytes_delta: u64, seconds: f64) -> Self {
        if seconds > 0.0 {
            Self((bytes_delta as f64 / seconds) as u64)
        } else {
            Self(0)
        }
    }
}

f64_type!(CommandsPerSecond);

impl CommandsPerSecond {
    #[must_use]
    pub fn from_delta(commands_delta: u64, seconds: f64) -> Self {
        if seconds > 0.0 {
            Self(commands_delta as f64 / seconds)
        } else {
            Self(0.0)
        }
    }
}

f64_type!(ErrorRatePercent);

impl ErrorRatePercent {
    #[must_use]
    pub fn from_counts(errors: ErrorCount, commands: CommandCount) -> Self {
        if commands.get() > 0 {
            Self((errors.get() as f64 / commands.get() as f64) * 100.0)
        } else {
            Self(0.0)
        }
    }

    #[must_use]
    pub fn from_raw_counts(errors: u64, commands: u64) -> Self {
        if commands > 0 {
            Self((errors as f64 / commands as f64) * 100.0)
        } else {
            Self(0.0)
        }
    }

    #[must_use]
    pub fn is_high(self) -> bool {
        self.0 > 5.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // CommandCount tests
    #[test]
    fn test_command_count_new() {
        let count = CommandCount::new(42);
        assert_eq!(count.get(), 42);
    }

    #[test]
    fn test_command_count_default() {
        let count = CommandCount::default();
        assert_eq!(count.get(), 0);
    }

    #[test]
    fn test_command_count_increment() {
        let mut count = CommandCount::new(10);
        count.increment();
        assert_eq!(count.get(), 11);
    }

    #[test]
    fn test_command_count_saturating_sub() {
        let count1 = CommandCount::new(100);
        let count2 = CommandCount::new(30);
        let result = count1.saturating_sub(count2);
        assert_eq!(result.get(), 70);
    }

    #[test]
    fn test_command_count_saturating_sub_underflow() {
        let count1 = CommandCount::new(10);
        let count2 = CommandCount::new(30);
        let result = count1.saturating_sub(count2);
        assert_eq!(result.get(), 0); // Saturates at 0
    }

    #[test]
    fn test_command_count_display() {
        let count = CommandCount::new(1234);
        assert_eq!(format!("{}", count), "1234");
    }

    // ErrorCount tests
    #[test]
    fn test_error_count_new() {
        let count = ErrorCount::new(5);
        assert_eq!(count.get(), 5);
    }

    #[test]
    fn test_error_count_increment() {
        let mut count = ErrorCount::new(0);
        count.increment();
        count.increment();
        assert_eq!(count.get(), 2);
    }

    #[test]
    fn test_error_count_is_zero() {
        let zero = ErrorCount::new(0);
        let nonzero = ErrorCount::new(1);

        assert!(zero.is_zero());
        assert!(!nonzero.is_zero());
    }

    // ArticleCount tests
    #[test]
    fn test_article_count_new() {
        let count = ArticleCount::new(10);
        assert_eq!(count.get(), 10);
    }

    #[test]
    fn test_article_count_increment() {
        let mut count = ArticleCount::new(5);
        count.increment();
        assert_eq!(count.get(), 6);
    }

    #[test]
    fn test_article_count_average_bytes() {
        let count = ArticleCount::new(10);
        let avg = count.average_bytes(5000);
        assert_eq!(avg, Some(500)); // 5000 / 10 = 500
    }

    #[test]
    fn test_article_count_average_bytes_zero_articles() {
        let count = ArticleCount::new(0);
        let avg = count.average_bytes(1000);
        assert_eq!(avg, None);
    }

    #[test]
    fn test_article_count_average_bytes_zero_bytes() {
        let count = ArticleCount::new(10);
        let avg = count.average_bytes(0);
        assert_eq!(avg, Some(0));
    }

    // ActiveConnections tests
    #[test]
    fn test_active_connections_new() {
        let active = ActiveConnections::new(5);
        assert_eq!(active.get(), 5);
    }

    #[test]
    fn test_active_connections_default() {
        let active = ActiveConnections::default();
        assert_eq!(active.get(), 0);
    }

    #[test]
    fn test_active_connections_display() {
        let active = ActiveConnections::new(42);
        assert_eq!(format!("{}", active), "42");
    }

    // Timing types tests
    #[test]
    fn test_ttfb_micros_new() {
        let ttfb = TtfbMicros::new(1000);
        assert_eq!(ttfb.get(), 1000);
    }

    #[test]
    fn test_ttfb_micros_add() {
        let mut ttfb = TtfbMicros::new(1000);
        ttfb.add(TtfbMicros::new(500));
        assert_eq!(ttfb.get(), 1500);
    }

    #[test]
    fn test_ttfb_micros_average() {
        let total = TtfbMicros::new(10000); // 10000 micros
        let count = NonZeroU64::new(10).unwrap();
        let avg = TtfbMicros::average(total, count);
        assert!((avg.get() - 1.0).abs() < 0.01); // 1000 micros = 1.0 ms
    }

    #[test]
    fn test_send_micros_average() {
        let total = SendMicros::new(5000);
        let count = NonZeroU64::new(10).unwrap();
        let avg = SendMicros::average(total, count);
        assert!((avg.get() - 0.5).abs() < 0.01); // 500 micros = 0.5 ms
    }

    #[test]
    fn test_recv_micros_average() {
        let total = RecvMicros::new(15000);
        let count = NonZeroU64::new(10).unwrap();
        let avg = RecvMicros::average(total, count);
        assert!((avg.get() - 1.5).abs() < 0.01); // 1500 micros = 1.5 ms
    }

    // Microseconds tests
    #[test]
    fn test_microseconds_new() {
        let micros = Microseconds::new(1000);
        assert_eq!(micros.get(), 1000);
    }

    #[test]
    fn test_microseconds_add() {
        let mut micros = Microseconds::new(1000);
        micros.add(Microseconds::new(500));
        assert_eq!(micros.get(), 1500);
    }

    #[test]
    fn test_microseconds_as_millis_f64() {
        let micros = Microseconds::new(1500);
        let millis = micros.as_millis_f64();
        assert!((millis - 1.5).abs() < 0.01);
    }

    // Milliseconds tests
    #[test]
    fn test_milliseconds_new() {
        let ms = Milliseconds::new(10.5);
        assert!((ms.get() - 10.5).abs() < 0.01);
    }

    #[test]
    fn test_milliseconds_from_micros() {
        let ms = Milliseconds::from_micros(5000.0);
        assert!((ms.get() - 5.0).abs() < 0.01);
    }

    // OverheadMillis tests
    #[test]
    fn test_overhead_millis_from_components() {
        let ttfb = Milliseconds::new(10.0);
        let send = Milliseconds::new(3.0);
        let recv = Milliseconds::new(5.0);

        let overhead = OverheadMillis::from_components(ttfb, send, recv);
        assert!((overhead.get() - 2.0).abs() < 0.01); // 10 - 3 - 5 = 2
    }

    #[test]
    fn test_overhead_millis_negative() {
        // Edge case: send + recv > ttfb (shouldn't happen in practice)
        let ttfb = Milliseconds::new(5.0);
        let send = Milliseconds::new(3.0);
        let recv = Milliseconds::new(4.0);

        let overhead = OverheadMillis::from_components(ttfb, send, recv);
        assert!((overhead.get() + 2.0).abs() < 0.01); // 5 - 3 - 4 = -2
    }

    // BytesPerSecond tests
    #[test]
    fn test_bytes_per_second_new() {
        let bps = BytesPerSecond::new(1000);
        assert_eq!(bps.get(), 1000);
    }

    #[test]
    fn test_bytes_per_second_from_delta() {
        let bps = BytesPerSecond::from_delta(1000, 2.0);
        assert_eq!(bps.get(), 500); // 1000 bytes / 2 seconds = 500 bps
    }

    #[test]
    fn test_bytes_per_second_from_delta_zero_time() {
        let bps = BytesPerSecond::from_delta(1000, 0.0);
        assert_eq!(bps.get(), 0); // Avoid division by zero
    }

    #[test]
    fn test_bytes_per_second_default() {
        let bps = BytesPerSecond::default();
        assert_eq!(bps.get(), 0);
    }

    // CommandsPerSecond tests
    #[test]
    fn test_commands_per_second_new() {
        let cps = CommandsPerSecond::new(10.5);
        assert!((cps.get() - 10.5).abs() < 0.01);
    }

    #[test]
    fn test_commands_per_second_from_delta() {
        let cps = CommandsPerSecond::from_delta(100, 10.0);
        assert!((cps.get() - 10.0).abs() < 0.01); // 100 / 10 = 10.0
    }

    #[test]
    fn test_commands_per_second_from_delta_zero_time() {
        let cps = CommandsPerSecond::from_delta(100, 0.0);
        assert_eq!(cps.get(), 0.0);
    }

    // ErrorRatePercent tests
    #[test]
    fn test_error_rate_percent_from_counts() {
        let errors = ErrorCount::new(5);
        let commands = CommandCount::new(100);
        let rate = ErrorRatePercent::from_counts(errors, commands);
        assert!((rate.get() - 5.0).abs() < 0.01); // 5/100 = 5%
    }

    #[test]
    fn test_error_rate_percent_from_counts_zero_commands() {
        let errors = ErrorCount::new(10);
        let commands = CommandCount::new(0);
        let rate = ErrorRatePercent::from_counts(errors, commands);
        assert_eq!(rate.get(), 0.0); // Avoid division by zero
    }

    #[test]
    fn test_error_rate_percent_from_raw_counts() {
        let rate = ErrorRatePercent::from_raw_counts(10, 100);
        assert!((rate.get() - 10.0).abs() < 0.01); // 10/100 = 10%
    }

    #[test]
    fn test_error_rate_percent_from_raw_counts_zero_commands() {
        let rate = ErrorRatePercent::from_raw_counts(5, 0);
        assert_eq!(rate.get(), 0.0);
    }

    #[test]
    fn test_error_rate_percent_is_high() {
        let low = ErrorRatePercent::new(3.0);
        let threshold = ErrorRatePercent::new(5.0);
        let high = ErrorRatePercent::new(10.0);

        assert!(!low.is_high());
        assert!(!threshold.is_high()); // 5.0 is NOT high (> 5.0)
        assert!(high.is_high());
    }

    #[test]
    fn test_error_rate_percent_is_high_edge_cases() {
        let just_above = ErrorRatePercent::new(5.01);
        let just_below = ErrorRatePercent::new(4.99);

        assert!(just_above.is_high());
        assert!(!just_below.is_high());
    }

    // Ordering tests
    #[test]
    fn test_command_count_ordering() {
        let c1 = CommandCount::new(10);
        let c2 = CommandCount::new(20);
        let c3 = CommandCount::new(10);

        assert!(c1 < c2);
        assert!(c2 > c1);
        assert_eq!(c1, c3);
    }

    #[test]
    fn test_error_count_ordering() {
        let e1 = ErrorCount::new(5);
        let e2 = ErrorCount::new(10);

        assert!(e1 < e2);
        assert!(e2 > e1);
    }

    #[test]
    fn test_article_count_ordering() {
        let a1 = ArticleCount::new(100);
        let a2 = ArticleCount::new(200);

        assert!(a1 < a2);
        assert!(a2 > a1);
    }

    #[test]
    fn test_active_connections_ordering() {
        let a1 = ActiveConnections::new(3);
        let a2 = ActiveConnections::new(5);

        assert!(a1 < a2);
        assert!(a2 > a1);
    }

    // Clone and Copy tests
    #[test]
    fn test_types_are_copy() {
        let count = CommandCount::new(42);
        let copied = count; // Copy, not move
        assert_eq!(count.get(), copied.get());
    }
}
