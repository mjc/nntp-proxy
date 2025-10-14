//! Configuration type definitions
//!
//! This module contains all the core configuration structures used by the proxy.

use crate::types::{
    CacheCapacity, HostName, MaxConnections, MaxErrors, Port, ServerName, duration_serde,
    option_duration_serde,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

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
    pub max_capacity: CacheCapacity,
    /// Time-to-live for cached articles
    #[serde(with = "duration_serde", default = "super::defaults::cache_ttl")]
    pub ttl: Duration,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_capacity: super::defaults::cache_max_capacity(),
            ttl: super::defaults::cache_ttl(),
        }
    }
}

/// Health check configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthCheckConfig {
    /// Interval between health checks
    #[serde(
        with = "duration_serde",
        default = "super::defaults::health_check_interval"
    )]
    pub interval: Duration,
    /// Timeout for each health check
    #[serde(
        with = "duration_serde",
        default = "super::defaults::health_check_timeout"
    )]
    pub timeout: Duration,
    /// Number of consecutive failures before marking unhealthy
    #[serde(default = "super::defaults::unhealthy_threshold")]
    pub unhealthy_threshold: MaxErrors,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            interval: super::defaults::health_check_interval(),
            timeout: super::defaults::health_check_timeout(),
            unhealthy_threshold: super::defaults::unhealthy_threshold(),
        }
    }
}

/// Configuration for a single backend server
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ServerConfig {
    pub host: HostName,
    pub port: Port,
    pub name: ServerName,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Maximum number of concurrent connections to this server
    #[serde(default = "super::defaults::max_connections")]
    pub max_connections: MaxConnections,

    /// Enable TLS/SSL for this backend connection
    #[serde(default)]
    pub use_tls: bool,
    /// Verify TLS certificates (recommended for production)
    #[serde(default = "super::defaults::tls_verify_cert")]
    pub tls_verify_cert: bool,
    /// Optional path to custom CA certificate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_cert_path: Option<String>,
    /// Interval to send keep-alive commands (DATE) on idle connections
    /// None disables keep-alive (default)
    #[serde(
        with = "option_duration_serde",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub connection_keepalive: Option<Duration>,
    /// Maximum number of connections to check per health check cycle
    /// Lower values reduce pool contention but may take longer to detect all stale connections
    #[serde(default = "super::defaults::health_check_max_per_cycle")]
    pub health_check_max_per_cycle: usize,
    /// Timeout when acquiring a connection for health checking
    /// Short timeout prevents blocking if pool is busy
    #[serde(
        with = "duration_serde",
        default = "super::defaults::health_check_pool_timeout"
    )]
    pub health_check_pool_timeout: Duration,
}
