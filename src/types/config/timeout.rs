//! Timeout newtypes for type-safe timeout handling
//!
//! This module provides strongly-typed wrappers around `std::time::Duration`
//! to prevent accidentally using the wrong timeout value in the wrong context.

use std::time::Duration;

/// Timeout for reading responses from backend servers
///
/// This timeout applies to individual read operations from backend connections.
/// Per [RFC 3977](https://datatracker.ietf.org/doc/html/rfc3977), NNTP servers
/// should respond promptly, but large article transfers may take longer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BackendReadTimeout(Duration);

impl BackendReadTimeout {
    /// Default backend read timeout (30 seconds)
    pub const DEFAULT: Self = Self(Duration::from_secs(30));

    /// Create a new backend read timeout
    #[inline]
    pub const fn new(duration: Duration) -> Self {
        Self(duration)
    }

    /// Get the underlying duration
    #[inline]
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        self.0
    }

    /// Get timeout in seconds
    #[inline]
    #[must_use]
    pub const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }
}

impl From<Duration> for BackendReadTimeout {
    fn from(duration: Duration) -> Self {
        Self(duration)
    }
}

impl From<BackendReadTimeout> for Duration {
    fn from(timeout: BackendReadTimeout) -> Self {
        timeout.0
    }
}

/// Timeout for establishing connections to backend servers
///
/// This timeout applies when creating new TCP connections to backend servers.
/// Should be relatively short to fail fast on connection issues.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionTimeout(Duration);

impl ConnectionTimeout {
    /// Default connection timeout (10 seconds)
    pub const DEFAULT: Self = Self(Duration::from_secs(10));

    /// Create a new connection timeout
    #[inline]
    pub const fn new(duration: Duration) -> Self {
        Self(duration)
    }

    /// Get the underlying duration
    #[inline]
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        self.0
    }

    /// Get timeout in seconds
    #[inline]
    #[must_use]
    pub const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }
}

impl From<Duration> for ConnectionTimeout {
    fn from(duration: Duration) -> Self {
        Self(duration)
    }
}

impl From<ConnectionTimeout> for Duration {
    fn from(timeout: ConnectionTimeout) -> Self {
        timeout.0
    }
}

/// Timeout for executing commands on backend servers
///
/// This timeout covers the entire command execution cycle:
/// - Sending the command
/// - Waiting for response
/// - Receiving the complete response
///
/// Longer than read timeout to account for backend processing time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandExecutionTimeout(Duration);

impl CommandExecutionTimeout {
    /// Default command execution timeout (60 seconds)
    pub const DEFAULT: Self = Self(Duration::from_secs(60));

    /// Create a new command execution timeout
    #[inline]
    pub const fn new(duration: Duration) -> Self {
        Self(duration)
    }

    /// Get the underlying duration
    #[inline]
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        self.0
    }

    /// Get timeout in seconds
    #[inline]
    #[must_use]
    pub const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }
}

impl From<Duration> for CommandExecutionTimeout {
    fn from(duration: Duration) -> Self {
        Self(duration)
    }
}

impl From<CommandExecutionTimeout> for Duration {
    fn from(timeout: CommandExecutionTimeout) -> Self {
        timeout.0
    }
}

/// Timeout for health check operations
///
/// Used when validating backend connection health. Should be short to avoid
/// blocking the connection pool for too long.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HealthCheckTimeout(Duration);

impl HealthCheckTimeout {
    /// Default health check timeout (2 seconds)
    pub const DEFAULT: Self = Self(Duration::from_secs(2));

    /// Create a new health check timeout
    #[inline]
    pub const fn new(duration: Duration) -> Self {
        Self(duration)
    }

    /// Get the underlying duration
    #[inline]
    #[must_use]
    pub const fn as_duration(self) -> Duration {
        self.0
    }

    /// Get timeout in seconds
    #[inline]
    #[must_use]
    pub const fn as_secs(self) -> u64 {
        self.0.as_secs()
    }
}

impl From<Duration> for HealthCheckTimeout {
    fn from(duration: Duration) -> Self {
        Self(duration)
    }
}

impl From<HealthCheckTimeout> for Duration {
    fn from(timeout: HealthCheckTimeout) -> Self {
        timeout.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_read_timeout() {
        let timeout = BackendReadTimeout::new(Duration::from_secs(30));
        assert_eq!(timeout.as_secs(), 30);
        assert_eq!(timeout.as_duration(), Duration::from_secs(30));

        // Test From conversions
        let from_duration: BackendReadTimeout = Duration::from_secs(45).into();
        assert_eq!(from_duration.as_secs(), 45);

        let to_duration: Duration = timeout.into();
        assert_eq!(to_duration, Duration::from_secs(30));
    }

    #[test]
    fn test_connection_timeout() {
        let timeout = ConnectionTimeout::new(Duration::from_secs(10));
        assert_eq!(timeout.as_secs(), 10);
        assert_eq!(timeout.as_duration(), Duration::from_secs(10));
    }

    #[test]
    fn test_command_execution_timeout() {
        let timeout = CommandExecutionTimeout::new(Duration::from_secs(60));
        assert_eq!(timeout.as_secs(), 60);
        assert_eq!(timeout.as_duration(), Duration::from_secs(60));
    }

    #[test]
    fn test_health_check_timeout() {
        let timeout = HealthCheckTimeout::new(Duration::from_secs(2));
        assert_eq!(timeout.as_secs(), 2);
        assert_eq!(timeout.as_duration(), Duration::from_secs(2));
    }

    #[test]
    fn test_default_values() {
        assert_eq!(BackendReadTimeout::DEFAULT.as_secs(), 30);
        assert_eq!(ConnectionTimeout::DEFAULT.as_secs(), 10);
        assert_eq!(CommandExecutionTimeout::DEFAULT.as_secs(), 60);
        assert_eq!(HealthCheckTimeout::DEFAULT.as_secs(), 2);
    }

    #[test]
    fn test_ordering() {
        let short = HealthCheckTimeout::new(Duration::from_secs(1));
        let long = HealthCheckTimeout::new(Duration::from_secs(10));
        assert!(short < long);
        assert_eq!(short, short);
    }
}
