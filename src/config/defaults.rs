//! Default values for configuration fields
//!
//! This module centralizes all default value functions used in serde deserialization.

use crate::types::{CacheCapacity, MaxConnections, MaxErrors, Port, ThreadCount};
use std::time::Duration;

/// Default listen host (bind to all interfaces)
#[inline]
pub fn listen_host() -> String {
    "0.0.0.0".to_string()
}

/// Default listen port
#[inline]
pub fn listen_port() -> Port {
    Port::new(8119).expect("8119 is a valid port")
}

/// Default number of worker threads (1 for single-threaded)
#[inline]
pub fn threads() -> ThreadCount {
    ThreadCount::from_value(1).expect("1 is non-zero")
}

/// Default maximum connections per server
#[inline]
pub fn max_connections() -> MaxConnections {
    MaxConnections::new(10).expect("10 is non-zero")
}

/// Default health check interval
#[inline]
pub fn health_check_interval() -> Duration {
    Duration::from_secs(30)
}

/// Default health check timeout
#[inline]
pub fn health_check_timeout() -> Duration {
    Duration::from_secs(5)
}

/// Default unhealthy threshold
#[inline]
pub fn unhealthy_threshold() -> MaxErrors {
    MaxErrors::new(3).expect("3 is non-zero")
}

/// Default cache max capacity (number of articles)
#[inline]
pub fn cache_max_capacity() -> CacheCapacity {
    CacheCapacity::new(10000).expect("10000 is non-zero")
}

/// Default cache TTL (1 hour)
#[inline]
pub fn cache_ttl() -> Duration {
    Duration::from_secs(3600)
}

/// Default for TLS certificate verification (true for security)
#[inline]
pub fn tls_verify_cert() -> bool {
    true
}

/// Default maximum number of connections to check per health check cycle
#[inline]
pub fn health_check_max_per_cycle() -> usize {
    use crate::constants::pool::MAX_CONNECTIONS_PER_HEALTH_CHECK_CYCLE;
    MAX_CONNECTIONS_PER_HEALTH_CHECK_CYCLE
}

/// Default timeout when acquiring a connection for health checking
#[inline]
pub fn health_check_pool_timeout() -> Duration {
    use crate::constants::pool::HEALTH_CHECK_POOL_TIMEOUT_MS;
    Duration::from_millis(HEALTH_CHECK_POOL_TIMEOUT_MS)
}
