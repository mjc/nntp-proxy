//! Default values for configuration fields
//!
//! This module centralizes all default value functions used in serde deserialization.

/// Default maximum connections per server
pub fn max_connections() -> u32 {
    10
}

/// Default health check interval in seconds
pub fn health_check_interval() -> u64 {
    30
}

/// Default health check timeout in seconds
pub fn health_check_timeout() -> u64 {
    5
}

/// Default unhealthy threshold
pub fn unhealthy_threshold() -> u32 {
    3
}

/// Default cache max capacity (number of articles)
pub fn cache_max_capacity() -> u64 {
    10000
}

/// Default cache TTL in seconds (1 hour)
pub fn cache_ttl_secs() -> u64 {
    3600
}

/// Default for TLS certificate verification (true for security)
pub fn tls_verify_cert() -> bool {
    true
}

/// Default maximum number of connections to check per health check cycle
pub fn health_check_max_per_cycle() -> usize {
    use crate::constants::pool::MAX_CONNECTIONS_PER_HEALTH_CHECK_CYCLE;
    MAX_CONNECTIONS_PER_HEALTH_CHECK_CYCLE
}

/// Default timeout in milliseconds when acquiring a connection for health checking
pub fn health_check_pool_timeout_ms() -> u64 {
    use crate::constants::pool::HEALTH_CHECK_POOL_TIMEOUT_MS;
    HEALTH_CHECK_POOL_TIMEOUT_MS
}
