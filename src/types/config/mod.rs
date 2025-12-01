//! Configuration-related type-safe wrappers using NonZero types
//!
//! This module provides validated configuration types that enforce
//! invariants at the type level using Rust's NonZero types.

#[macro_use]
mod buffer;
mod cache;
pub mod duration;
mod limits;
mod network;
mod timeout;

// Re-export all public types
pub use buffer::{BufferSize, WindowSize};
pub use cache::CacheCapacity;
pub use duration::{duration_serde, option_duration_serde};
pub use limits::{MaxConnections, MaxErrors, ThreadCount};
pub use network::Port;
pub use timeout::{
    BackendReadTimeout, CommandExecutionTimeout, ConnectionTimeout, HealthCheckTimeout,
};

#[cfg(test)]
mod tests {
    use super::*;

    // Test that all exported types exist and are usable
    #[test]
    fn test_buffer_types_available() {
        let _ = BufferSize::try_new(1024);
        let _ = WindowSize::try_new(8192);
    }

    #[test]
    fn test_cache_types_available() {
        let _ = CacheCapacity::try_new(1000);
    }

    #[test]
    fn test_limit_types_available() {
        let _ = MaxConnections::try_new(10);
        let _ = MaxErrors::try_new(3);
        let _ = ThreadCount::from_value(4);
    }

    #[test]
    fn test_network_types_available() {
        let _ = Port::try_new(119);
    }

    #[test]
    fn test_timeout_types_available() {
        use std::time::Duration;
        let _ = ConnectionTimeout::new(Duration::from_secs(30));
        let _ = BackendReadTimeout::new(Duration::from_secs(60));
        let _ = CommandExecutionTimeout::new(Duration::from_secs(120));
        let _ = HealthCheckTimeout::new(Duration::from_secs(10));
    }

    // Test macro-generated implementations
    #[test]
    fn test_nonzero_newtype_basic_usage() {
        // Test via BufferSize (uses the macro)
        let size = BufferSize::try_new(4096).unwrap();
        assert_eq!(size.get(), 4096);

        // Test Display
        assert_eq!(format!("{}", size), "4096");

        // Test From (nutype uses into_inner, not From)
        let val: usize = size.into_inner();
        assert_eq!(val, 4096);
    }

    #[test]
    fn test_nonzero_newtype_zero_rejected() {
        assert!(BufferSize::try_new(0).is_err());
        assert!(WindowSize::try_new(0).is_err());
        assert!(CacheCapacity::try_new(0).is_err());
        assert!(MaxConnections::try_new(0).is_err());
        assert!(MaxErrors::try_new(0).is_err());
    }

    #[test]
    fn test_nonzero_newtype_serde() {
        let size = BufferSize::try_new(8192).unwrap();

        // Serialize
        let json = serde_json::to_string(&size).unwrap();
        assert_eq!(json, "8192");

        // Deserialize
        let parsed: BufferSize = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.get(), 8192);

        // Zero rejected during deserialization
        let result = serde_json::from_str::<BufferSize>("0");
        assert!(result.is_err());
        // nutype validation error - just check it failed, don't check specific message
    }

    #[test]
    fn test_nonzero_newtype_clone_copy() {
        let size1 = BufferSize::try_new(1024).unwrap();
        let size2 = size1;
        let size3 = size1; // Copy

        assert_eq!(size1, size2);
        assert_eq!(size1, size3);
    }

    #[test]
    fn test_nonzero_newtype_equality() {
        let size1 = BufferSize::try_new(2048).unwrap();
        let size2 = BufferSize::try_new(2048).unwrap();
        let size3 = BufferSize::try_new(4096).unwrap();

        assert_eq!(size1, size2);
        assert_ne!(size1, size3);
    }

    #[test]
    fn test_nonzero_newtype_hash() {
        use std::collections::HashSet;

        let size1 = BufferSize::try_new(512).unwrap();
        let size2 = BufferSize::try_new(512).unwrap();
        let size3 = BufferSize::try_new(1024).unwrap();

        let mut set = HashSet::new();
        set.insert(size1);
        set.insert(size2); // Duplicate
        set.insert(size3);

        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_nonzero_newtype_debug() {
        let size = BufferSize::try_new(16384).unwrap();
        let debug = format!("{:?}", size);
        assert!(debug.contains("BufferSize"));
    }
}
