//! Configuration loading from files and environment variables
//!
//! This module handles loading configuration from TOML files and environment variables,
//! with environment variables taking precedence for Docker/container deployments.

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use std::fs;
use std::path::{Path, PathBuf};

use super::defaults;
use super::types::{Config, Server};

type TomlTable = toml::map::Map<String, toml::Value>;

fn table<'a>(value: &'a toml::Value, key: &str) -> Option<&'a TomlTable> {
    value.get(key)?.as_table()
}

fn table_has_key(value: &toml::Value, section: &str, key: &str) -> bool {
    table(value, section).is_some_and(|t| t.contains_key(key))
}

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn write_canonical_config(config_path: &str, config: &Config) -> Result<()> {
    let canonical =
        toml::to_string_pretty(config).context("Failed to serialize migrated config")?;
    let path = Path::new(config_path);
    let backup_path = path_with_suffix(path, ".bak");

    fs::copy(path, &backup_path)
        .with_context(|| format!("Failed to create backup config '{}'", backup_path.display()))?;

    let tmp_path = path_with_suffix(path, ".tmp");
    fs::write(&tmp_path, canonical).with_context(|| {
        format!(
            "Failed to write migrated config to temporary file '{}'",
            tmp_path.display()
        )
    })?;
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "Failed to atomically replace config '{}' with migrated schema",
            path.display()
        )
    })?;

    Ok(())
}

fn assign_from_table<T>(target: &mut T, source: &TomlTable, key: &str) -> bool
where
    T: DeserializeOwned,
{
    let Some(value) = source
        .get(key)
        .and_then(|value| value.clone().try_into::<T>().ok())
    else {
        return false;
    };

    *target = value;
    true
}

fn migrate_proxy_routing(config: &mut Config, raw: &toml::Value) -> bool {
    let Some(proxy) = table(raw, "proxy") else {
        return false;
    };

    if raw.get("routing").is_some() {
        return false;
    }

    let mut migrated = false;
    migrated |= assign_from_table(&mut config.routing.routing_mode, proxy, "routing_mode");
    migrated |= assign_from_table(
        &mut config.routing.backend_selection,
        proxy,
        "backend_selection",
    );
    migrated
}

fn migrate_proxy_memory(config: &mut Config, raw: &toml::Value) -> bool {
    let Some(proxy) = table(raw, "proxy") else {
        return false;
    };

    if raw.get("memory").is_some() {
        return false;
    }

    let mut migrated = false;
    migrated |= assign_from_table(
        &mut config.memory.buffer_pool_count,
        proxy,
        "buffer_pool_count",
    );
    migrated |= assign_from_table(
        &mut config.memory.capture_pool_count,
        proxy,
        "capture_pool_count",
    );
    migrated
}

fn migrate_cache_precheck(config: &mut Config, raw: &toml::Value) -> bool {
    if !table_has_key(raw, "routing", "adaptive_precheck")
        && let Some(cache) = table(raw, "cache")
        && assign_from_table(
            &mut config.routing.adaptive_precheck,
            cache,
            "adaptive_precheck",
        )
    {
        return true;
    }

    false
}

fn migrate_legacy_config(config: &mut Config, raw: &toml::Value) -> bool {
    migrate_proxy_routing(config, raw)
        | migrate_proxy_memory(config, raw)
        | migrate_cache_precheck(config, raw)
}

/// Environment variable getter trait for dependency injection
pub trait EnvProvider {
    fn get(&self, key: &str) -> Option<String>;
}

/// Standard environment provider using `std::env::var`
#[derive(Default)]
pub struct StdEnvProvider;

impl EnvProvider for StdEnvProvider {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }
}

