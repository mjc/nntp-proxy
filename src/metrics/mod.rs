//! Real-time metrics collection for the NNTP proxy
//!
//! This module provides lock-free, thread-safe metrics tracking using atomic operations.
//! Metrics are designed to be updated frequently from hot paths with minimal overhead.
//!
//! # Architecture
//!
//! - **collector**: Lock-free data accumulation using atomic operations (internal types here)
//! - **connection_stats**: Connection lifecycle tracking with aggregation
//! - **snapshot**: MetricsSnapshot with aggregation/query methods
//! - **backend_stats**: BackendStats with calculation methods
//! - **user_stats**: UserStats with helper methods
//! - **types**: Type-safe newtypes for all metric values (including BackendHealthStatus)
//!
//! ## Rate Calculations
//!
//! Snapshots contain cumulative counters only. The TUI calculates rates by:
//! 1. Getting snapshots at regular intervals (e.g., 250ms)
//! 2. Calculating: rate = (new_value - old_value) / time_delta
//! 3. This keeps the collector simple (just accumulation) and calculator logic in one place (TUI)

mod backend_stats;
mod collector;
mod connection_stats;
mod snapshot;
pub mod types;
mod user_stats;

pub use backend_stats::BackendStats;
pub use collector::MetricsCollector;
pub use connection_stats::ConnectionStatsAggregator;
pub use snapshot::MetricsSnapshot;
pub use types::*;
pub use user_stats::UserStats;
