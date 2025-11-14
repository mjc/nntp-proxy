//! Configuration type definitions
//!
//! This module contains all the core configuration structures used by the proxy.

use crate::types::{
    CacheCapacity, HostName, MaxConnections, MaxErrors, Port, ServerName, ThreadCount,
    duration_serde, option_duration_serde,
};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Routing mode for the proxy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
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
    pub const fn supports_per_command_routing(&self) -> bool {
        matches!(self, Self::PerCommand | Self::Hybrid)
    }

    /// Check if this mode can handle stateful commands
    #[must_use]
    pub const fn supports_stateful_commands(&self) -> bool {
        matches!(self, Self::Standard | Self::Hybrid)
    }

    /// Get a human-readable description of this routing mode
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Standard => "standard 1:1 mode",
            Self::PerCommand => "per-command routing mode",
            Self::Hybrid => "hybrid routing mode",
        }
    }
}

impl std::fmt::Display for RoutingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Main proxy configuration
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
pub struct Config {
    /// List of backend NNTP servers
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
    /// Proxy server settings
    #[serde(default)]
    pub proxy: ProxyConfig,
    /// Health check configuration
    #[serde(default)]
    pub health_check: HealthCheckConfig,
    /// Cache configuration (optional, for caching proxy)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheConfig>,
    /// Client authentication configuration
    #[serde(default)]
    pub client_auth: ClientAuthConfig,
}

/// Proxy server settings
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ProxyConfig {
    /// Host/IP to bind to (default: 0.0.0.0)
    pub host: String,
    /// Port to listen on (default: 8119)
    pub port: Port,
    /// Number of worker threads (default: 1, use 0 for CPU cores)
    pub threads: ThreadCount,
}

impl ProxyConfig {
    /// Default listen host (all interfaces)
    pub const DEFAULT_HOST: &'static str = "0.0.0.0";
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            host: Self::DEFAULT_HOST.to_string(),
            port: Port::default(),
            threads: ThreadCount::default(),
        }
    }
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

/// Client authentication configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ClientAuthConfig {
    /// Required username for client authentication (if set, auth is enabled)
    /// DEPRECATED: Use `users` instead for multi-user support
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Required password for client authentication
    /// DEPRECATED: Use `users` instead for multi-user support
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Optional custom greeting message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub greeting: Option<String>,
    /// List of authorized users (replaces username/password for multi-user support)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<UserCredentials>,
}

/// Individual user credentials
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserCredentials {
    pub username: String,
    pub password: String,
}

impl ClientAuthConfig {
    /// Check if authentication is enabled
    pub fn is_enabled(&self) -> bool {
        // Auth is enabled if either the legacy single-user config or multi-user list is populated
        (!self.users.is_empty()) || (self.username.is_some() && self.password.is_some())
    }

    /// Get all users (combines legacy + new format)
    pub fn all_users(&self) -> Vec<(&str, &str)> {
        let mut users = Vec::new();

        // Add legacy single user if present
        if let (Some(u), Some(p)) = (&self.username, &self.password) {
            users.push((u.as_str(), p.as_str()));
        }

        // Add multi-user list
        for user in &self.users {
            users.push((user.username.as_str(), user.password.as_str()));
        }

        users
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

/// Builder for constructing `ServerConfig` instances
///
/// Provides a fluent API for creating server configurations, especially useful in tests
/// where creating ServerConfig with all 11+ fields is verbose.
///
/// # Examples
///
/// ```
/// use nntp_proxy::config::ServerConfig;
///
/// // Minimal configuration
/// let config = ServerConfig::builder("news.example.com", 119)
///     .build()
///     .unwrap();
///
/// // With authentication and TLS
/// let config = ServerConfig::builder("secure.example.com", 563)
///     .name("Secure Server")
///     .username("user")
///     .password("pass")
///     .max_connections(20)
///     .use_tls(true)
///     .build()
///     .unwrap();
/// ```
pub struct ServerConfigBuilder {
    host: String,
    port: u16,
    name: Option<String>,
    username: Option<String>,
    password: Option<String>,
    max_connections: Option<usize>,
    use_tls: bool,
    tls_verify_cert: bool,
    tls_cert_path: Option<String>,
    connection_keepalive: Option<Duration>,
    health_check_max_per_cycle: Option<usize>,
    health_check_pool_timeout: Option<Duration>,
}

impl ServerConfigBuilder {
    /// Create a new builder with required parameters
    ///
    /// # Arguments
    /// * `host` - Backend server hostname or IP address
    /// * `port` - Backend server port number
    #[must_use]
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            name: None,
            username: None,
            password: None,
            max_connections: None,
            use_tls: false,
            tls_verify_cert: true, // Secure by default
            tls_cert_path: None,
            connection_keepalive: None,
            health_check_max_per_cycle: None,
            health_check_pool_timeout: None,
        }
    }

