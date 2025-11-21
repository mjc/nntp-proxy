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
    use proptest::prelude::*;

    // Property tests for all timeout types
    proptest! {
        #[test]
        fn backend_read_timeout_roundtrip(secs in 0u64..86400u64) {
            let duration = Duration::from_secs(secs);
            let timeout = BackendReadTimeout::new(duration);
            prop_assert_eq!(timeout.as_duration(), duration);
            prop_assert_eq!(timeout.as_secs(), secs);
        }

        #[test]
        fn connection_timeout_roundtrip(secs in 0u64..86400u64) {
            let duration = Duration::from_secs(secs);
            let timeout = ConnectionTimeout::new(duration);
            prop_assert_eq!(timeout.as_duration(), duration);
            prop_assert_eq!(timeout.as_secs(), secs);
        }

        #[test]
        fn command_execution_timeout_roundtrip(secs in 0u64..86400u64) {
            let duration = Duration::from_secs(secs);
            let timeout = CommandExecutionTimeout::new(duration);
            prop_assert_eq!(timeout.as_duration(), duration);
            prop_assert_eq!(timeout.as_secs(), secs);
        }

        #[test]
        fn health_check_timeout_roundtrip(secs in 0u64..86400u64) {
            let duration = Duration::from_secs(secs);
            let timeout = HealthCheckTimeout::new(duration);
            prop_assert_eq!(timeout.as_duration(), duration);
            prop_assert_eq!(timeout.as_secs(), secs);
        }

        #[test]
        fn timeout_from_into_duration(secs in 0u64..86400u64) {
            let duration = Duration::from_secs(secs);
            let timeout = BackendReadTimeout::from(duration);
            let back: Duration = timeout.into();
            prop_assert_eq!(back, duration);
        }

        #[test]
        fn timeout_ordering_property(a in 0u64..1000u64, b in 0u64..1000u64) {
            let t1 = ConnectionTimeout::new(Duration::from_secs(a));
            let t2 = ConnectionTimeout::new(Duration::from_secs(b));
            prop_assert_eq!(t1.cmp(&t2), a.cmp(&b));
        }

        #[test]
        fn timeout_clone_equality(secs in 0u64..86400u64) {
            let timeout = BackendReadTimeout::new(Duration::from_secs(secs));
            let cloned = timeout;
            prop_assert_eq!(timeout, cloned);
        }

        #[test]
        fn timeout_debug_contains_name(secs in 0u64..100u64) {
            let timeout = BackendReadTimeout::new(Duration::from_secs(secs));
            let debug_str = format!("{:?}", timeout);
            prop_assert!(debug_str.contains("BackendReadTimeout"));
        }
    }

    // Constant verification tests
    #[test]
    fn all_default_constants_correct() {
        assert_eq!(BackendReadTimeout::DEFAULT.as_secs(), 30);
        assert_eq!(ConnectionTimeout::DEFAULT.as_secs(), 10);
        assert_eq!(CommandExecutionTimeout::DEFAULT.as_secs(), 60);
        assert_eq!(HealthCheckTimeout::DEFAULT.as_secs(), 2);
    }

    // Edge case tests
    #[test]
    fn zero_timeout_is_valid() {
        let timeout = BackendReadTimeout::new(Duration::from_secs(0));
        assert_eq!(timeout.as_secs(), 0);
    }

    #[test]
    fn large_timeout_one_day() {
        let large = Duration::from_secs(86400);
        let timeout = CommandExecutionTimeout::new(large);
        assert_eq!(timeout.as_secs(), 86400);
    }

    #[test]
    fn timeout_hash_works() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(BackendReadTimeout::DEFAULT);
        assert!(set.contains(&BackendReadTimeout::DEFAULT));
    }
}
