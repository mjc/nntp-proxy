//! Configuration types, defaults, and loading.
//!
//! Configuration can be loaded from a TOML file, environment variables, or the
//! command line. The public types in this module describe the normalized
//! runtime configuration; legacy field names are migrated at load time so the
//! rest of the proxy does not need compatibility branches.

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
    BackendSelectionStrategy, Cache, ClientAuth, CompressionCodec, Config, DiskCache, HealthCheck,
    Memory, Proxy, QueuePressureLimits, Routing, RoutingMode, Server, UserCredentials,
};

// Re-export default functions for use in tests and other modules
pub use defaults::{
    cache_max_capacity, cache_ttl, disk_cache_capacity, disk_cache_compression_codec,
    disk_cache_path, disk_cache_shards, health_check_interval, health_check_max_per_cycle,
    health_check_pool_timeout, health_check_timeout, max_connections, tls_verify_cert,
    unhealthy_threshold,
};
