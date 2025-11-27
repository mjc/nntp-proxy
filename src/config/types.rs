//! Configuration type definitions
//!
//! This module contains all the core configuration structures used by the proxy.

use super::defaults;
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
    /// Stateful 1:1 mode - each client gets a dedicated backend connection
    Stateful,
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
        matches!(self, Self::Stateful | Self::Hybrid)
    }

    /// Get a human-readable description of this routing mode
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Stateful => "stateful 1:1 mode",
            Self::PerCommand => "per-command routing mode (stateless)",
            Self::Hybrid => "hybrid routing mode",
        }
    }
}

impl std::fmt::Display for RoutingMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Routing strategy for backend selection
///
/// Determines how the proxy selects which backend server to route requests to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RoutingStrategy {
    /// Round-robin load balancing (default)
    ///
    /// Distributes requests evenly across all backends in rotation.
    /// Simple, predictable, and works well for homogeneous backend pools.
    RoundRobin,

    /// Adaptive weighted routing based on backend performance
    ///
    /// Selects backends based on real-time metrics:
    /// - Pending request count (40% weight)
    /// - Average TTFB (30% weight)
    /// - Error rate (20% weight)
    /// - Cache hit rate (10% weight)
    ///
    /// Routes more traffic to faster, more reliable backends.
    AdaptiveWeighted,
}

impl Default for RoutingStrategy {
    /// Default routing strategy is RoundRobin for predictable load distribution
    fn default() -> Self {
        Self::RoundRobin
    }
}

impl RoutingStrategy {
    /// Get a human-readable description of this routing strategy
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::RoundRobin => "round-robin load balancing",
            Self::AdaptiveWeighted => "adaptive weighted routing",
        }
    }
}

impl std::fmt::Display for RoutingStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Precheck command to use for article availability detection
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum PrecheckCommand {
    /// Use STAT command (default - less bandwidth)
    Stat,
    /// Use HEAD command (returns headers)
    Head,
    /// Auto-detect: use both initially, then switch to whichever gives negative response on disagreement
    #[default]
    Auto,
}

impl PrecheckCommand {
    /// Get a human-readable description
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Stat => "STAT command",
            Self::Head => "HEAD command",
            Self::Auto => "auto-detect (both commands until disagreement)",
        }
    }
}

impl std::fmt::Display for PrecheckCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Routing configuration
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Routing {
    /// Routing strategy for backend selection
    #[serde(default)]
    pub strategy: RoutingStrategy,

    /// Enable adaptive precheck (parallel STAT to all backends on cache miss)
    ///
    /// When enabled, if an ARTICLE request has no cached location information,
    /// the proxy will STAT all backends in parallel to discover which have the article,
    /// cache the results, and route to the best available backend.
    ///
    /// This is similar to SABnzbd's "precheck" feature but more intelligent:
    /// - Only triggers on cache miss (not for every article)
    /// - Caches results for future requests
    /// - Works with adaptive routing to choose the best backend
    ///
    /// Default: true (enabled for optimal performance)
    #[serde(default = "default_adaptive_precheck")]
    pub adaptive_precheck: bool,

    /// Article location cache size (number of message IDs)
    ///
    /// The cache tracks which backends have which articles to optimize routing.
    /// Uses ~100 bytes per entry. Default 640,000 entries = ~64 MB.
    ///
    /// Set to 0 to disable location caching (not recommended).
    #[serde(default = "default_precheck_cache_size")]
    pub precheck_cache_size: u64,
}

impl Default for Routing {
    fn default() -> Self {
        Self {
            strategy: RoutingStrategy::default(),
            adaptive_precheck: default_adaptive_precheck(),
            precheck_cache_size: default_precheck_cache_size(),
        }
    }
}

/// Default: adaptive precheck disabled (opt-in to avoid latency increase)
const fn default_adaptive_precheck() -> bool {
    false
}

/// Default: 640,000 entries (~64 MB)
const fn default_precheck_cache_size() -> u64 {
    640_000
}

