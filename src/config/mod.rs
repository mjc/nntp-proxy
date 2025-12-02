//! Configuration module
//!
//! This module handles all configuration types and loading
//! for the NNTP proxy server.

mod defaults;
mod loading;
mod types;
mod validation;

// Re-export public types
pub use loading::{
    ConfigSource, create_default_config, has_server_env_vars, load_config, load_config_from_env,
    load_config_with_fallback,
};
pub use types::{
    BackendSelectionStrategy, Cache, ClientAuth, Config, HealthCheck, RoutingMode, Server,
};

// Re-export default functions for use in tests and other modules
pub use defaults::{
    cache_max_capacity, cache_ttl, health_check_interval, health_check_max_per_cycle,
    health_check_pool_timeout, health_check_timeout, max_connections, tls_verify_cert,
    unhealthy_threshold,
};
