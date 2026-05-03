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

    /// Get short lowercase name for metrics/logging (no allocation)
    #[must_use]
    pub const fn short_name(&self) -> &'static str {
        match self {
            Self::Stateful => "stateful",
            Self::PerCommand => "per-command",
            Self::Hybrid => "hybrid",
        }
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

/// Backend selection strategy for load balancing
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum BackendSelectionStrategy {
    /// Weighted round-robin - distributes requests proportionally to `max_connections`
    WeightedRoundRobin,
    /// Least-loaded - routes to backend with fewest pending requests
    LeastLoaded,
}

impl Default for BackendSelectionStrategy {
    /// Default is least-loaded for optimal dynamic load distribution
    fn default() -> Self {
        Self::LeastLoaded
    }
}

impl BackendSelectionStrategy {
    /// Get a human-readable description
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::WeightedRoundRobin => "weighted round-robin",
            Self::LeastLoaded => "least-loaded",
        }
    }
}

impl std::fmt::Display for BackendSelectionStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Main proxy configuration
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
pub struct Config {
    /// List of backend NNTP servers
    #[serde(default)]
    pub servers: Vec<Server>,
    /// Proxy server settings
    #[serde(default)]
    pub proxy: Proxy,
    /// Health check configuration
    #[serde(default)]
    pub health_check: HealthCheck,
    /// Cache configuration (optional in config - defaults to 0 capacity for availability tracking only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<Cache>,
    /// Client authentication configuration
    #[serde(default)]
    pub client_auth: ClientAuth,
}

/// Proxy server settings
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Proxy {
    /// Host/IP to bind to (default: 0.0.0.0)
    pub host: String,
    /// Port to listen on (default: 8119)
    pub port: Port,
    /// Number of worker threads (default: 1, use 0 for CPU cores)
    pub threads: ThreadCount,
    /// Routing mode for the proxy
    pub routing_mode: RoutingMode,
    /// Backend selection strategy for load balancing
    pub backend_selection: BackendSelectionStrategy,
    /// Validate yEnc structure and checksums (default: true)
    pub validate_yenc: bool,
    /// Number of buffers in the main buffer pool (default: 50)
    /// Controls memory usage for I/O operations: count × 724KB
    /// Higher values support more concurrent connections but use more memory
    #[serde(default = "super::defaults::buffer_pool_count")]
    pub buffer_pool_count: usize,
    /// Number of buffers in the capture pool for caching (default: 16)
    /// Controls memory usage for cache operations: count × 768KB
    /// Only used when caching is enabled
    #[serde(default = "super::defaults::capture_pool_count")]
    pub capture_pool_count: usize,
    /// Minimum log level for the debug.log file appender (default: "warn")
    /// Accepts tracing filter directives: "error", "warn", "info", "debug", "trace"
    #[serde(default = "super::defaults::log_file_level")]
    pub log_file_level: String,
    /// Path to stats file for metric persistence (optional)
    /// When set, metrics are persisted to this file every 30 seconds and on shutdown
    /// Defaults to "stats.json" alongside the config file if not specified
    #[serde(default)]
    pub stats_file: Option<std::path::PathBuf>,
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
            routing_mode: RoutingMode::default(),
            backend_selection: BackendSelectionStrategy::default(),
            validate_yenc: true,
            buffer_pool_count: defaults::buffer_pool_count(),
            capture_pool_count: defaults::capture_pool_count(),
            log_file_level: defaults::log_file_level(),
            stats_file: None,
        }
    }
}