/// Main proxy configuration
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Default)]
pub struct Config {
    /// List of backend NNTP servers
    #[serde(default)]
    pub servers: Vec<Server>,
    /// Proxy server settings
    #[serde(default)]
    pub proxy: Proxy,
    /// Routing configuration
    #[serde(default)]
    pub routing: Routing,
    /// Health check configuration
    #[serde(default)]
    pub health_check: HealthCheck,
    /// Cache configuration (optional, for caching proxy)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<Cache>,
    /// Client authentication configuration
    #[serde(default)]
    pub client_auth: ClientAuth,
}

/// Proxy server settings
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Proxy {
    /// Host/IP to bind to (default: 0.0.0.0)
    pub host: String,
    /// Port to listen on (default: 8119)
    pub port: Port,
    /// Number of worker threads (default: 1, use 0 for CPU cores)
    pub threads: ThreadCount,
}

impl Proxy {
    /// Default listen host (all interfaces)
    pub const DEFAULT_HOST: &'static str = "0.0.0.0";
}

impl Default for Proxy {
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
pub struct Cache {
    /// Maximum number of articles to cache
    #[serde(default = "super::defaults::cache_max_capacity")]
    pub max_capacity: CacheCapacity,
    /// Time-to-live for cached articles
    #[serde(with = "duration_serde", default = "super::defaults::cache_ttl")]
    pub ttl: Duration,
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            max_capacity: defaults::cache_max_capacity(),
            ttl: defaults::cache_ttl(),
        }
    }
}

/// Health check configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthCheck {
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

impl Default for HealthCheck {
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
pub struct ClientAuth {
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

impl ClientAuth {
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
pub struct Server {
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
    /// Command to use for precheck (STAT, HEAD, or Auto)
    /// Auto mode uses both commands initially, then switches to whichever gives negative response on disagreement
    #[serde(default)]
    pub precheck_command: PrecheckCommand,
}

/// Builder for constructing `Server` instances
///
/// Provides a fluent API for creating server configurations, especially useful in tests
/// where creating Server with all 11+ fields is verbose.
///
/// # Examples
///
/// ```
/// use nntp_proxy::config::Server;
///
/// // Minimal configuration
/// let config = Server::builder("news.example.com", 119)
///     .build()
///     .unwrap();
///
/// // With authentication and TLS
/// let config = Server::builder("secure.example.com", 563)
///     .name("Secure Server")
///     .username("user")
///     .password("pass")
///     .max_connections(20)
///     .use_tls(true)
///     .build()
///     .unwrap();
/// ```
pub struct ServerBuilder {
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
    precheck_command: Option<PrecheckCommand>,
}

impl ServerBuilder {
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
            precheck_command: None,
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

    /// Set precheck command (STAT, HEAD, or Auto)
    #[must_use]
    pub fn precheck_command(mut self, command: PrecheckCommand) -> Self {
        self.precheck_command = Some(command);
        self
    }

