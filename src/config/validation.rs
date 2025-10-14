//! Configuration validation
//!
//! This module provides validation logic for the configuration to ensure
//! all settings are valid before the proxy starts.

use anyhow::Result;
use std::time::Duration;

use super::types::{Config, ServerConfig};
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
fn validate_server(server: &ServerConfig) -> Result<()> {
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