/// Cache configuration for article caching
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cache {
    /// Maximum cache size in bytes (memory tier for hybrid cache)
    ///
    /// Supports human-readable formats:
    /// - \"1gb\" = 1 GB
    /// - \"500mb\" = 500 MB
    /// - \"64mb\" = 64 MB (default)
    /// - 10000 = 10,000 bytes
    #[serde(default = "super::defaults::cache_max_capacity")]
    pub max_capacity: CacheCapacity,
    /// Time-to-live for cached articles
    #[serde(with = "duration_serde", default = "super::defaults::cache_ttl")]
    pub ttl: Duration,
    /// Whether to cache article bodies (default: true)
    ///
    /// When false:
    /// - Cache still tracks backend availability (smart routing, 430 retry)
    /// - Article bodies are NOT stored (saves ~750KB per article)
    /// - Useful for availability-only mode with limited memory
    ///
    /// When true:
    /// - Full caching mode (bodies + availability tracking)
    /// - Can serve articles from cache without backend query
    #[serde(default = "super::defaults::cache_articles")]
    pub cache_articles: bool,
    /// Enable adaptive availability prechecking for STAT/HEAD commands (default: false)
    ///
    /// When true:
    /// - STAT/HEAD commands with message-ID check all backends simultaneously
    /// - Returns optimistic response to client immediately (assumes article exists)
    /// - Updates availability cache in background based on actual backend responses
    /// - Improves future routing decisions by learning which backends have articles
    /// - For HEAD with `cache_articles=true`, also caches the headers
    ///
    /// When false:
    /// - STAT/HEAD commands use normal routing (single backend check)
    ///
    /// Trade-off: Uses more backend connections but builds accurate availability data
    #[serde(default = "super::defaults::adaptive_precheck")]
    pub adaptive_precheck: bool,

    /// Disk cache configuration (requires `hybrid-cache` feature)
    ///
    /// When enabled, articles evicted from memory are written to disk,
    /// creating a two-tier cache (memory → disk → backend).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<DiskCache>,
}

/// Compression codec for disk cache storage
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum CompressionCodec {
    /// No compression (fastest, largest disk usage)
    None,
    /// LZ4 compression (fast, ~60% reduction for typical NNTP articles, default)
    ///
    /// Uses SIMD (SSE2/AVX2) auto-detection for maximum throughput.
    /// Compression level: fast mode (default).
    #[default]
    Lz4,
    /// Zstandard compression (better ratio, moderate CPU overhead)
    ///
    /// Uses SIMD (SSE2/AVX2/AVX512) auto-detection.
    /// Compression level: 3 (library default, balanced speed/ratio).
    Zstd,
}

impl std::fmt::Display for CompressionCodec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Lz4 => write!(f, "lz4"),
            Self::Zstd => write!(f, "zstd"),
        }
    }
}

/// Disk cache configuration for hybrid caching
///
/// When enabled, creates a two-tier cache:
/// - Hot articles in memory (fast, limited capacity)
/// - Cold articles on disk (slower, larger capacity)
///
/// Requires the `hybrid-cache` feature to be enabled.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DiskCache {
    /// Path to disk cache directory
    ///
    /// Directory will be created if it doesn't exist.
    /// Recommended: Use a fast SSD or `NVMe` drive.
    #[serde(default = "super::defaults::disk_cache_path")]
    pub path: std::path::PathBuf,

    /// Maximum disk cache size in bytes
    ///
    /// Supports human-readable formats:
    /// - \"100gb\" = 100 GB
    /// - \"10gb\" = 10 GB (default)
    /// - \"1tb\" = 1 TB
    #[serde(default = "super::defaults::disk_cache_capacity")]
    pub capacity: CacheCapacity,

    /// Compression codec for disk storage (default: lz4)
    ///
    /// Options:
    /// - "lz4" (default): Fast compression (~60% reduction), minimal CPU overhead
    /// - "zstd": Better compression ratio, moderate CPU overhead
    /// - "none": No compression, fastest but largest disk usage
    ///
    /// For the "lz4" and "zstd" codecs, SIMD optimizations (SSE2/AVX2/AVX512) are
    /// auto-detected and enabled by default. When `compression = "none"`, no
    /// compression or SIMD acceleration is performed.
    #[serde(default = "super::defaults::disk_cache_compression_codec")]
    pub compression: CompressionCodec,

    /// Number of shards for concurrent disk access (default: 4)
    ///
    /// Higher values improve concurrency but use more file handles.
    #[serde(default = "super::defaults::disk_cache_shards")]
    pub shards: usize,
}

