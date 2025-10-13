//! Configuration module
//!
//! This module handles all configuration types and loading
//! for the NNTP proxy server.

use anyhow::Result;
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

/// Default maximum connections per server
fn default_max_connections() -> u32 {
    10
}

/// Default health check interval in seconds
fn default_health_check_interval() -> u64 {
    30
}

/// Default health check timeout in seconds
fn default_health_check_timeout() -> u64 {
    5
}

/// Default unhealthy threshold
fn default_unhealthy_threshold() -> u32 {
    3
}

/// Default cache max capacity (number of articles)
fn default_cache_max_capacity() -> u64 {
    10000
}

/// Default cache TTL in seconds (1 hour)
fn default_cache_ttl_secs() -> u64 {
    3600
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
    #[serde(default = "default_cache_max_capacity")]
    pub max_capacity: u64,
    /// Time-to-live for cached articles in seconds
    #[serde(default = "default_cache_ttl_secs")]
    pub ttl_secs: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_capacity: default_cache_max_capacity(),
            ttl_secs: default_cache_ttl_secs(),
        }
    }
}

/// Health check configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HealthCheckConfig {
    /// Interval between health checks in seconds
    #[serde(default = "default_health_check_interval")]
    pub interval_secs: u64,
    /// Timeout for each health check in seconds
    #[serde(default = "default_health_check_timeout")]
    pub timeout_secs: u64,
    /// Number of consecutive failures before marking unhealthy
    #[serde(default = "default_unhealthy_threshold")]
    pub unhealthy_threshold: u32,
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_health_check_interval(),
            timeout_secs: default_health_check_timeout(),
            unhealthy_threshold: default_unhealthy_threshold(),
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
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,

    /// Enable TLS/SSL for this backend connection
    #[serde(default)]
    pub use_tls: bool,
    /// Verify TLS certificates (recommended for production)
    #[serde(default = "default_tls_verify_cert")]
    pub tls_verify_cert: bool,
    /// Optional path to custom CA certificate
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_cert_path: Option<String>,
}

/// Default for TLS certificate verification (true for security)
fn default_tls_verify_cert() -> bool {
    true
}

impl Config {
    /// Validate configuration for correctness
    ///
    /// Checks for:
    /// - Empty server names
    /// - Invalid ports (0)
    /// - Invalid max_connections (0)
    /// - At least one server configured
    pub fn validate(&self) -> Result<()> {
        if self.servers.is_empty() {
            return Err(anyhow::anyhow!(
                "Configuration must have at least one server"
            ));
        }

        for server in &self.servers {
            if server.name.trim().is_empty() {
                return Err(anyhow::anyhow!("Server name cannot be empty"));
            }
            if server.host.trim().is_empty() {
                return Err(anyhow::anyhow!("Server '{}' has empty host", server.name));
            }
            if server.port == 0 {
                return Err(anyhow::anyhow!(
                    "Invalid port 0 for server '{}'",
                    server.name
                ));
            }
            if server.max_connections == 0 {
                return Err(anyhow::anyhow!(
                    "max_connections must be > 0 for server '{}'",
                    server.name
                ));
            }
        }

        // Validate health check configuration
        if self.health_check.interval_secs == 0 {
            return Err(anyhow::anyhow!("health_check.interval_secs must be > 0"));
        }
        if self.health_check.timeout_secs == 0 {
            return Err(anyhow::anyhow!("health_check.timeout_secs must be > 0"));
        }
        if self.health_check.unhealthy_threshold == 0 {
            return Err(anyhow::anyhow!(
                "health_check.unhealthy_threshold must be > 0"
            ));
        }

        // Validate cache configuration if present
        if let Some(cache) = &self.cache {
            if cache.max_capacity == 0 {
                return Err(anyhow::anyhow!("cache.max_capacity must be > 0"));
            }
            if cache.ttl_secs == 0 {
                return Err(anyhow::anyhow!("cache.ttl_secs must be > 0"));
            }
        }

        Ok(())
    }
}