    /// Build the Server
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Host is empty or invalid
    /// - Port is 0
    /// - Name is empty (when explicitly set)
    /// - Max connections is 0 (when explicitly set)
    pub fn build(self) -> Result<Server, anyhow::Error> {
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

        let precheck_command = self.precheck_command.unwrap_or_default();

        Ok(Server {
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
            precheck_command,
        })
    }
}

impl Server {
    /// Create a builder for constructing a Server
    ///
    /// # Examples
    ///
    /// ```
    /// use nntp_proxy::config::Server;
    ///
    /// let config = Server::builder("news.example.com", 119)
    ///     .name("Example Server")
    ///     .max_connections(15)
    ///     .build()
    ///     .unwrap();
    /// ```
    #[must_use]
    pub fn builder(host: impl Into<String>, port: u16) -> ServerBuilder {
        ServerBuilder::new(host, port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // RoutingMode tests
    #[test]
    fn test_routing_mode_default() {
        assert_eq!(RoutingMode::default(), RoutingMode::Hybrid);
    }

    #[test]
    fn test_routing_mode_supports_per_command() {
        assert!(RoutingMode::PerCommand.supports_per_command_routing());
        assert!(RoutingMode::Hybrid.supports_per_command_routing());
        assert!(!RoutingMode::Stateful.supports_per_command_routing());
    }

    #[test]
    fn test_routing_mode_supports_stateful() {
        assert!(RoutingMode::Stateful.supports_stateful_commands());
        assert!(RoutingMode::Hybrid.supports_stateful_commands());
        assert!(!RoutingMode::PerCommand.supports_stateful_commands());
    }

    #[test]
    fn test_routing_mode_as_str() {
        assert_eq!(RoutingMode::Stateful.as_str(), "stateful 1:1 mode");
        assert_eq!(
            RoutingMode::PerCommand.as_str(),
            "per-command routing mode (stateless)"
        );
        assert_eq!(RoutingMode::Hybrid.as_str(), "hybrid routing mode");
    }

    #[test]
    fn test_routing_mode_display() {
        assert_eq!(RoutingMode::Stateful.to_string(), "stateful 1:1 mode");
        assert_eq!(RoutingMode::Hybrid.to_string(), "hybrid routing mode");
    }

    // Proxy tests
    #[test]
    fn test_proxy_default() {
        let proxy = Proxy::default();
        assert_eq!(proxy.host, "0.0.0.0");
        assert_eq!(proxy.port.get(), 8119);
    }

    #[test]
    fn test_proxy_default_host_constant() {
        assert_eq!(Proxy::DEFAULT_HOST, "0.0.0.0");
    }

    // Cache tests
    #[test]
    fn test_cache_default() {
        let cache = Cache::default();
        assert_eq!(cache.max_capacity.get(), 10000);
        assert_eq!(cache.ttl, Duration::from_secs(3600));
    }

    // HealthCheck tests
    #[test]
    fn test_health_check_default() {
        let hc = HealthCheck::default();
        assert_eq!(hc.interval, Duration::from_secs(30));
        assert_eq!(hc.timeout, Duration::from_secs(5));
        assert_eq!(hc.unhealthy_threshold.get(), 3);
    }

    // ClientAuth tests
    #[test]
    fn test_client_auth_is_enabled_legacy() {
        let mut auth = ClientAuth::default();
        assert!(!auth.is_enabled());

        auth.username = Some("user".to_string());
        auth.password = Some("pass".to_string());
        assert!(auth.is_enabled());
    }

    #[test]
    fn test_client_auth_is_enabled_multi_user() {
        let mut auth = ClientAuth::default();
        auth.users.push(UserCredentials {
            username: "alice".to_string(),
            password: "secret".to_string(),
        });
        assert!(auth.is_enabled());
    }

    #[test]
    fn test_client_auth_all_users_legacy() {
        let auth = ClientAuth {
            username: Some("user".to_string()),
            password: Some("pass".to_string()),
            ..Default::default()
        };

        let users = auth.all_users();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0], ("user", "pass"));
    }

    #[test]
    fn test_client_auth_all_users_multi() {
        let mut auth = ClientAuth::default();
        auth.users.push(UserCredentials {
            username: "alice".to_string(),
            password: "alice_pw".to_string(),
        });
        auth.users.push(UserCredentials {
            username: "bob".to_string(),
            password: "bob_pw".to_string(),
        });

        let users = auth.all_users();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0], ("alice", "alice_pw"));
        assert_eq!(users[1], ("bob", "bob_pw"));
    }