    /// Set a friendly name for logging (defaults to "host:port")
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Set authentication username
    #[must_use]
    pub fn username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    /// Set authentication password
    #[must_use]
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Set maximum number of concurrent connections
    #[must_use]
    pub fn max_connections(mut self, max: usize) -> Self {
        self.max_connections = Some(max);
        self
    }

    /// Enable TLS/SSL for this backend connection
    #[must_use]
    pub fn use_tls(mut self, enabled: bool) -> Self {
        self.use_tls = enabled;
        self
    }

    /// Set whether to verify TLS certificates
    #[must_use]
    pub fn tls_verify_cert(mut self, verify: bool) -> Self {
        self.tls_verify_cert = verify;
        self
    }

    /// Set path to custom CA certificate
    #[must_use]
    pub fn tls_cert_path(mut self, path: impl Into<String>) -> Self {
        self.tls_cert_path = Some(path.into());
        self
    }

    /// Set keep-alive interval for idle connections
    #[must_use]
    pub fn connection_keepalive(mut self, interval: Duration) -> Self {
        self.connection_keepalive = Some(interval);
        self
    }

    /// Set maximum connections to check per health check cycle
    #[must_use]
    pub fn health_check_max_per_cycle(mut self, max: usize) -> Self {
        self.health_check_max_per_cycle = Some(max);
        self
    }

    /// Set timeout for acquiring connections during health checks
    #[must_use]
    pub fn health_check_pool_timeout(mut self, timeout: Duration) -> Self {
        self.health_check_pool_timeout = Some(timeout);
        self
    }

    /// Build the ServerConfig
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Host is empty or invalid
    /// - Port is 0
    /// - Name is empty (when explicitly set)
    /// - Max connections is 0 (when explicitly set)
    pub fn build(self) -> Result<ServerConfig, anyhow::Error> {
        use crate::types::{HostName, MaxConnections, Port, ServerName};

        let host = HostName::new(self.host.clone())?;

        let port = Port::new(self.port)
            .ok_or_else(|| anyhow::anyhow!("Invalid port: {} (must be 1-65535)", self.port))?;

        let name_str = self
            .name
            .unwrap_or_else(|| format!("{}:{}", self.host, self.port));
        let name = ServerName::new(name_str)?;

        let max_connections = if let Some(max) = self.max_connections {
            MaxConnections::new(max)
                .ok_or_else(|| anyhow::anyhow!("Invalid max_connections: {} (must be > 0)", max))?
        } else {
            super::defaults::max_connections()
        };

        let health_check_max_per_cycle = self
            .health_check_max_per_cycle
            .unwrap_or_else(super::defaults::health_check_max_per_cycle);

        let health_check_pool_timeout = self
            .health_check_pool_timeout
            .unwrap_or_else(super::defaults::health_check_pool_timeout);

        Ok(ServerConfig {
            host,
            port,
            name,
            username: self.username,
            password: self.password,
            max_connections,
            use_tls: self.use_tls,
            tls_verify_cert: self.tls_verify_cert,
            tls_cert_path: self.tls_cert_path,
            connection_keepalive: self.connection_keepalive,
            health_check_max_per_cycle,
            health_check_pool_timeout,
        })
    }
}

impl ServerConfig {
    /// Create a builder for constructing a ServerConfig
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::config::ServerConfig;
    ///
    /// let config = ServerConfig::builder("news.example.com", 119)
    ///     .name("Example Server")
    ///     .max_connections(15)
    ///     .build()
    ///     .unwrap();
    /// ```
    #[must_use]
    pub fn builder(host: impl Into<String>, port: u16) -> ServerConfigBuilder {
        ServerConfigBuilder::new(host, port)
    }
}
