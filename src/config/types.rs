//! Configuration type definitions
//!
//! This module contains all the core configuration structures used by the proxy.

use serde::{Deserialize, Serialize};

/// Routing mode for the proxy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoutingMode {
    /// Standard 1:1 mode - each client gets a dedicated backend connection
    Standard,
    /// Per-command routing - each command can use a different backend (stateless only)
    PerCommand,
    /// Hybrid mode - starts in per-command routing, auto-switches to stateful on first stateful command
    Hybrid,
}

impl Default for RoutingMode {
    /// Default routing mode is Hybrid, which provides optimal performance and full protocol support.
    /// This mode automatically starts in per-command routing for efficiency and seamlessly switches
    /// to stateful mode when commands requiring group context are detected.
    fn default() -> Self {
        Self::Hybrid
    }
}

impl RoutingMode {
    /// Check if this mode supports per-command routing
    #[must_use]
    pub fn supports_per_command_routing(&self) -> bool {
        matches!(self, Self::PerCommand | Self::Hybrid)
    }

    /// Check if this mode can handle stateful commands
    #[must_use]
    pub fn supports_stateful_commands(&self) -> bool {
        matches!(self, Self::Standard | Self::Hybrid)
    }
}

/// Main proxy configuration
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
pub struct Config {
    /// List of backend NNTP servers
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
    /// Health check configuration
    #[serde(default)]
    pub health_check: HealthCheckConfig,
    /// Cache configuration (optional, for caching proxy)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheConfig>,
}

/// Cache configuration for article caching
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CacheConfig {
    /// Maximum number of articles to cache
    #[serde(default = "super::defaults::cache_max_capacity")]
    pub max_capacity: u64,
    /// Time-to-live for cached articles in seconds
    #[serde(default = "super::defaults::cache_ttl_secs")]
    pub ttl_secs: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_capacity: super::defaults::cache_max_capacity(),
            ttl_secs: super::defaults::cache_ttl_secs(),
        }
    }
}

/// Health check configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthCheckConfig {
    /// Interval between health checks in seconds
    #[serde(default = "super::defaults::health_check_interval")]
    pub interval_secs: u64,
    /// Timeout for each health check in seconds
    #[serde(default = "super::defaults::health_check_timeout")]
    pub timeout_secs: u64,
    /// Number of consecutive failures before marking unhealthy
    #[serde(default = "super::defaults::unhealthy_threshold")]
    pub unhealthy_threshold: u32,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            interval_secs: super::defaults::health_check_interval(),
            timeout_secs: super::defaults::health_check_timeout(),
            unhealthy_threshold: super::defaults::unhealthy_threshold(),
        }
    }
}

/// Configuration for a single backend server
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Maximum number of concurrent connections to this server
    #[serde(default = "super::defaults::max_connections")]
    pub max_connections: u32,

    /// Enable TLS/SSL for this backend connection
    #[serde(default)]
    pub use_tls: bool,
    /// Verify TLS certificates (recommended for production)
    #[serde(default = "super::defaults::tls_verify_cert")]
    pub tls_verify_cert: bool,
    /// Optional path to custom CA certificate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_cert_path: Option<String>,
    /// Interval in seconds to send keep-alive commands (DATE) on idle connections
    /// Set to 0 to disable keep-alive (default)
    #[serde(default)]
    pub connection_keepalive_secs: u64,
    /// Maximum number of connections to check per health check cycle
    /// Lower values reduce pool contention but may take longer to detect all stale connections
    #[serde(default = "super::defaults::health_check_max_per_cycle")]
    pub health_check_max_per_cycle: usize,
    /// Timeout in milliseconds when acquiring a connection for health checking
    /// Short timeout prevents blocking if pool is busy
    #[serde(default = "super::defaults::health_check_pool_timeout_ms")]
    pub health_check_pool_timeout_ms: u64,
}
