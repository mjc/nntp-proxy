//! Proxy-specific feature tests (not part of RFC)
//!
//! This module contains tests for nntp-proxy features that go beyond the standard
//! NNTP protocol, including routing strategies, load balancing, caching integration,
//! and hybrid mode operations.

pub mod config;
pub mod hybrid_mode;
pub mod metrics;
pub mod routing;
