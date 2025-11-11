//! Configuration-related type-safe wrappers using NonZero types
//!
//! This module provides validated configuration types that enforce
//! invariants at the type level using Rust's NonZero types.

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