impl Default for DiskCache {
    fn default() -> Self {
        Self {
            path: defaults::disk_cache_path(),
            capacity: defaults::disk_cache_capacity(),
            compression: defaults::disk_cache_compression_codec(),
            shards: defaults::disk_cache_shards(),
        }
    }
}

impl Default for Cache {
    fn default() -> Self {
        Self {
            max_capacity: defaults::cache_max_capacity(),
            ttl: defaults::cache_ttl(),
            cache_articles: defaults::cache_articles(),
            adaptive_precheck: defaults::adaptive_precheck(),
            disk: None,
        }
    }
}

/// Health check configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ClientAuth {
    /// Optional custom greeting message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub greeting: Option<String>,
    /// List of authorized users for client authentication
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub users: Vec<UserCredentials>,
}

/// Individual user credentials
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserCredentials {
    pub username: String,
    pub password: String,
}

impl ClientAuth {
    /// Check if authentication is enabled
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        !self.users.is_empty()
    }

    /// Get all users
    #[must_use]
    pub fn all_users(&self) -> Vec<(&str, &str)> {
        self.users
            .iter()
            .map(|user| (user.username.as_str(), user.password.as_str()))
            .collect()
    }
}

/// Configuration for a single backend server
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    /// How long to wait before replacing an actively-removed connection.
    /// Prevents backend connection limit exceeded (482) errors from connection churn.
    /// Default: 30 seconds. Set to 0 to disable.
    #[serde(
        with = "option_duration_serde",
        default = "super::defaults::replacement_cooldown_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub replacement_cooldown: Option<Duration>,
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
    /// Server tier for prioritization (lower = higher priority, default: 0)
    /// Servers with lower tier numbers are tried first; higher tiers only when lower exhausted
    #[serde(default)]
    pub tier: u8,
    /// Wire compression for backend connections (RFC 8054 COMPRESS DEFLATE)
    /// None (default) = auto-detect, Some(true) = require, Some(false) = disable
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compress: Option<bool>,
    /// Compression level (0-9). None = fast (level 1). Higher = better ratio, more CPU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compress_level: Option<u32>,

    /// Duration of proxy-wide inactivity after which this backend's idle connections are cleared.
    /// Prevents stale connections from accumulating during overnight idle periods.
    /// Default: 600 seconds (10 minutes). Set to 0 to disable.
    #[serde(
        with = "duration_serde",
        default = "super::defaults::backend_idle_timeout"
    )]
    pub backend_idle_timeout: Duration,

    /// Enable backend pipelining (request multiplexing) for this server
    /// When enabled, client requests are queued and batched onto shared connections
    /// Default: true
    #[serde(default = "super::defaults::enable_pipelining")]
    pub enable_pipelining: bool,
    /// Maximum queue depth for pipelined requests (backpressure threshold)
    /// Default: 1000
    #[serde(default = "super::defaults::pipeline_queue_depth")]
    pub pipeline_queue_depth: usize,
    /// Maximum number of commands per pipeline batch
    /// Default: 4
    #[serde(default = "super::defaults::pipeline_batch_size")]
    pub pipeline_batch_size: usize,
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
/// use nntp_proxy::types::{Port, MaxConnections};
///
/// // Minimal configuration
/// let config = Server::builder("news.example.com", Port::try_new(119).unwrap())
///     .build()
///     .unwrap();
///
/// // With authentication and TLS
/// let config = Server::builder("secure.example.com", Port::try_new(563).unwrap())
///     .name("Secure Server")
///     .username("user")
///     .password("pass")
///     .max_connections(MaxConnections::try_new(20).unwrap())
///     .use_tls(true)
///     .build()
///     .unwrap();
/// ```
pub struct ServerBuilder {
    host: String,
    port: Port,
    name: Option<String>,
    username: Option<String>,
    password: Option<String>,
    max_connections: Option<MaxConnections>,
    use_tls: bool,
    tls_verify_cert: bool,
    tls_cert_path: Option<String>,
    connection_keepalive: Option<Duration>,
    replacement_cooldown: Option<Duration>,
    health_check_max_per_cycle: Option<usize>,
    health_check_pool_timeout: Option<Duration>,
    tier: u8,
    compress: Option<bool>,
    compress_level: Option<u32>,
    backend_idle_timeout: Option<Duration>,
    enable_pipelining: bool,
    pipeline_queue_depth: Option<usize>,
    pipeline_batch_size: Option<usize>,
}