/// Parse server configuration from environment variables (pure function, easily testable)
///
/// # Arguments
/// * `index` - Server index (0, 1, 2, ...)
/// * `env` - Environment variable provider
///
/// # Returns
/// Some(Server) if HOST variable exists, None otherwise
pub fn parse_server_from_env<E: EnvProvider>(index: usize, env: &E) -> Option<Server> {
    // Check if this server index exists by looking for HOST
    let host_key = format!("NNTP_SERVER_{index}_HOST");
    let host = env.get(&host_key)?;

    // Parse port (required)
    let port_key = format!("NNTP_SERVER_{index}_PORT");
    let port = env
        .get(&port_key)
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(119); // Default NNTP port

    // Get name (required, use host as fallback)
    let name_key = format!("NNTP_SERVER_{index}_NAME");
    let name = env
        .get(&name_key)
        .unwrap_or_else(|| format!("Server {index}"));

    // Optional fields
    let username_key = format!("NNTP_SERVER_{index}_USERNAME");
    let username = env.get(&username_key);

    let password_key = format!("NNTP_SERVER_{index}_PASSWORD");
    let password = env.get(&password_key);

    let max_conn_key = format!("NNTP_SERVER_{index}_MAX_CONNECTIONS");
    let max_connections = env
        .get(&max_conn_key)
        .and_then(|m| m.parse::<usize>().ok())
        .and_then(|m| crate::types::MaxConnections::try_new(m).ok())
        .unwrap_or_else(defaults::max_connections);

    // TLS configuration
    let use_tls_key = format!("NNTP_SERVER_{index}_USE_TLS");
    let use_tls = env
        .get(&use_tls_key)
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or(false);

    let tls_verify_key = format!("NNTP_SERVER_{index}_TLS_VERIFY_CERT");
    let tls_verify_cert = env
        .get(&tls_verify_key)
        .and_then(|v| v.parse::<bool>().ok())
        .unwrap_or_else(defaults::tls_verify_cert);

    let tls_cert_path_key = format!("NNTP_SERVER_{index}_TLS_CERT_PATH");
    let tls_cert_path = env.get(&tls_cert_path_key);

    // Connection keepalive (in seconds)
    let keepalive_key = format!("NNTP_SERVER_{index}_CONNECTION_KEEPALIVE");
    let connection_keepalive = env
        .get(&keepalive_key)
        .and_then(|k| k.parse::<u64>().ok())
        .map(std::time::Duration::from_secs);

    // Health check configuration
    let health_max_key = format!("NNTP_SERVER_{index}_HEALTH_CHECK_MAX_PER_CYCLE");
    let health_check_max_per_cycle = env
        .get(&health_max_key)
        .and_then(|h| h.parse::<usize>().ok())
        .unwrap_or_else(defaults::health_check_max_per_cycle);

    let health_timeout_key = format!("NNTP_SERVER_{index}_HEALTH_CHECK_POOL_TIMEOUT");
    let health_check_pool_timeout = env
        .get(&health_timeout_key)
        .and_then(|h| h.parse::<u64>().ok())
        .map_or_else(
            defaults::health_check_pool_timeout,
            std::time::Duration::from_secs,
        );

    let tier_key = format!("NNTP_SERVER_{index}_TIER");
    let tier = env.get(&tier_key).map_or(0, |tier_str| {
        tier_str
            .parse::<u8>()
            .unwrap_or_else(|_| panic!("Invalid tier in {tier_key}: '{tier_str}' (must be 0-255)"))
    });

    Some(Server {
        host: crate::types::HostName::try_new(host.clone())
            .unwrap_or_else(|_| panic!("Invalid hostname in {host_key}: '{host}'")),
        port: crate::types::Port::try_new(port)
            .unwrap_or_else(|_| panic!("Invalid port in {port_key}: {port}")),
        name: crate::types::ServerName::try_new(name.clone())
            .unwrap_or_else(|_| panic!("Invalid server name in {name_key}: '{name}'")),
        username,
        password,
        max_connections,
        use_tls,
        tls_verify_cert,
        tls_cert_path,
        connection_keepalive,
        replacement_cooldown: crate::config::defaults::replacement_cooldown_option(),
        health_check_max_per_cycle,
        health_check_pool_timeout,
        tier,
        compress: None,
        compress_level: None,
        backend_idle_timeout: crate::config::defaults::backend_idle_timeout(),
        backend_pipelining: crate::config::defaults::enable_pipelining(),
        pipeline_queue_depth: crate::config::defaults::pipeline_queue_depth(),
        pipeline_batch_size: crate::config::defaults::pipeline_batch_size(),
    })
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
fn load_servers_from_env() -> Option<Vec<Server>> {
    load_servers_from_env_provider(&StdEnvProvider)
}

/// Load servers using a custom environment provider (testable version)
pub fn load_servers_from_env_provider<E: EnvProvider>(env: &E) -> Option<Vec<Server>> {
    let servers: Vec<Server> = (0..)
        .map_while(|index| parse_server_from_env(index, env))
        .collect();

    if servers.is_empty() {
        None
    } else {
        Some(servers)
    }
}

/// Check if any backend server environment variables are set
///
/// Returns true if at least `NNTP_SERVER_0_HOST` is set
#[must_use]
pub fn has_server_env_vars() -> bool {
    std::env::var("NNTP_SERVER_0_HOST").is_ok()
}

/// Load configuration from environment variables only
///
/// Used when no config file is present. Requires at least `NNTP_SERVER_0_HOST` to be set.
///
/// # Errors
///
/// Returns an error if no backend servers are configured via environment variables.
pub fn load_config_from_env() -> Result<Config> {
    use anyhow::Context;

    let servers = load_servers_from_env()
        .context("No backend servers configured via environment variables. Set NNTP_SERVER_0_HOST, NNTP_SERVER_0_PORT, etc.")?;

    let config = Config {
        servers,
        ..Default::default()
    };

    // Validate the loaded configuration
    config.validate()?;

    Ok(config)
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
///
/// # Errors
/// Returns read/parsing or validation errors encountered while loading the
/// config. Legacy schema write-back failures are logged but do not prevent
/// startup because the migrated in-memory config can still be used.
pub fn load_config(config_path: &str) -> Result<Config> {
    load_config_with_env_provider(config_path, &StdEnvProvider)
}

fn load_config_with_env_provider<E: EnvProvider>(config_path: &str, env: &E) -> Result<Config> {
    use anyhow::Context;

    let config_content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read config file '{config_path}'"))?;

    let raw_config: toml::Value = toml::from_str(&config_content)
        .with_context(|| format!("Failed to parse config file '{config_path}'"))?;

    let mut config: Config = toml::from_str(&config_content)
        .with_context(|| format!("Failed to parse config file '{config_path}'"))?;

    let mut file_config = config.clone();
    let migrated = migrate_legacy_config(&mut file_config, &raw_config);

    if migrated {
        tracing::info!(
            "Migrating legacy configuration schema in '{}' to the new routing/cache/memory layout",
            config_path
        );
        config = file_config;
        if let Err(error) = write_canonical_config(config_path, &config) {
            tracing::warn!(
                "Failed to write migrated config schema for '{}': {:#}. Continuing with migrated in-memory config.",
                config_path,
                error
            );
        }
    }

    // Check for environment variable server overrides after migration so
    // canonical file rewrites never discard container-provided backends.
    if let Some(env_servers) = load_servers_from_env_provider(env) {
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

/// Configuration source
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    /// Loaded from TOML file
    File,
    /// Loaded from environment variables
    Environment,
    /// Default config created (file doesn't exist)
    DefaultCreated,
}

impl ConfigSource {
    /// Get a human-readable description
    #[must_use]
    pub const fn description(&self) -> &'static str {
        match self {
            Self::File => "configuration file",
            Self::Environment => "environment variables",
            Self::DefaultCreated => "default configuration (created)",
        }
    }
}

/// Load configuration with automatic fallback logic
///
/// Attempts to load configuration in this order:
/// 1. If config file exists, load from file (with env var overrides)
/// 2. Else if environment variables exist (`NNTP_SERVER_*`), load from env
/// 3. Else create default config file and return default config
///
/// # Arguments
/// * `config_path` - Path to configuration file
///
/// # Returns
/// Tuple of (Config, `ConfigSource`) indicating where config came from
///
/// # Errors
/// Returns error if:
/// - Config file exists but can't be read or parsed
/// - Environment variables exist but are invalid
/// - Default config can't be created
pub fn load_config_with_fallback(config_path: &str) -> Result<(Config, ConfigSource)> {
    use anyhow::Context;

    // Check if config file exists
    if std::path::Path::new(config_path).exists() {
        match load_config(config_path) {
            Ok(config) => {
                tracing::info!("Loaded configuration from file: {}", config_path);
                return Ok((config, ConfigSource::File));
            }
            Err(e) => {
                tracing::error!(
                    "Failed to load existing config file '{}': {}",
                    config_path,
                    e
                );
                tracing::error!("Please check your config file syntax and try again");
                return Err(e);
            }
        }
    }

    // Config file doesn't exist - check for environment variables
    if has_server_env_vars() {
        match load_config_from_env() {
            Ok(config) => {
                tracing::info!(
                    "Using configuration from environment variables (no config file found)"
                );
                return Ok((config, ConfigSource::Environment));
            }
            Err(e) => {
                tracing::error!(
                    "Failed to load configuration from environment variables: {}",
                    e
                );
                return Err(e);
            }
        }
    }

    // No config file and no env vars - create default
    tracing::warn!(
        "Config file '{}' not found and no NNTP_SERVER_* environment variables set",
        config_path
    );
    tracing::warn!("Creating default config file - please edit it to add your backend servers");

    let default_config = create_default_config();
    let config_toml =
        toml::to_string_pretty(&default_config).context("Failed to serialize default config")?;

    std::fs::write(config_path, &config_toml)
        .with_context(|| format!("Failed to write default config to '{config_path}'"))?;

    tracing::info!("Created default config file: {}", config_path);
    Ok((default_config, ConfigSource::DefaultCreated))
}

/// Create a default configuration for examples/testing
#[must_use]
///
/// # Panics
/// Panics only if the hard-coded example hostname, port, or server name stop
/// satisfying their validated newtype constructors.
pub fn create_default_config() -> Config {
    Config {
        servers: vec![Server {
            host: crate::types::HostName::try_new("news.example.com".to_string())
                .expect("Valid hostname"),
            port: crate::types::Port::try_new(119).expect("Valid port"),
            name: crate::types::ServerName::try_new("Example News Server".to_string())
                .expect("Valid server name"),
            username: None,
            password: None,
            max_connections: defaults::max_connections(),
            use_tls: false,
            tls_verify_cert: defaults::tls_verify_cert(),
            tls_cert_path: None,
            connection_keepalive: None,
            replacement_cooldown: defaults::replacement_cooldown_option(),
            health_check_max_per_cycle: defaults::health_check_max_per_cycle(),
            health_check_pool_timeout: defaults::health_check_pool_timeout(),
            tier: 0,
            compress: None,
            compress_level: None,
            backend_idle_timeout: defaults::backend_idle_timeout(),
            backend_pipelining: defaults::enable_pipelining(),
            pipeline_queue_depth: defaults::pipeline_queue_depth(),
            pipeline_batch_size: defaults::pipeline_batch_size(),
        }],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{BackendSelectionStrategy, RoutingMode};
    use std::collections::HashMap;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // Mock environment provider for testing
    struct MockEnv {
        vars: HashMap<String, String>,
    }

    impl MockEnv {
        fn new() -> Self {
            Self {
                vars: HashMap::new(),
            }
        }

        fn set(&mut self, key: impl Into<String>, value: impl Into<String>) -> &mut Self {
            self.vars.insert(key.into(), value.into());
            self
        }
    }

    impl EnvProvider for MockEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.vars.get(key).cloned()
        }
    }

    #[test]
    fn test_parse_server_from_env_minimal() {
        let mut env = MockEnv::new();
        env.set("NNTP_SERVER_0_HOST", "news.example.com");

        let server = parse_server_from_env(0, &env);
        assert!(server.is_some());

        let server = server.unwrap();
        assert_eq!(server.host.as_str(), "news.example.com");
        assert_eq!(server.port.get(), 119); // Default port
        assert_eq!(server.name.as_str(), "Server 0"); // Default name
        assert!(server.username.is_none());
        assert!(server.password.is_none());
    }

    #[test]
    fn test_parse_server_from_env_full() {
        let mut env = MockEnv::new();
        env.set("NNTP_SERVER_0_HOST", "secure.example.com")
            .set("NNTP_SERVER_0_PORT", "563")
            .set("NNTP_SERVER_0_NAME", "Secure News")
            .set("NNTP_SERVER_0_USERNAME", "testuser")
            .set("NNTP_SERVER_0_PASSWORD", "testpass")
            .set("NNTP_SERVER_0_MAX_CONNECTIONS", "20")
            .set("NNTP_SERVER_0_USE_TLS", "true")
            .set("NNTP_SERVER_0_TLS_VERIFY_CERT", "false");

        let server = parse_server_from_env(0, &env).unwrap();
        assert_eq!(server.host.as_str(), "secure.example.com");
        assert_eq!(server.port.get(), 563);
        assert_eq!(server.name.as_str(), "Secure News");
        assert_eq!(server.username, Some("testuser".to_string()));
        assert_eq!(server.password, Some("testpass".to_string()));
        assert_eq!(server.max_connections.get(), 20);
        assert!(server.use_tls);
        assert!(!server.tls_verify_cert);
    }

    #[test]
    fn test_parse_server_from_env_no_host() {
        let env = MockEnv::new();
        let server = parse_server_from_env(0, &env);
        assert!(server.is_none());
    }

    #[test]
    fn test_parse_server_from_env_invalid_port() {
        let mut env = MockEnv::new();
        env.set("NNTP_SERVER_0_HOST", "news.example.com")
            .set("NNTP_SERVER_0_PORT", "invalid");

        let server = parse_server_from_env(0, &env).unwrap();
        assert_eq!(server.port.get(), 119); // Falls back to default
    }

    #[test]
    fn test_parse_server_from_env_invalid_max_connections() {
        let mut env = MockEnv::new();
        env.set("NNTP_SERVER_0_HOST", "news.example.com")
            .set("NNTP_SERVER_0_MAX_CONNECTIONS", "not_a_number");

        let server = parse_server_from_env(0, &env).unwrap();
        assert_eq!(server.max_connections.get(), 10); // Default
    }

    #[test]
    fn test_parse_server_from_env_zero_max_connections() {
        let mut env = MockEnv::new();
        env.set("NNTP_SERVER_0_HOST", "news.example.com")
            .set("NNTP_SERVER_0_MAX_CONNECTIONS", "0");

        let server = parse_server_from_env(0, &env).unwrap();
        assert_eq!(server.max_connections.get(), 10); // Falls back to default (NonZero rejects 0)
    }

    #[test]
    fn test_parse_server_from_env_keepalive() {
        let mut env = MockEnv::new();
        env.set("NNTP_SERVER_0_HOST", "news.example.com")
            .set("NNTP_SERVER_0_CONNECTION_KEEPALIVE", "300");

        let server = parse_server_from_env(0, &env).unwrap();
        assert_eq!(
            server.connection_keepalive,
            Some(std::time::Duration::from_mins(5))
        );
    }

    #[test]
    fn test_parse_server_from_env_health_check_config() {
        let mut env = MockEnv::new();
        env.set("NNTP_SERVER_0_HOST", "news.example.com")
            .set("NNTP_SERVER_0_HEALTH_CHECK_MAX_PER_CYCLE", "5")
            .set("NNTP_SERVER_0_HEALTH_CHECK_POOL_TIMEOUT", "15");

        let server = parse_server_from_env(0, &env).unwrap();
        assert_eq!(server.health_check_max_per_cycle, 5);
        assert_eq!(
            server.health_check_pool_timeout,
            std::time::Duration::from_secs(15)
        );
    }

    #[test]
    fn test_parse_server_from_env_tls_cert_path() {
        let mut env = MockEnv::new();
        env.set("NNTP_SERVER_0_HOST", "news.example.com")
            .set("NNTP_SERVER_0_USE_TLS", "true")
            .set("NNTP_SERVER_0_TLS_CERT_PATH", "/path/to/ca.pem");

        let server = parse_server_from_env(0, &env).unwrap();
        assert!(server.use_tls);
        assert_eq!(server.tls_cert_path, Some("/path/to/ca.pem".to_string()));
    }

    #[test]
    fn test_load_servers_from_env_provider_empty() {
        let env = MockEnv::new();
        let servers = load_servers_from_env_provider(&env);
        assert!(servers.is_none());
    }

    #[test]
    fn test_load_servers_from_env_provider_single() {
        let mut env = MockEnv::new();
        env.set("NNTP_SERVER_0_HOST", "news1.example.com");

        let servers = load_servers_from_env_provider(&env);
        assert!(servers.is_some());

        let servers = servers.unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].host.as_str(), "news1.example.com");
    }

    #[test]
    fn test_load_servers_from_env_provider_multiple() {
        let mut env = MockEnv::new();
        env.set("NNTP_SERVER_0_HOST", "news1.example.com")
            .set("NNTP_SERVER_0_PORT", "119")
            .set("NNTP_SERVER_1_HOST", "news2.example.com")
            .set("NNTP_SERVER_1_PORT", "563")
            .set("NNTP_SERVER_1_USE_TLS", "true")
            .set("NNTP_SERVER_2_HOST", "news3.example.com");

        let servers = load_servers_from_env_provider(&env);
        assert!(servers.is_some());

        let servers = servers.unwrap();
        assert_eq!(servers.len(), 3);
        assert_eq!(servers[0].host.as_str(), "news1.example.com");
        assert_eq!(servers[1].host.as_str(), "news2.example.com");
        assert_eq!(servers[2].host.as_str(), "news3.example.com");
        assert!(servers[1].use_tls);
        assert!(!servers[0].use_tls);
    }

    #[test]
    fn test_load_servers_from_env_provider_gaps() {
        let mut env = MockEnv::new();
        // Server 0 and 2 defined, but not 1 - should stop at 1
        env.set("NNTP_SERVER_0_HOST", "news1.example.com")
            .set("NNTP_SERVER_2_HOST", "news3.example.com");

        let servers = load_servers_from_env_provider(&env);
        assert!(servers.is_some());

        let servers = servers.unwrap();
        // Should only get server 0, stops at first gap
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].host.as_str(), "news1.example.com");
    }

    #[test]
    fn test_parse_server_from_env_bool_variations() {
        let mut env = MockEnv::new();
        env.set("NNTP_SERVER_0_HOST", "news.example.com")
            .set("NNTP_SERVER_0_USE_TLS", "True")
            .set("NNTP_SERVER_0_TLS_VERIFY_CERT", "FALSE");

        let server = parse_server_from_env(0, &env).unwrap();
        // Rust's parse::<bool>() requires exact "true"/"false" lowercase
        // So these should fail to parse and use defaults
        assert!(!server.use_tls); // Defaults to false
        assert!(server.tls_verify_cert); // Defaults to true
    }

    #[test]
    fn test_parse_server_from_env_correct_bool() {
        let mut env = MockEnv::new();
        env.set("NNTP_SERVER_0_HOST", "news.example.com")
            .set("NNTP_SERVER_0_USE_TLS", "true")
            .set("NNTP_SERVER_0_TLS_VERIFY_CERT", "false");

        let server = parse_server_from_env(0, &env).unwrap();
        assert!(server.use_tls);
        assert!(!server.tls_verify_cert);
    }

    #[test]
    fn test_config_source_description() {
        assert_eq!(ConfigSource::File.description(), "configuration file");
        assert_eq!(
            ConfigSource::Environment.description(),
            "environment variables"
        );
        assert_eq!(
            ConfigSource::DefaultCreated.description(),
            "default configuration (created)"
        );
    }

    #[test]
    fn test_config_source_equality() {
        assert_eq!(ConfigSource::File, ConfigSource::File);
        assert_ne!(ConfigSource::File, ConfigSource::Environment);
        assert_ne!(ConfigSource::Environment, ConfigSource::DefaultCreated);
    }

    #[test]
    fn test_load_config_with_fallback_creates_default() {
        use tempfile::NamedTempFile;

        let temp_file = NamedTempFile::new().unwrap();
        let path = temp_file.path().to_str().unwrap().to_string();

        // Remove the temp file so it doesn't exist
        drop(temp_file);

        // Should create default config
        let result = load_config_with_fallback(&path);
        assert!(result.is_ok());

        let (config, source) = result.unwrap();
        assert_eq!(source, ConfigSource::DefaultCreated);
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].host.as_str(), "news.example.com");

        // Cleanup
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_config_with_fallback_reads_existing() {
        use std::io::Write;
        use tempfile::NamedTempFile;

        let mut temp_file = NamedTempFile::new().unwrap();

        // Write a valid config
        let config_content = r#"
[[servers]]
host = "test.example.com"
port = 119
name = "Test Server"
"#;
        temp_file.write_all(config_content.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        // Get path as owned string before borrowing for read
        let path = temp_file.path().to_str().unwrap().to_string();

        let result = load_config_with_fallback(&path);
        assert!(result.is_ok());

        let (config, source) = result.unwrap();
        assert_eq!(source, ConfigSource::File);
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].host.as_str(), "test.example.com");
    }

    #[test]
    fn test_create_default_config() {
        let config = create_default_config();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].host.as_str(), "news.example.com");
        assert_eq!(config.servers[0].port.get(), 119);
        assert!(!config.servers[0].use_tls);
    }

    #[test]
    fn test_load_config_migrates_legacy_schema() {
        let mut temp_file = NamedTempFile::new().unwrap();

        let config_content = r#"
[[servers]]
host = "legacy.example.com"
port = 119
name = "Legacy Server"

[proxy]
routing_mode = "per-command"
backend_selection = "least-loaded"
buffer_pool_count = 64
capture_pool_count = 32

[cache]
max_capacity = "256mib"
ttl = 1800
cache_articles = true
adaptive_precheck = true
"#;
        temp_file.write_all(config_content.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let path = temp_file.path().to_str().unwrap().to_string();
        let config = load_config(&path).unwrap();

        assert_eq!(config.routing.routing_mode, RoutingMode::PerCommand);
        assert_eq!(
            config.routing.backend_selection,
            BackendSelectionStrategy::LeastLoaded
        );
        assert_eq!(config.memory.buffer_pool_count, 64);
        assert_eq!(config.memory.capture_pool_count, 32);
        assert!(config.routing.adaptive_precheck);
        let cache = config.cache.as_ref().unwrap();
        assert_eq!(cache.article_cache_capacity.get(), 256 * 1024 * 1024);
        assert_eq!(cache.article_cache_ttl_secs.as_secs(), 1800);
        assert!(cache.store_article_bodies);

        let migrated = std::fs::read_to_string(&path).unwrap();
        assert!(migrated.contains("[routing]"));
        assert!(migrated.contains("[memory]"));
        assert!(migrated.contains("mode = \"per-command\""));
        assert!(migrated.contains("buffer_pool_count = 64"));
        assert!(migrated.contains("capture_pool_count = 32"));
        assert!(migrated.contains("article_cache_capacity = 268435456"));
        assert!(migrated.contains("article_cache_ttl_secs = 1800"));
        assert!(migrated.contains("store_article_bodies = true"));

        let proxy_offset = migrated.find("[proxy]").unwrap();
        let routing_offset = migrated.find("[routing]").unwrap();
        let memory_offset = migrated.find("[memory]").unwrap();
        let cache_offset = migrated.find("[cache]").unwrap();
        let health_check_offset = migrated.find("[health_check]").unwrap();
        let client_auth_offset = migrated.find("[client_auth]").unwrap();
        let servers_offset = migrated.find("[[servers]]").unwrap();

        assert!(proxy_offset < routing_offset);
        assert!(routing_offset < memory_offset);
        assert!(memory_offset < cache_offset);
        assert!(cache_offset < health_check_offset);
        assert!(health_check_offset < client_auth_offset);
        assert!(client_auth_offset < servers_offset);

        let backup_path = format!("{path}.bak");
        assert!(std::path::Path::new(&backup_path).exists());
    }

    #[test]
    fn test_load_config_preserves_env_server_overrides_after_migration() {
        let mut temp_file = NamedTempFile::new().unwrap();

        let config_content = r#"
[proxy]
routing_mode = "per-command"
backend_selection = "least-loaded"
buffer_pool_count = 64
capture_pool_count = 32
"#;
        temp_file.write_all(config_content.as_bytes()).unwrap();
        temp_file.flush().unwrap();

        let mut env = MockEnv::new();
        env.set("NNTP_SERVER_0_HOST", "env.example.com")
            .set("NNTP_SERVER_0_PORT", "563")
            .set("NNTP_SERVER_0_NAME", "Env Server")
            .set("NNTP_SERVER_0_USE_TLS", "true");

        let path = temp_file.path().to_str().unwrap().to_string();
        let config = load_config_with_env_provider(&path, &env).unwrap();

        assert_eq!(config.routing.routing_mode, RoutingMode::PerCommand);
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].host.as_str(), "env.example.com");
        assert_eq!(config.servers[0].port.get(), 563);
        assert_eq!(config.servers[0].name.as_str(), "Env Server");
        assert!(config.servers[0].use_tls);
    }

    #[test]
    fn test_load_config_uses_migrated_config_when_writeback_fails() {
        let temp_dir = tempfile::tempdir().unwrap();
        let path = temp_dir.path().join("config.toml");

        let config_content = r#"
[[servers]]
host = "legacy.example.com"
port = 119
name = "Legacy Server"

[proxy]
routing_mode = "per-command"
backend_selection = "least-loaded"
"#;
        std::fs::write(&path, config_content).unwrap();

        let backup_path = path_with_suffix(&path, ".bak");
        std::fs::create_dir(&backup_path).unwrap();

        let result = load_config_with_env_provider(path.to_str().unwrap(), &MockEnv::new());

        let config = result.unwrap();
        assert_eq!(config.routing.routing_mode, RoutingMode::PerCommand);
        assert_eq!(
            config.routing.backend_selection,
            BackendSelectionStrategy::LeastLoaded
        );
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].host.as_str(), "legacy.example.com");
        assert!(backup_path.is_dir());
    }
}
