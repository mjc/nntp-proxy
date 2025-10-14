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
            .and_then(|m| m.parse::<u32>().ok())
            .unwrap_or_else(defaults::max_connections);

        servers.push(ServerConfig {
            host,
            port,
            name,
            username,
            password,
            max_connections,
            use_tls: false,
            tls_verify_cert: defaults::tls_verify_cert(),
            tls_cert_path: None,
            connection_keepalive_secs: 0,
            health_check_max_per_cycle: defaults::health_check_max_per_cycle(),
            health_check_pool_timeout_ms: defaults::health_check_pool_timeout_ms(),
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
            max_connections: defaults::max_connections(),
            use_tls: false,
            tls_verify_cert: defaults::tls_verify_cert(),
            tls_cert_path: None,
            connection_keepalive_secs: 0,
            health_check_max_per_cycle: defaults::health_check_max_per_cycle(),
            health_check_pool_timeout_ms: defaults::health_check_pool_timeout_ms(),
        }],
        ..Default::default()
    }
}
