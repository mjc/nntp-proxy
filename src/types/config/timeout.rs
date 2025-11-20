//! Timeout newtypes for type-safe timeout handling
//!
//! This module provides strongly-typed wrappers around `std::time::Duration`
//! to prevent accidentally using the wrong timeout value in the wrong context.

use std::time::Duration;

macro_rules! timeout_newtype {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident($default_secs:expr);
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        $vis struct $name(Duration);

        impl $name {
            /// Default timeout value
            pub const DEFAULT: Self = Self(Duration::from_secs($default_secs));

            /// Create a new timeout
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

        impl From<Duration> for $name {
            fn from(duration: Duration) -> Self {
                Self(duration)
            }
        }

        impl From<$name> for Duration {
            fn from(timeout: $name) -> Self {
                timeout.0
            }
        }
    };
}

timeout_newtype! {
    /// Timeout for reading responses from backend servers
    ///
    /// This timeout applies to individual read operations from backend connections.
    /// Per [RFC 3977](https://datatracker.ietf.org/doc/html/rfc3977), NNTP servers
    /// should respond promptly, but large article transfers may take longer.
    pub struct BackendReadTimeout(30);
}

timeout_newtype! {
    /// Timeout for establishing connections to backend servers
    ///
    /// This timeout applies when creating new TCP connections to backend servers.
    /// Should be relatively short to fail fast on connection issues.
    pub struct ConnectionTimeout(10);
}

timeout_newtype! {
    /// Timeout for executing individual NNTP commands
    ///
    /// This timeout applies to the entire request/response cycle for a single command.
    /// Includes both sending the command and receiving the complete response.
    pub struct CommandExecutionTimeout(60);
}

timeout_newtype! {
    /// Timeout for health check operations
    ///
    /// Health checks should complete quickly to avoid blocking pool operations.
    /// A short timeout ensures unhealthy backends are detected promptly.
    pub struct HealthCheckTimeout(2);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backend_read_timeout() {
        let timeout = BackendReadTimeout::DEFAULT;
        assert_eq!(timeout.as_secs(), 30);
        assert_eq!(timeout.as_duration(), Duration::from_secs(30));
    }

    #[test]
    fn test_connection_timeout() {
        let timeout = ConnectionTimeout::new(Duration::from_secs(5));
        assert_eq!(timeout.as_secs(), 5);
    }

    #[test]
    fn test_backend_read_timeout_default() {
        let timeout = BackendReadTimeout::DEFAULT;
        assert_eq!(timeout.as_secs(), 30);
    }

    #[test]
    fn test_backend_read_timeout_new() {
        let timeout = BackendReadTimeout::new(Duration::from_secs(45));
        assert_eq!(timeout.as_secs(), 45);
        assert_eq!(timeout.as_duration(), Duration::from_secs(45));
    }

    #[test]
    fn test_backend_read_timeout_from_duration() {
        let duration = Duration::from_secs(60);
        let timeout = BackendReadTimeout::from(duration);
        assert_eq!(timeout.as_secs(), 60);
    }

    #[test]
    fn test_backend_read_timeout_into_duration() {
        let timeout = BackendReadTimeout::new(Duration::from_secs(20));
        let duration: Duration = timeout.into();
        assert_eq!(duration, Duration::from_secs(20));
    }

    #[test]
    fn test_connection_timeout_default() {
        let timeout = ConnectionTimeout::DEFAULT;
        assert_eq!(timeout.as_secs(), 10);
    }

    #[test]
    fn test_connection_timeout_new() {
        let timeout = ConnectionTimeout::new(Duration::from_secs(15));
        assert_eq!(timeout.as_secs(), 15);
    }

    #[test]
    fn test_connection_timeout_from_duration() {
        let duration = Duration::from_secs(8);
        let timeout = ConnectionTimeout::from(duration);
        assert_eq!(timeout.as_secs(), 8);
    }

    #[test]
    fn test_connection_timeout_into_duration() {
        let timeout = ConnectionTimeout::new(Duration::from_secs(12));
        let duration: Duration = timeout.into();
        assert_eq!(duration, Duration::from_secs(12));
    }

    #[test]
    fn test_command_execution_timeout_default() {
        let timeout = CommandExecutionTimeout::DEFAULT;
        assert_eq!(timeout.as_secs(), 60);
    }

    #[test]
    fn test_command_execution_timeout_new() {
        let timeout = CommandExecutionTimeout::new(Duration::from_secs(90));
        assert_eq!(timeout.as_secs(), 90);
        assert_eq!(timeout.as_duration(), Duration::from_secs(90));
    }

    #[test]
    fn test_command_execution_timeout_conversions() {
        let duration = Duration::from_secs(120);
        let timeout = CommandExecutionTimeout::from(duration);
        let back: Duration = timeout.into();
        assert_eq!(back, duration);
    }

    #[test]
    fn test_health_check_timeout_default() {
        let timeout = HealthCheckTimeout::DEFAULT;
        assert_eq!(timeout.as_secs(), 2);
    }

    #[test]
    fn test_health_check_timeout_new() {
        let timeout = HealthCheckTimeout::new(Duration::from_secs(3));
        assert_eq!(timeout.as_secs(), 3);
    }

    #[test]
    fn test_health_check_timeout_conversions() {
        let duration = Duration::from_secs(5);
        let timeout = HealthCheckTimeout::from(duration);
        assert_eq!(timeout.as_duration(), duration);
    }

    #[test]
    fn test_timeout_ordering() {
        let t1 = ConnectionTimeout::new(Duration::from_secs(5));
        let t2 = ConnectionTimeout::new(Duration::from_secs(10));
        assert!(t1 < t2);
        assert!(t2 > t1);
        assert_eq!(t1, t1);
    }

    #[test]
    fn test_timeout_hash() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(BackendReadTimeout::DEFAULT);
        assert!(set.contains(&BackendReadTimeout::DEFAULT));
    }

    #[test]
    fn test_timeout_clone() {
        let timeout = ConnectionTimeout::DEFAULT;
        let cloned = timeout;
        assert_eq!(timeout, cloned);
    }

    #[test]
    fn test_timeout_debug() {
        let timeout = BackendReadTimeout::DEFAULT;
        let debug_str = format!("{:?}", timeout);
        assert!(debug_str.contains("BackendReadTimeout"));
    }

    #[test]
    fn test_all_default_constants() {
        // Verify all timeout types have valid defaults
        assert_eq!(BackendReadTimeout::DEFAULT.as_secs(), 30);
        assert_eq!(ConnectionTimeout::DEFAULT.as_secs(), 10);
        assert_eq!(CommandExecutionTimeout::DEFAULT.as_secs(), 60);
        assert_eq!(HealthCheckTimeout::DEFAULT.as_secs(), 2);
    }

    #[test]
    fn test_zero_timeout() {
        let timeout = BackendReadTimeout::new(Duration::from_secs(0));
        assert_eq!(timeout.as_secs(), 0);
    }

    #[test]
    fn test_large_timeout() {
        let large = Duration::from_secs(86400); // 1 day
        let timeout = CommandExecutionTimeout::new(large);
        assert_eq!(timeout.as_secs(), 86400);
    }
}
