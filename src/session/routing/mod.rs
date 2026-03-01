//! Routing decisions and policies for session handling
//!
//! This module contains pure functions for making routing decisions without I/O.
//! All functions here are easily testable and have no side effects.

mod cache_policy;
mod decisions;
mod metrics_policy;

pub use cache_policy::{CacheAction, determine_cache_action};
pub use decisions::{CommandRoutingDecision, decide_command_routing};
pub use metrics_policy::{MetricsAction, determine_metrics_action};
