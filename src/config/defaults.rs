//! Default values for configuration fields
//!
//! This module centralizes all default value functions used in serde deserialization.

use crate::types::{CacheCapacity, MaxConnections, MaxErrors};
use std::path::PathBuf;
use std::time::Duration;

/// Default maximum connections per server
#[inline]
pub fn max_connections() -> MaxConnections {
    MaxConnections::try_new(10).expect("10 is non-zero")
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
    MaxErrors::try_new(3).expect("3 is non-zero")
}

/// Default cache max capacity in bytes (memory tier)
#[inline]
pub fn cache_max_capacity() -> CacheCapacity {
    // 64 MB default (good for availability-only mode)
    CacheCapacity::try_new(64 * 1024 * 1024).expect("64MB is non-zero")
}

/// Default cache TTL (1 hour)
#[inline]
pub fn cache_ttl() -> Duration {
    Duration::from_secs(3600)
}

/// Default for caching article bodies (true = full caching)
#[inline]
pub fn cache_articles() -> bool {
    true
}

/// Default for adaptive availability prechecking (false = disabled)
#[inline]
pub fn adaptive_precheck() -> bool {
    false
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

// Disk cache defaults (for hybrid-cache feature)

/// Default disk cache path
#[inline]
pub fn disk_cache_path() -> PathBuf {
    PathBuf::from("/var/cache/nntp-proxy")
}

/// Default disk cache capacity (10 GB)
#[inline]
pub fn disk_cache_capacity() -> CacheCapacity {
    CacheCapacity::try_new(10 * 1024 * 1024 * 1024).expect("10GB is non-zero")
}

/// Default disk cache compression (true = LZ4 enabled)
#[inline]
pub fn disk_cache_compression() -> bool {
    true
}

/// Default disk cache shards
#[inline]
pub fn disk_cache_shards() -> usize {
    4
}