/// Load backend server configuration from environment variables
///
/// Supports indexed environment variables for Docker/container deployments:
/// - `NNTP_SERVER_0_HOST`, `NNTP_SERVER_0_PORT`, `NNTP_SERVER_0_NAME`, etc.
/// - `NNTP_SERVER_1_HOST`, `NNTP_SERVER_1_PORT`, `NNTP_SERVER_1_NAME`, etc.
///
/// Optional per-server variables:
/// - `NNTP_SERVER_N_USERNAME` - Backend authentication username
/// - `NNTP_SERVER_N_PASSWORD` - Backend authentication password
/// - `NNTP_SERVER_N_MAX_CONNECTIONS` - Max connections (default: 10)
///
/// If any `NNTP_SERVER_N_HOST` is found, environment variables take precedence
/// over config file servers.
fn load_servers_from_env() -> Option<Vec<ServerConfig>> {
    let mut servers = Vec::new();
    let mut index = 0;

    loop {
        // Check if this server index exists by looking for HOST
        let host_key = format!("NNTP_SERVER_{}_HOST", index);
        let host = match std::env::var(&host_key) {
            Ok(h) => h,
            Err(_) => {
                // No more servers found
                break;
            }
        };

        // Parse port (required)
        let port_key = format!("NNTP_SERVER_{}_PORT", index);
        let port = std::env::var(&port_key)
            .ok()
            .and_then(|p| p.parse::<u16>().ok())
            .unwrap_or(119); // Default NNTP port

        // Get name (required, use host as fallback)
        let name_key = format!("NNTP_SERVER_{}_NAME", index);
        let name = std::env::var(&name_key).unwrap_or_else(|_| format!("Server {}", index));

        // Optional fields
        let username_key = format!("NNTP_SERVER_{}_USERNAME", index);
        let username = std::env::var(&username_key).ok();

        let password_key = format!("NNTP_SERVER_{}_PASSWORD", index);
        let password = std::env::var(&password_key).ok();

        let max_conn_key = format!("NNTP_SERVER_{}_MAX_CONNECTIONS", index);
        let max_connections = std::env::var(&max_conn_key)
            .ok()
            .and_then(|m| m.parse::<u32>().ok())
            .unwrap_or_else(default_max_connections);

        servers.push(ServerConfig {
            host,
            port,
            name,
            username,
            password,
            max_connections,
            use_tls: false,
            tls_verify_cert: default_tls_verify_cert(),
            tls_cert_path: None,
        });

        index += 1;
    }

    if servers.is_empty() {
        None
    } else {
        Some(servers)
    }
}

