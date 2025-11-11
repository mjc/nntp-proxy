//! Configuration loading from files and environment variables
//!
//! This module handles loading configuration from TOML files and environment variables,
//! with environment variables taking precedence for Docker/container deployments.

use anyhow::Result;

use super::defaults;
use super::types::{Config, ServerConfig};

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
            .and_then(|m| m.parse::<usize>().ok())
            .and_then(crate::types::MaxConnections::new)
            .unwrap_or_else(defaults::max_connections);

        // TLS configuration
        let use_tls_key = format!("NNTP_SERVER_{}_USE_TLS", index);
        let use_tls = std::env::var(&use_tls_key)
            .ok()
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or(false);

        let tls_verify_key = format!("NNTP_SERVER_{}_TLS_VERIFY_CERT", index);
        let tls_verify_cert = std::env::var(&tls_verify_key)
            .ok()
            .and_then(|v| v.parse::<bool>().ok())
            .unwrap_or_else(defaults::tls_verify_cert);

        let tls_cert_path_key = format!("NNTP_SERVER_{}_TLS_CERT_PATH", index);
        let tls_cert_path = std::env::var(&tls_cert_path_key).ok();

        // Connection keepalive (in seconds)
        let keepalive_key = format!("NNTP_SERVER_{}_CONNECTION_KEEPALIVE", index);
        let connection_keepalive = std::env::var(&keepalive_key)
            .ok()
            .and_then(|k| k.parse::<u64>().ok())
            .map(std::time::Duration::from_secs);

        // Health check configuration
        let health_max_key = format!("NNTP_SERVER_{}_HEALTH_CHECK_MAX_PER_CYCLE", index);
        let health_check_max_per_cycle = std::env::var(&health_max_key)
            .ok()
            .and_then(|h| h.parse::<usize>().ok())
            .unwrap_or_else(defaults::health_check_max_per_cycle);

        let health_timeout_key = format!("NNTP_SERVER_{}_HEALTH_CHECK_POOL_TIMEOUT", index);
        let health_check_pool_timeout = std::env::var(&health_timeout_key)
            .ok()
            .and_then(|h| h.parse::<u64>().ok())
            .map(std::time::Duration::from_secs)
            .unwrap_or_else(defaults::health_check_pool_timeout);

        servers.push(ServerConfig {
            host: crate::types::HostName::new(host.clone())
                .unwrap_or_else(|_| panic!("Invalid hostname in {}: '{}'", host_key, host)),
            port: crate::types::Port::new(port)
                .unwrap_or_else(|| panic!("Invalid port in {}: {}", port_key, port)),
            name: crate::types::ServerName::new(name.clone())
                .unwrap_or_else(|_| panic!("Invalid server name in {}: '{}'", name_key, name)),
            username,
            password,
            max_connections,
            use_tls,
            tls_verify_cert,
            tls_cert_path,
            connection_keepalive,
            health_check_max_per_cycle,
            health_check_pool_timeout,
        });

        index += 1;
    }

    if servers.is_empty() {
        None
    } else {
        Some(servers)
    }
}

/// Check if any backend server environment variables are set
///
/// Returns true if at least NNTP_SERVER_0_HOST is set
pub fn has_server_env_vars() -> bool {
    std::env::var("NNTP_SERVER_0_HOST").is_ok()
}

/// Load configuration from environment variables only
///
/// Used when no config file is present. Requires at least NNTP_SERVER_0_HOST to be set.
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
pub fn load_config(config_path: &str) -> Result<Config> {
    use anyhow::Context;

    let config_content = std::fs::read_to_string(config_path)
        .with_context(|| format!("Failed to read config file '{}'", config_path))?;

    let mut config: Config = toml::from_str(&config_content)
        .with_context(|| format!("Failed to parse config file '{}'", config_path))?;

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
            host: crate::types::HostName::new("news.example.com".to_string())
                .expect("Valid hostname"),
            port: crate::types::Port::new(119).expect("Valid port"),
            name: crate::types::ServerName::new("Example News Server".to_string())
                .expect("Valid server name"),
            username: None,
            password: None,
            max_connections: defaults::max_connections(),
            use_tls: false,
            tls_verify_cert: defaults::tls_verify_cert(),
            tls_cert_path: None,
            connection_keepalive: None,
            health_check_max_per_cycle: defaults::health_check_max_per_cycle(),
            health_check_pool_timeout: defaults::health_check_pool_timeout(),
        }],
        ..Default::default()
    }
}
