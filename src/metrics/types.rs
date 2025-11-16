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
