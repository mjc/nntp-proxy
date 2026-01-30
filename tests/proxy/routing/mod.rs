//! Router and routing strategy tests
//!
//! This module contains tests for backend routing strategies, load balancing,
//! round-robin, weighted routing, tiered routing, and stateful routing modes.

pub mod backend_availability;
pub mod basic;
pub mod duplicate_greeting;
pub mod edge_cases;
pub mod least_loaded;
pub mod load_tracking;
pub mod modes;
pub mod retry_430;
pub mod round_robin;
pub mod router;
pub mod stale_connection;
pub mod stateful;
pub mod tiered;
pub mod weighted;