impl ServerBuilder {
    /// Create a new builder with required parameters
    ///
    /// # Arguments
    /// * `host` - Backend server hostname or IP address
    /// * `port` - Backend server port
    #[must_use]
    pub fn new(host: impl Into<String>, port: Port) -> Self {
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
            replacement_cooldown: None,
            health_check_max_per_cycle: None,
            health_check_pool_timeout: None,
            tier: 0,
            compress: None,
            compress_level: None,
            backend_idle_timeout: None,
            enable_pipelining: true,
            pipeline_queue_depth: None,
            pipeline_batch_size: None,
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
    pub const fn max_connections(mut self, max: MaxConnections) -> Self {
        self.max_connections = Some(max);
        self
    }

    /// Enable TLS/SSL for this backend connection
    #[must_use]
    pub const fn use_tls(mut self, enabled: bool) -> Self {
        self.use_tls = enabled;
        self
    }

    /// Set whether to verify TLS certificates
    #[must_use]
    pub const fn tls_verify_cert(mut self, verify: bool) -> Self {
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
    pub const fn connection_keepalive(mut self, interval: Duration) -> Self {
        self.connection_keepalive = Some(interval);
        self
    }

    /// Set connection replacement cooldown duration
    #[must_use]
    pub const fn replacement_cooldown(mut self, cooldown: Duration) -> Self {
        self.replacement_cooldown = Some(cooldown);
        self
    }

    /// Set maximum connections to check per health check cycle
    #[must_use]
    pub const fn health_check_max_per_cycle(mut self, max: usize) -> Self {
        self.health_check_max_per_cycle = Some(max);
        self
    }

    /// Set timeout for acquiring connections during health checks
    #[must_use]
    pub const fn health_check_pool_timeout(mut self, timeout: Duration) -> Self {
        self.health_check_pool_timeout = Some(timeout);
        self
    }

    /// Set server tier for prioritization (lower = higher priority)
    #[must_use]
    pub const fn tier(mut self, tier: u8) -> Self {
        self.tier = tier;
        self
    }

    /// Set wire compression mode (RFC 8054 COMPRESS DEFLATE)
    #[must_use]
    pub const fn compress(mut self, compress: Option<bool>) -> Self {
        self.compress = compress;
        self
    }

    /// Set compression level (0-9, default: 1 = fast)
    ///
    /// # Panics
    ///
    /// Panics if `level` is greater than 9.
    #[must_use]
    pub fn compress_level(mut self, level: u32) -> Self {
        assert!(level <= 9, "compress_level must be 0-9, got {level}");
        self.compress_level = Some(level);
        self
    }

    /// Set the backend idle timeout duration
    ///
    /// Connections to this backend are cleared after this duration of proxy-wide inactivity.
    /// Default: 10 minutes.
    #[must_use]
    pub const fn backend_idle_timeout(mut self, timeout: Duration) -> Self {
        self.backend_idle_timeout = Some(timeout);
        self
    }

    /// Enable or disable backend pipelining (request multiplexing)
    #[must_use]
    pub const fn enable_pipelining(mut self, enabled: bool) -> Self {
        self.enable_pipelining = enabled;
        self
    }

    /// Set pipeline queue depth
    #[must_use]
    pub const fn pipeline_queue_depth(mut self, depth: usize) -> Self {
        self.pipeline_queue_depth = Some(depth);
        self
    }

    /// Set pipeline batch size
    #[must_use]
    pub const fn pipeline_batch_size(mut self, size: usize) -> Self {
        self.pipeline_batch_size = Some(size);
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
        use crate::types::{HostName, ServerName};

        let host = HostName::try_new(self.host.clone())?;
        let port = self.port; // Already a Port type
        let name_str = self
            .name
            .unwrap_or_else(|| format!("{}:{}", self.host, self.port.get()));
        let name = ServerName::try_new(name_str)?;

        let max_connections = self
            .max_connections
            .unwrap_or_else(super::defaults::max_connections);

        let health_check_max_per_cycle = self
            .health_check_max_per_cycle
            .unwrap_or_else(super::defaults::health_check_max_per_cycle);

        let health_check_pool_timeout = self
            .health_check_pool_timeout
            .unwrap_or_else(super::defaults::health_check_pool_timeout);

        let pipeline_queue_depth = self
            .pipeline_queue_depth
            .unwrap_or_else(super::defaults::pipeline_queue_depth);

        let pipeline_batch_size = self
            .pipeline_batch_size
            .unwrap_or_else(super::defaults::pipeline_batch_size);

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
            replacement_cooldown: self
                .replacement_cooldown
                .or_else(super::defaults::replacement_cooldown_option),
            health_check_max_per_cycle,
            health_check_pool_timeout,
            tier: self.tier,
            compress: self.compress,
            compress_level: self.compress_level,
            backend_idle_timeout: self
                .backend_idle_timeout
                .unwrap_or_else(super::defaults::backend_idle_timeout),
            enable_pipelining: self.enable_pipelining,
            pipeline_queue_depth,
            pipeline_batch_size,
        })
    }
}