/// Load configuration from a TOML file, with environment variable overrides
///
/// Environment variables for backend servers take precedence over config file:
/// - `NNTP_SERVER_0_HOST`, `NNTP_SERVER_0_PORT`, `NNTP_SERVER_0_NAME`
/// - `NNTP_SERVER_1_HOST`, `NNTP_SERVER_1_PORT`, `NNTP_SERVER_1_NAME`
/// - etc.
///
/// This allows Docker/container deployments to override servers without
/// modifying the config file.
pub fn load_config(config_path: &str) -> Result<Config> {
    let config_content = std::fs::read_to_string(config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read config file '{}': {}", config_path, e))?;

    let mut config: Config = toml::from_str(&config_content)
        .map_err(|e| anyhow::anyhow!("Failed to parse config file '{}': {}", config_path, e))?;

    // Check for environment variable server overrides
    if let Some(env_servers) = load_servers_from_env() {
        tracing::info!(
            "Using {} backend server(s) from environment variables (overriding config file)",
            env_servers.len()
        );
        config.servers = env_servers;
    }

    // Validate the loaded configuration
    config.validate()?;

    Ok(config)
}

/// Create a default configuration for examples/testing
#[must_use]
pub fn create_default_config() -> Config {
    Config {
        servers: vec![ServerConfig {
            host: "news.example.com".to_string(),
            port: 119,
            name: "Example News Server".to_string(),
            username: None,
            password: None,
            max_connections: default_max_connections(),
            use_tls: false,
            tls_verify_cert: default_tls_verify_cert(),
            tls_cert_path: None,
        }],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn create_test_config() -> Config {
        Config {
            servers: vec![
                ServerConfig {
                    host: "server1.example.com".to_string(),
                    port: 119,
                    name: "Test Server 1".to_string(),
                    username: None,
                    password: None,
                    max_connections: 5,
                    use_tls: false,
                    tls_verify_cert: default_tls_verify_cert(),
                    tls_cert_path: None,
                },
                ServerConfig {
                    host: "server2.example.com".to_string(),
                    port: 119,
                    name: "Test Server 2".to_string(),
                    username: None,
                    password: None,
                    max_connections: 8,
                    use_tls: false,
                    tls_verify_cert: default_tls_verify_cert(),
                    tls_cert_path: None,
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn test_server_config_creation() {
        let config = ServerConfig {
            host: "news.example.com".to_string(),
            port: 119,
            name: "Example Server".to_string(),
            username: None,
            password: None,
            max_connections: 15,
            use_tls: false,
            tls_verify_cert: default_tls_verify_cert(),
            tls_cert_path: None,
        };

        assert_eq!(config.host, "news.example.com");
        assert_eq!(config.port, 119);
        assert_eq!(config.name, "Example Server");
        assert_eq!(config.max_connections, 15);
    }

    #[test]
    fn test_load_config_from_file() -> Result<()> {
        let config = create_test_config();
        let config_toml = toml::to_string_pretty(&config)?;

        // Create a temporary file
        let mut temp_file = NamedTempFile::new()?;
        write!(temp_file, "{}", config_toml)?;

        // Load config from file
        let loaded_config = load_config(temp_file.path().to_str().unwrap())?;

        assert_eq!(loaded_config.servers.len(), 2);
        assert_eq!(loaded_config.servers[0].name, "Test Server 1");
        assert_eq!(loaded_config.servers[0].host, "server1.example.com");
        assert_eq!(loaded_config.servers[0].port, 119);

        Ok(())
    }

    #[test]
    fn test_load_config_nonexistent_file() {
        let result = load_config("/nonexistent/path/config.toml");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to read config file")
        );
    }

    #[test]
    fn test_load_config_invalid_toml() -> Result<()> {
        let invalid_toml = "invalid toml content [[[";

        // Create a temporary file with invalid TOML
        let mut temp_file = NamedTempFile::new()?;
        write!(temp_file, "{}", invalid_toml)?;

        let result = load_config(temp_file.path().to_str().unwrap());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Failed to parse config file")
        );

        Ok(())
    }

    #[test]
    fn test_create_default_config() {
        let config = create_default_config();

        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].host, "news.example.com");
        assert_eq!(config.servers[0].port, 119);
        assert_eq!(config.servers[0].name, "Example News Server");
    }

    #[test]
    fn test_config_serialization() -> Result<()> {
        let config = create_test_config();

        // Serialize to TOML
        let toml_string = toml::to_string_pretty(&config)?;
        assert!(toml_string.contains("server1.example.com"));
        assert!(toml_string.contains("Test Server 1"));

        // Deserialize back
        let deserialized: Config = toml::from_str(&toml_string)?;
        assert_eq!(deserialized, config);

        Ok(())
    }

    #[test]
    fn test_config_with_authentication() -> Result<()> {
        let config = Config {
            servers: vec![ServerConfig {
                host: "secure.news.com".to_string(),
                port: 563,
                name: "Secure Server".to_string(),
                username: Some("user123".to_string()),
                password: Some("password123".to_string()),
                max_connections: 10,
                use_tls: false,
                tls_verify_cert: default_tls_verify_cert(),
                tls_cert_path: None,
            }],
            ..Default::default()
        };

        // Serialize and deserialize
        let toml_string = toml::to_string_pretty(&config)?;
        let deserialized: Config = toml::from_str(&toml_string)?;

        assert_eq!(
            deserialized.servers[0].username,
            Some("user123".to_string())
        );
        assert_eq!(
            deserialized.servers[0].password,
            Some("password123".to_string())
        );

        Ok(())
    }

    #[test]
    fn test_config_missing_username_with_password() -> Result<()> {
        let toml_str = r#"
[[servers]]
host = "news.example.com"
port = 119
name = "Test"
password = "secret"
max_connections = 5
"#;

        let config: Result<Config, _> = toml::from_str(toml_str);
        // Should still parse - validation happens at runtime if needed
        assert!(config.is_ok());

        Ok(())
    }

    #[test]
    fn test_config_edge_case_ports() -> Result<()> {
        // Test minimum valid port
        let config1 = Config {
            servers: vec![ServerConfig {
                host: "news.com".to_string(),
                port: 1,
                name: "Min Port".to_string(),
                username: None,
                password: None,
                max_connections: 5,
                use_tls: false,
                tls_verify_cert: default_tls_verify_cert(),
                tls_cert_path: None,
            }],
            ..Default::default()
        };
        let toml1 = toml::to_string(&config1)?;
        let parsed1: Config = toml::from_str(&toml1)?;
        assert_eq!(parsed1.servers[0].port, 1);

        // Test maximum valid port
        let config2 = Config {
            servers: vec![ServerConfig {
                host: "news.com".to_string(),
                port: 65535,
                name: "Max Port".to_string(),
                username: None,
                password: None,
                max_connections: 5,
                use_tls: false,
                tls_verify_cert: default_tls_verify_cert(),
                tls_cert_path: None,
            }],
            ..Default::default()
        };
        let toml2 = toml::to_string(&config2)?;
        let parsed2: Config = toml::from_str(&toml2)?;
        assert_eq!(parsed2.servers[0].port, 65535);

        Ok(())
    }

    #[test]
    fn test_config_multiple_servers() -> Result<()> {
        let config = Config {
            servers: vec![
                ServerConfig {
                    host: "server1.com".to_string(),
                    port: 119,
                    name: "Server 1".to_string(),
                    username: None,
                    password: None,
                    max_connections: 5,
                    use_tls: false,
                    tls_verify_cert: default_tls_verify_cert(),
                    tls_cert_path: None,
                },
                ServerConfig {
                    host: "server2.com".to_string(),
                    port: 563,
                    name: "Server 2".to_string(),
                    username: Some("user".to_string()),
                    password: Some("pass".to_string()),
                    max_connections: 10,
                    use_tls: false,
                    tls_verify_cert: default_tls_verify_cert(),
                    tls_cert_path: None,
                },
                ServerConfig {
                    host: "server3.com".to_string(),
                    port: 8119,
                    name: "Server 3".to_string(),
                    username: None,
                    password: None,
                    max_connections: 15,
                    use_tls: false,
                    tls_verify_cert: default_tls_verify_cert(),
                    tls_cert_path: None,
                },
            ],
            ..Default::default()
        };

        let toml_string = toml::to_string_pretty(&config)?;
        let deserialized: Config = toml::from_str(&toml_string)?;

        assert_eq!(deserialized.servers.len(), 3);
        assert_eq!(deserialized.servers[1].username, Some("user".to_string()));
        assert_eq!(deserialized.servers[2].port, 8119);

        Ok(())
    }

    #[test]
    fn test_config_empty_strings() -> Result<()> {
        let toml_str = r#"
[[servers]]
host = ""
port = 119
name = ""
max_connections = 5
"#;

        let config: Config = toml::from_str(toml_str)?;
        assert_eq!(config.servers[0].host, "");
        assert_eq!(config.servers[0].name, "");

        Ok(())
    }

    #[test]
    fn test_config_special_characters_in_strings() -> Result<()> {
        let config = Config {
            servers: vec![ServerConfig {
                host: "news-server.example.com".to_string(),
                port: 119,
                name: "Test Server (Production) #1".to_string(),
                username: Some("user@domain.com".to_string()),
                password: Some("p@ssw0rd!#$%".to_string()),
                max_connections: 5,
                use_tls: false,
                tls_verify_cert: default_tls_verify_cert(),
                tls_cert_path: None,
            }],
            ..Default::default()
        };

        let toml_string = toml::to_string_pretty(&config)?;
        let deserialized: Config = toml::from_str(&toml_string)?;

        assert_eq!(deserialized.servers[0].host, "news-server.example.com");
        assert_eq!(deserialized.servers[0].name, "Test Server (Production) #1");
        assert_eq!(
            deserialized.servers[0].username,
            Some("user@domain.com".to_string())
        );

        Ok(())
    }

    #[test]
    fn test_config_max_connections_bounds() -> Result<()> {
        // Test minimum connections
        let config1 = Config {
            servers: vec![ServerConfig {
                host: "news.com".to_string(),
                port: 119,
                name: "Min Connections".to_string(),
                username: None,
                password: None,
                max_connections: 1,
                use_tls: false,
                tls_verify_cert: default_tls_verify_cert(),
                tls_cert_path: None,
            }],
            ..Default::default()
        };
        let toml1 = toml::to_string(&config1)?;
        let parsed1: Config = toml::from_str(&toml1)?;
        assert_eq!(parsed1.servers[0].max_connections, 1);

        // Test large connections
        let config2 = Config {
            servers: vec![ServerConfig {
                host: "news.com".to_string(),
                port: 119,
                name: "Many Connections".to_string(),
                username: None,
                password: None,
                max_connections: 1000,
                use_tls: false,
                tls_verify_cert: default_tls_verify_cert(),
                tls_cert_path: None,
            }],
            ..Default::default()
        };
        let toml2 = toml::to_string(&config2)?;
        let parsed2: Config = toml::from_str(&toml2)?;
        assert_eq!(parsed2.servers[0].max_connections, 1000);

        Ok(())
    }

    #[test]
    fn test_config_ipv4_and_ipv6_hosts() -> Result<()> {
        let config = Config {
            servers: vec![
                ServerConfig {
                    host: "192.168.1.1".to_string(),
                    port: 119,
                    name: "IPv4 Server".to_string(),
                    username: None,
                    password: None,
                    max_connections: 5,
                    use_tls: false,
                    tls_verify_cert: default_tls_verify_cert(),
                    tls_cert_path: None,
                },
                ServerConfig {
                    host: "::1".to_string(),
                    port: 119,
                    name: "IPv6 Server".to_string(),
                    username: None,
                    password: None,
                    max_connections: 5,
                    use_tls: false,
                    tls_verify_cert: default_tls_verify_cert(),
                    tls_cert_path: None,
                },
                ServerConfig {
                    host: "2001:db8::1".to_string(),
                    port: 119,
                    name: "IPv6 Global".to_string(),
                    username: None,
                    password: None,
                    max_connections: 5,
                    use_tls: false,
                    tls_verify_cert: default_tls_verify_cert(),
                    tls_cert_path: None,
                },
            ],
            ..Default::default()
        };

        let toml_string = toml::to_string_pretty(&config)?;
        let deserialized: Config = toml::from_str(&toml_string)?;

        assert_eq!(deserialized.servers[0].host, "192.168.1.1");
        assert_eq!(deserialized.servers[1].host, "::1");
        assert_eq!(deserialized.servers[2].host, "2001:db8::1");

        Ok(())
    }

    #[test]
    fn test_config_unicode_in_names() -> Result<()> {
        let config = Config {
            servers: vec![ServerConfig {
                host: "news.example.com".to_string(),
                port: 119,
                name: "测试服务器 🚀".to_string(),
                username: None,
                password: None,
                max_connections: 5,
                use_tls: false,
                tls_verify_cert: default_tls_verify_cert(),
                tls_cert_path: None,
            }],
            ..Default::default()
        };

        let toml_string = toml::to_string_pretty(&config)?;
        let deserialized: Config = toml::from_str(&toml_string)?;

        assert_eq!(deserialized.servers[0].name, "测试服务器 🚀");

        Ok(())
    }

    // Test helper functions to encapsulate unsafe env var operations
    // SAFETY: These are only safe when tests are run serially (not in parallel)
    // Use #[serial] attribute to ensure thread-safety
    #[cfg(test)]
    mod test_env_helpers {
        /// Set an environment variable (test helper)
        /// SAFETY: Only safe when called from #[serial] tests to avoid race conditions
        pub fn set_env(key: &str, value: &str) {
            unsafe {
                std::env::set_var(key, value);
            }
        }

        /// Remove an environment variable (test helper)
        /// SAFETY: Only safe when called from #[serial] tests to avoid race conditions
        pub fn remove_env(key: &str) {
            unsafe {
                std::env::remove_var(key);
            }
        }

        /// Remove multiple environment variables (test helper)
        /// SAFETY: Only safe when called from #[serial] tests to avoid race conditions
        pub fn remove_env_range(prefix: &str, start: usize, end: usize) {
            unsafe {
                for i in start..end {
                    std::env::remove_var(format!("{}_{}", prefix, i));
                }
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_load_servers_from_env() {
        use test_env_helpers::*;

        // Set up environment variables for testing
        set_env("NNTP_SERVER_0_HOST", "env-server1.com");
        set_env("NNTP_SERVER_0_PORT", "8119");
        set_env("NNTP_SERVER_0_NAME", "Env Server 1");
        set_env("NNTP_SERVER_0_USERNAME", "envuser");
        set_env("NNTP_SERVER_0_PASSWORD", "envpass");
        set_env("NNTP_SERVER_0_MAX_CONNECTIONS", "15");

        set_env("NNTP_SERVER_1_HOST", "env-server2.com");
        set_env("NNTP_SERVER_1_PORT", "119");
        // No name set - should use default "Server 1"
        // No max_connections set - should use default 10

        let servers = load_servers_from_env().expect("Should load servers from env");

        assert_eq!(servers.len(), 2);

        // Check first server
        assert_eq!(servers[0].host, "env-server1.com");
        assert_eq!(servers[0].port, 8119);
        assert_eq!(servers[0].name, "Env Server 1");
        assert_eq!(servers[0].username, Some("envuser".to_string()));
        assert_eq!(servers[0].password, Some("envpass".to_string()));
        assert_eq!(servers[0].max_connections, 15);

        // Check second server
        assert_eq!(servers[1].host, "env-server2.com");
        assert_eq!(servers[1].port, 119);
        assert_eq!(servers[1].name, "Server 1"); // Default name
        assert_eq!(servers[1].username, None);
        assert_eq!(servers[1].password, None);
        assert_eq!(servers[1].max_connections, 10); // Default

        // Clean up environment variables
        remove_env("NNTP_SERVER_0_HOST");
        remove_env("NNTP_SERVER_0_PORT");
        remove_env("NNTP_SERVER_0_NAME");
        remove_env("NNTP_SERVER_0_USERNAME");
        remove_env("NNTP_SERVER_0_PASSWORD");
        remove_env("NNTP_SERVER_0_MAX_CONNECTIONS");
        remove_env("NNTP_SERVER_1_HOST");
        remove_env("NNTP_SERVER_1_PORT");
    }

    #[test]
    #[serial_test::serial]
    fn test_load_servers_from_env_empty() {
        use test_env_helpers::*;

        // Ensure no env vars are set
        remove_env_range("NNTP_SERVER", 0, 5);

        let servers = load_servers_from_env();
        assert!(servers.is_none(), "Should return None when no env vars set");
    }
}