    #[test]
    fn test_client_auth_all_users_combined() {
        let mut auth = ClientAuth {
            username: Some("legacy".to_string()),
            password: Some("legacy_pw".to_string()),
            ..Default::default()
        };
        auth.users.push(UserCredentials {
            username: "alice".to_string(),
            password: "alice_pw".to_string(),
        });

        let users = auth.all_users();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0], ("legacy", "legacy_pw"));
        assert_eq!(users[1], ("alice", "alice_pw"));
    }

    // ServerBuilder tests
    #[test]
    fn test_server_builder_minimal() {
        let server = Server::builder("news.example.com", 119).build().unwrap();

        assert_eq!(server.host.as_str(), "news.example.com");
        assert_eq!(server.port.get(), 119);
        assert_eq!(server.name.as_str(), "news.example.com:119");
        assert_eq!(server.max_connections.get(), 10);
        assert!(!server.use_tls);
        assert!(server.tls_verify_cert); // Secure by default
    }

    #[test]
    fn test_server_builder_with_name() {
        let server = Server::builder("localhost", 119)
            .name("Test Server")
            .build()
            .unwrap();

        assert_eq!(server.name.as_str(), "Test Server");
    }

    #[test]
    fn test_server_builder_with_auth() {
        let server = Server::builder("news.example.com", 119)
            .username("testuser")
            .password("testpass")
            .build()
            .unwrap();

        assert_eq!(server.username.as_ref().unwrap(), "testuser");
        assert_eq!(server.password.as_ref().unwrap(), "testpass");
    }

    #[test]
    fn test_server_builder_with_max_connections() {
        let server = Server::builder("localhost", 119)
            .max_connections(20)
            .build()
            .unwrap();

        assert_eq!(server.max_connections.get(), 20);
    }

    #[test]
    fn test_server_builder_with_tls() {
        let server = Server::builder("secure.example.com", 563)
            .use_tls(true)
            .tls_verify_cert(false)
            .tls_cert_path("/path/to/cert.pem")
            .build()
            .unwrap();

        assert!(server.use_tls);
        assert!(!server.tls_verify_cert);
        assert_eq!(server.tls_cert_path.as_ref().unwrap(), "/path/to/cert.pem");
    }

    #[test]
    fn test_server_builder_with_keepalive() {
        let keepalive = Duration::from_secs(300);
        let server = Server::builder("localhost", 119)
            .connection_keepalive(keepalive)
            .build()
            .unwrap();

        assert_eq!(server.connection_keepalive, Some(keepalive));
    }

    #[test]
    fn test_server_builder_with_health_check_settings() {
        let timeout = Duration::from_millis(500);
        let server = Server::builder("localhost", 119)
            .health_check_max_per_cycle(5)
            .health_check_pool_timeout(timeout)
            .build()
            .unwrap();

        assert_eq!(server.health_check_max_per_cycle, 5);
        assert_eq!(server.health_check_pool_timeout, timeout);
    }

    #[test]
    fn test_server_builder_chaining() {
        let server = Server::builder("news.example.com", 563)
            .name("Production Server")
            .username("admin")
            .password("secret")
            .max_connections(25)
            .use_tls(true)
            .tls_verify_cert(true)
            .build()
            .unwrap();

        assert_eq!(server.name.as_str(), "Production Server");
        assert_eq!(server.max_connections.get(), 25);
        assert!(server.use_tls);
    }

    #[test]
    fn test_server_builder_invalid_host() {
        let result = Server::builder("", 119).build();
        assert!(result.is_err());
    }

    #[test]
    fn test_server_builder_invalid_port() {
        let result = Server::builder("localhost", 0).build();
        assert!(result.is_err());
    }

    #[test]
    fn test_server_builder_invalid_max_connections() {
        let result = Server::builder("localhost", 119).max_connections(0).build();
        assert!(result.is_err());
    }

    #[test]
    fn test_server_builder_from_server_method() {
        let builder = Server::builder("localhost", 119);
        let server = builder.build().unwrap();
        assert_eq!(server.host.as_str(), "localhost");
    }

    // Config tests
    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert!(config.servers.is_empty());
        assert_eq!(config.proxy.host, "0.0.0.0");
        assert!(config.cache.is_none());
        assert!(!config.client_auth.is_enabled());
    }
}