impl Server {
    /// Create a builder for configuring a backend server
    ///
    /// # Example
    ///
    /// ```
    /// use nntp_proxy::config::Server;
    /// use nntp_proxy::types::{Port, MaxConnections};
    ///
    /// let config = Server::builder("news.example.com", Port::try_new(119).unwrap())
    ///     .name("Example Server")
    ///     .max_connections(MaxConnections::try_new(15).unwrap())
    ///     .build()
    ///     .unwrap();
    /// ```
    #[must_use]
    pub fn builder(host: impl Into<String>, port: Port) -> ServerBuilder {
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
        assert_eq!(cache.max_capacity.get(), 64 * 1024 * 1024); // 64 MB
        assert_eq!(cache.ttl, Duration::from_hours(1));
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
    fn test_client_auth_is_enabled() {
        let mut auth = ClientAuth::default();
        assert!(!auth.is_enabled());

        auth.users.push(UserCredentials {
            username: "user".to_string(),
            password: "pass".to_string(),
        });
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
    fn test_client_auth_all_users_single() {
        let mut auth = ClientAuth::default();
        auth.users.push(UserCredentials {
            username: "user".to_string(),
            password: "pass".to_string(),
        });

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

    // ServerBuilder tests
    #[test]
    fn test_server_builder_minimal() {
        let server = Server::builder("news.example.com", Port::try_new(119).unwrap())
            .build()
            .unwrap();

        assert_eq!(server.host.as_str(), "news.example.com");
        assert_eq!(server.port.get(), 119);
        assert_eq!(server.name.as_str(), "news.example.com:119");
        assert_eq!(server.max_connections.get(), 10);
        assert!(!server.use_tls);
        assert!(server.tls_verify_cert); // Secure by default
    }

    #[test]
    fn test_server_builder_with_name() {
        let server = Server::builder("localhost", Port::try_new(119).unwrap())
            .name("Test Server")
            .build()
            .unwrap();

        assert_eq!(server.name.as_str(), "Test Server");
    }

    #[test]
    fn test_server_builder_with_auth() {
        let server = Server::builder("news.example.com", Port::try_new(119).unwrap())
            .username("testuser")
            .password("testpass")
            .build()
            .unwrap();

        assert_eq!(server.username.as_ref().unwrap(), "testuser");
        assert_eq!(server.password.as_ref().unwrap(), "testpass");
    }

    #[test]
    fn test_server_builder_with_max_connections() {
        let server = Server::builder("localhost", Port::try_new(119).unwrap())
            .max_connections(MaxConnections::try_new(20).unwrap())
            .build()
            .unwrap();

        assert_eq!(server.max_connections.get(), 20);
    }

    #[test]
    fn test_server_builder_with_tls() {
        let server = Server::builder("secure.example.com", Port::try_new(563).unwrap())
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
        let keepalive = Duration::from_mins(5);
        let server = Server::builder("localhost", Port::try_new(119).unwrap())
            .connection_keepalive(keepalive)
            .build()
            .unwrap();

        assert_eq!(server.connection_keepalive, Some(keepalive));
    }

    #[test]
    fn test_server_builder_with_health_check_settings() {
        let timeout = Duration::from_millis(500);
        let server = Server::builder("localhost", Port::try_new(119).unwrap())
            .health_check_max_per_cycle(5)
            .health_check_pool_timeout(timeout)
            .build()
            .unwrap();

        assert_eq!(server.health_check_max_per_cycle, 5);
        assert_eq!(server.health_check_pool_timeout, timeout);
    }

    #[test]
    fn test_server_builder_chaining() {
        let server = Server::builder("news.example.com", Port::try_new(563).unwrap())
            .name("Production Server")
            .username("admin")
            .password("secret")
            .max_connections(MaxConnections::try_new(25).unwrap())
            .use_tls(true)
            .tls_verify_cert(true)
            .build()
            .unwrap();

        assert_eq!(server.name.as_str(), "Production Server");
        assert_eq!(server.max_connections.get(), 25);
        assert!(server.use_tls);
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

    // CompressionCodec tests
    #[test]
    fn test_compression_codec_serde_lz4() {
        let json = r#""lz4""#;
        let codec: CompressionCodec = serde_json::from_str(json).unwrap();
        assert_eq!(codec, CompressionCodec::Lz4);
        assert_eq!(serde_json::to_string(&codec).unwrap(), json);
    }

    #[test]
    fn test_compression_codec_serde_zstd() {
        let json = r#""zstd""#;
        let codec: CompressionCodec = serde_json::from_str(json).unwrap();
        assert_eq!(codec, CompressionCodec::Zstd);
    }

    #[test]
    fn test_compression_codec_serde_none() {
        let json = r#""none""#;
        let codec: CompressionCodec = serde_json::from_str(json).unwrap();
        assert_eq!(codec, CompressionCodec::None);
    }

    #[test]
    fn test_compression_codec_default_is_lz4() {
        assert_eq!(CompressionCodec::default(), CompressionCodec::Lz4);
    }

    #[test]
    fn test_compression_codec_display() {
        assert_eq!(CompressionCodec::Lz4.to_string(), "lz4");
        assert_eq!(CompressionCodec::Zstd.to_string(), "zstd");
        assert_eq!(CompressionCodec::None.to_string(), "none");
    }

    // DiskCache tests with compression codec
    #[test]
    fn test_disk_cache_default_compression_is_lz4() {
        let disk_cache = DiskCache::default();
        assert_eq!(disk_cache.compression, CompressionCodec::Lz4);
    }

    #[test]
    fn test_disk_cache_deserialize_compression_codec() {
        let toml = r#"
            path = "/tmp/cache"
            capacity = "100mb"
            compression = "zstd"
            shards = 4
        "#;
        let disk_cache: DiskCache = toml::from_str(toml).unwrap();
        assert_eq!(disk_cache.compression, CompressionCodec::Zstd);
    }

    #[test]
    fn test_disk_cache_deserialize_compression_none() {
        let toml = r#"
            path = "/tmp/cache"
            capacity = "100mb"
            compression = "none"
            shards = 4
        "#;
        let disk_cache: DiskCache = toml::from_str(toml).unwrap();
        assert_eq!(disk_cache.compression, CompressionCodec::None);
    }
}
