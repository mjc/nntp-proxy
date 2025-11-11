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
