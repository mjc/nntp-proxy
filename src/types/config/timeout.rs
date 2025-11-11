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
}
