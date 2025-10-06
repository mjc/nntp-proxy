//! Configuration module
//!
//! This module handles all configuration types and loading
//! for the NNTP proxy server.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// Default maximum connections per server
fn default_max_connections() -> u32 {
    10
}

/// Main proxy configuration
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
pub struct Config {
    /// List of backend NNTP servers
    pub servers: Vec<ServerConfig>,
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
}

/// Load configuration from a TOML file
pub fn load_config(config_path: &str) -> Result<Config> {
    let config_content = std::fs::read_to_string(config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read config file '{}': {}", config_path, e))?;

    let config: Config = toml::from_str(&config_content)
        .map_err(|e| anyhow::anyhow!("Failed to parse config file '{}': {}", config_path, e))?;

    Ok(config)
}

/// Create a default configuration for examples/testing
pub fn create_default_config() -> Config {
    Config {
        servers: vec![ServerConfig {
            host: "news.example.com".to_string(),
            port: 119,
            name: "Example News Server".to_string(),
            username: None,
            password: None,
            max_connections: default_max_connections(),
        }],
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
                },
                ServerConfig {
                    host: "server2.example.com".to_string(),
                    port: 119,
                    name: "Test Server 2".to_string(),
                    username: None,
                    password: None,
                    max_connections: 8,
                },
            ],
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
}
