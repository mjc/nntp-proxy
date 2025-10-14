//! Configuration validation
//!
//! This module provides validation logic for the configuration to ensure
//! all settings are valid before the proxy starts.

use anyhow::Result;

use super::types::{CacheConfig, Config, HealthCheckConfig, ServerConfig};
use crate::constants::pool::{MAX_RECOMMENDED_KEEPALIVE_SECS, MIN_RECOMMENDED_KEEPALIVE_SECS};

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
            validate_server(server)?;
        }

        validate_health_check(&self.health_check)?;

        if let Some(cache) = &self.cache {
            validate_cache(cache)?;
        }

        Ok(())
    }
}

/// Validate a single server configuration
fn validate_server(server: &ServerConfig) -> Result<()> {
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

    // Warn if connection_keepalive_secs is outside recommended range
    if server.connection_keepalive_secs > 0 {
        if server.connection_keepalive_secs < MIN_RECOMMENDED_KEEPALIVE_SECS {
            tracing::warn!(
                "Server '{}' has connection_keepalive_secs set to {} seconds (< {} seconds). \
                 This may cause excessive health check traffic and connection churn. \
                 Consider using at least {} seconds or 0 to disable.",
                server.name,
                server.connection_keepalive_secs,
                MIN_RECOMMENDED_KEEPALIVE_SECS,
                MIN_RECOMMENDED_KEEPALIVE_SECS
            );
        } else if server.connection_keepalive_secs > MAX_RECOMMENDED_KEEPALIVE_SECS {
            tracing::warn!(
                "Server '{}' has connection_keepalive_secs set to {} seconds (> {} seconds / 5 minutes). \
                 This may not detect stale connections quickly enough. Consider a lower value.",
                server.name,
                server.connection_keepalive_secs,
                MAX_RECOMMENDED_KEEPALIVE_SECS
            );
        }
    }

    Ok(())
}

/// Validate health check configuration
fn validate_health_check(health_check: &HealthCheckConfig) -> Result<()> {
    if health_check.interval_secs == 0 {
        return Err(anyhow::anyhow!("health_check.interval_secs must be > 0"));
    }
    if health_check.timeout_secs == 0 {
        return Err(anyhow::anyhow!("health_check.timeout_secs must be > 0"));
    }
    if health_check.unhealthy_threshold == 0 {
        return Err(anyhow::anyhow!(
            "health_check.unhealthy_threshold must be > 0"
        ));
    }
    Ok(())
}

/// Validate cache configuration
fn validate_cache(cache: &CacheConfig) -> Result<()> {
    if cache.max_capacity == 0 {
        return Err(anyhow::anyhow!("cache.max_capacity must be > 0"));
    }
    if cache.ttl_secs == 0 {
        return Err(anyhow::anyhow!("cache.ttl_secs must be > 0"));
    }
    Ok(())
}
