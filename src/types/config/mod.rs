//! Configuration-related type-safe wrappers using NonZero types
//!
//! This module provides validated configuration types that enforce
//! invariants at the type level using Rust's NonZero types.

/// Macro to generate NonZero newtype wrappers with standard implementations
///
/// Eliminates boilerplate for NonZero-wrapped types.
/// Each type gets: new(), get(), Display, From, Serialize, Deserialize
macro_rules! nonzero_newtype {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident($nonzero:ty : $primitive:ty, serialize as $ser_fn:ident);
    ) => {
        $(#[$meta])*
        #[repr(transparent)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis struct $name($nonzero);

        impl $name {
            /// Create a new instance, returning None if value is 0
            #[must_use]
            pub const fn new(value: $primitive) -> Option<Self> {
                match <$nonzero>::new(value) {
                    Some(nz) => Some(Self(nz)),
                    None => None,
                }
            }

            /// Get the inner value
            #[must_use]
            #[inline]
            pub const fn get(&self) -> $primitive {
                self.0.get()
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.get())
            }
        }

        impl From<$name> for $primitive {
            fn from(val: $name) -> Self {
                val.get()
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.$ser_fn(self.get() as _)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let value = <$primitive>::deserialize(deserializer)?;
                Self::new(value).ok_or_else(|| {
                    serde::de::Error::custom(concat!(stringify!($name), " cannot be 0"))
                })
            }
        }
    };
}

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
        let _ = BufferSize::new(1024);
        let _ = WindowSize::new(8192);
    }

    #[test]
    fn test_cache_types_available() {
        let _ = CacheCapacity::new(1000);
    }

    #[test]
    fn test_limit_types_available() {
        let _ = MaxConnections::new(10);
        let _ = MaxErrors::new(3);
        let _ = ThreadCount::from_value(4);
    }

    #[test]
    fn test_network_types_available() {
        let _ = Port::new(119);
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
        let size = BufferSize::new(4096).unwrap();
        assert_eq!(size.get(), 4096);

        // Test Display
        assert_eq!(format!("{}", size), "4096");

        // Test From
        let val: usize = size.into();
        assert_eq!(val, 4096);
    }

    #[test]
    fn test_nonzero_newtype_zero_rejected() {
        assert!(BufferSize::new(0).is_none());
        assert!(WindowSize::new(0).is_none());
        assert!(CacheCapacity::new(0).is_none());
        assert!(MaxConnections::new(0).is_none());
        assert!(MaxErrors::new(0).is_none());
    }

    #[test]
    fn test_nonzero_newtype_serde() {
        let size = BufferSize::new(8192).unwrap();

        // Serialize
        let json = serde_json::to_string(&size).unwrap();
        assert_eq!(json, "8192");

        // Deserialize
        let parsed: BufferSize = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.get(), 8192);

        // Zero rejected during deserialization
        let result = serde_json::from_str::<BufferSize>("0");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("cannot be 0"));
    }

    #[test]
    fn test_nonzero_newtype_clone_copy() {
        let size1 = BufferSize::new(1024).unwrap();
        let size2 = size1;
        let size3 = size1; // Copy

        assert_eq!(size1, size2);
        assert_eq!(size1, size3);
    }

    #[test]
    fn test_nonzero_newtype_equality() {
        let size1 = BufferSize::new(2048).unwrap();
        let size2 = BufferSize::new(2048).unwrap();
        let size3 = BufferSize::new(4096).unwrap();

        assert_eq!(size1, size2);
        assert_ne!(size1, size3);
    }

    #[test]
    fn test_nonzero_newtype_hash() {
        use std::collections::HashSet;

        let size1 = BufferSize::new(512).unwrap();
        let size2 = BufferSize::new(512).unwrap();
        let size3 = BufferSize::new(1024).unwrap();

        let mut set = HashSet::new();
        set.insert(size1);
        set.insert(size2); // Duplicate
        set.insert(size3);

        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_nonzero_newtype_debug() {
        let size = BufferSize::new(16384).unwrap();
        let debug = format!("{:?}", size);
        assert!(debug.contains("BufferSize"));
    }
}
