//! Configuration validation
//!
//! This module provides validation logic for the configuration to ensure
//! all settings are valid before the proxy starts.

use anyhow::Result;
use std::time::Duration;

use super::types::{Config, Server};
use crate::constants::pool::{MAX_RECOMMENDED_KEEPALIVE_SECS, MIN_RECOMMENDED_KEEPALIVE_SECS};

const MIN_RECOMMENDED_KEEPALIVE: Duration = Duration::from_secs(MIN_RECOMMENDED_KEEPALIVE_SECS);
const MAX_RECOMMENDED_KEEPALIVE: Duration = Duration::from_secs(MAX_RECOMMENDED_KEEPALIVE_SECS);

impl Config {
    /// Validate configuration for correctness
    ///
    /// Most validations are now enforced by type system (NonZero types, validated strings, etc.)
    /// This checks remaining semantic constraints:
    /// - At least one server configured
    /// - Keep-alive intervals are in recommended ranges
    pub fn validate(&self) -> Result<()> {
        if self.servers.is_empty() {
            return Err(anyhow::anyhow!(
                "Configuration must have at least one server"
            ));
        }

        for server in &self.servers {
            validate_server(server)?;
        }

        Ok(())
    }
}

/// Validate a single server configuration
fn validate_server(server: &Server) -> Result<()> {
    // Name, host, port, max_connections validations now enforced by types:
    // - HostName/ServerName cannot be empty (validated at construction)
    // - Port cannot be 0 (NonZeroU16)
    // - max_connections cannot be 0 (NonZeroUsize via MaxConnections)

    // Warn if connection_keepalive is outside recommended range
    if let Some(keepalive) = server.connection_keepalive {
        if keepalive < MIN_RECOMMENDED_KEEPALIVE {
            tracing::warn!(
                "Server '{}' has connection_keepalive set to {:?} (< {:?}). \
                 This may cause excessive health check traffic and connection churn. \
                 Consider using at least {:?} or None to disable.",
                server.name.as_str(),
                keepalive,
                MIN_RECOMMENDED_KEEPALIVE,
                MIN_RECOMMENDED_KEEPALIVE
            );
        } else if keepalive > MAX_RECOMMENDED_KEEPALIVE {
            tracing::warn!(
                "Server '{}' has connection_keepalive set to {:?} (> {:?} / 5 minutes). \
                 This may not detect stale connections quickly enough. Consider a lower value.",
                server.name.as_str(),
                keepalive,
                MAX_RECOMMENDED_KEEPALIVE
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Port;

    fn create_test_server(name: &str, keepalive: Option<Duration>) -> Server {
        let mut builder = Server::builder("localhost", Port::try_new(119).unwrap()).name(name);

        if let Some(ka) = keepalive {
            builder = builder.connection_keepalive(ka);
        }

        builder.build().unwrap()
    }

    #[test]
    fn test_validate_empty_config_fails() {
        let config = Config {
            servers: vec![],
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_single_server_succeeds() {
        let config = Config {
            servers: vec![create_test_server("test", None)],
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_multiple_servers_succeeds() {
        let config = Config {
            servers: vec![
                create_test_server("server1", None),
                create_test_server("server2", None),
            ],
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_server_with_recommended_keepalive() {
        let server = create_test_server("test", Some(Duration::from_secs(60)));
        assert!(validate_server(&server).is_ok());
    }

    #[test]
    fn test_validate_server_with_low_keepalive_warns() {
        // This should warn but not fail
        let server = create_test_server("test", Some(Duration::from_secs(5)));
        assert!(validate_server(&server).is_ok());
    }

    #[test]
    fn test_validate_server_with_high_keepalive_warns() {
        // This should warn but not fail
        let server = create_test_server("test", Some(Duration::from_secs(600)));
        assert!(validate_server(&server).is_ok());
    }

    #[test]
    fn test_validate_server_with_no_keepalive() {
        let server = create_test_server("test", None);
        assert!(validate_server(&server).is_ok());
    }

    #[test]
    fn test_validate_server_at_min_boundary() {
        let server = create_test_server("test", Some(MIN_RECOMMENDED_KEEPALIVE));
        assert!(validate_server(&server).is_ok());
    }

    #[test]
    fn test_validate_server_at_max_boundary() {
        let server = create_test_server("test", Some(MAX_RECOMMENDED_KEEPALIVE));
        assert!(validate_server(&server).is_ok());
    }
}
