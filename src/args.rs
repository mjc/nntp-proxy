//! Command-line argument parsing for NNTP proxy binaries
//!
//! Provides shared argument structures to avoid duplication across binaries.

use crate::RoutingMode;
use crate::types::{CacheCapacity, ConfigPath, Port, ThreadCount};
use clap::Parser;

/// Parse port from command line argument
fn parse_port(s: &str) -> Result<Port, String> {
    let port: u16 = s
        .parse()
        .map_err(|e| format!("Invalid port number: {}", e))?;
    Port::try_new(port).map_err(|e| format!("Invalid port: {}", e))
}

/// Common command-line arguments for NNTP proxy binaries
///
/// Use `#[command(flatten)]` in binary-specific Args to include these fields.
#[derive(Parser, Debug, Clone)]
pub struct CommonArgs {
    /// Port to listen on (overrides config file)
    #[arg(short, long, env, value_parser = parse_port)]
    pub port: Option<Port>,

    /// Host to bind to (overrides config file)
    #[arg(long, env)]
    pub host: Option<String>,

    /// Routing mode: stateful, per-command, or hybrid
    ///
    /// - stateful: 1:1 mode, each client gets a dedicated backend connection
    /// - per-command: Each command can use a different backend (stateless only)
    /// - hybrid: Starts in per-command mode, auto-switches to stateful on first stateful command
    #[arg(
        short = 'm',
        long = "routing-mode",
        value_enum,
        default_value = "hybrid",
        env
    )]
    pub routing_mode: RoutingMode,

    /// Configuration file path
    #[arg(short, long, default_value = "config.toml", env)]
    pub config: ConfigPath,

    /// Number of worker threads (default: 1, use 0 for CPU cores)
    #[arg(short, long, env)]
    pub threads: Option<ThreadCount>,
}

impl CommonArgs {
    /// Default port when none specified
    const DEFAULT_PORT: u16 = 8119;

    /// Default host when none specified
    const DEFAULT_HOST: &'static str = "0.0.0.0";

    /// Get formatted listen address
    ///
    /// # Arguments
    /// * `config_port` - Port from config file (if any)
    ///
    /// # Returns
    /// Formatted listen address (e.g., "0.0.0.0:8119")
    #[must_use]
    pub fn listen_addr(&self, config_port: Option<Port>) -> String {
        let port = self
            .effective_port(config_port)
            .map_or(Self::DEFAULT_PORT, |p| p.get());

        format!("{}:{}", self.effective_host(), port)
    }

    /// Get effective port (from args or config)
    #[must_use]
    pub fn effective_port(&self, config_port: Option<Port>) -> Option<Port> {
        self.port.or(config_port)
    }

    /// Get effective host
    #[must_use]
    pub fn effective_host(&self) -> &str {
        self.host.as_deref().unwrap_or(Self::DEFAULT_HOST)
    }
}

/// Cache-specific arguments
#[derive(Parser, Debug, Clone)]
pub struct CacheArgs {
    /// Cache max capacity in bytes (default 64 MB)
    #[arg(long, default_value = "67108864", env)]
    pub cache_capacity: CacheCapacity,

    /// Cache TTL in seconds
    #[arg(long, default_value = "3600", env)]
    pub cache_ttl: u64,
}

impl CacheArgs {
    /// Get cache TTL as Duration
    #[must_use]
    pub const fn ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.cache_ttl)
    }

    /// Get cache capacity as usize
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.cache_capacity.get() as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_common_args_defaults() {
        let args = CommonArgs {
            port: None,
            host: None,
            routing_mode: RoutingMode::Hybrid,
            config: ConfigPath::new("config.toml").unwrap(),
            threads: None,
        };

        assert_eq!(args.listen_addr(None), "0.0.0.0:8119");
        assert_eq!(args.effective_host(), "0.0.0.0");
        assert!(args.effective_port(None).is_none());
    }

    #[test]
    fn test_common_args_with_port() {
        let args = CommonArgs {
            port: Some(Port::try_new(9119).unwrap()),
            host: None,
            routing_mode: RoutingMode::Hybrid,
            config: ConfigPath::new("config.toml").unwrap(),
            threads: None,
        };

        assert_eq!(args.listen_addr(None), "0.0.0.0:9119");
        assert_eq!(
            args.effective_port(None),
            Some(Port::try_new(9119).unwrap())
        );
    }

    #[test]
    fn test_common_args_with_host() {
        let args = CommonArgs {
            port: None,
            host: Some("127.0.0.1".to_string()),
            routing_mode: RoutingMode::Hybrid,
            config: ConfigPath::new("config.toml").unwrap(),
            threads: None,
        };

        assert_eq!(args.listen_addr(None), "127.0.0.1:8119");
        assert_eq!(args.effective_host(), "127.0.0.1");
    }

    #[test]
    fn test_common_args_port_override() {
        let args = CommonArgs {
            port: Some(Port::try_new(9119).unwrap()),
            host: None,
            routing_mode: RoutingMode::Hybrid,
            config: ConfigPath::new("config.toml").unwrap(),
            threads: None,
        };

        let config_port = Some(Port::try_new(7119).unwrap());

        // Args port should override config port
        assert_eq!(args.listen_addr(config_port), "0.0.0.0:9119");
        assert_eq!(
            args.effective_port(config_port),
            Some(Port::try_new(9119).unwrap())
        );
    }

    #[test]
    fn test_common_args_config_port_fallback() {
        let args = CommonArgs {
            port: None,
            host: None,
            routing_mode: RoutingMode::Hybrid,
            config: ConfigPath::new("config.toml").unwrap(),
            threads: None,
        };

        let config_port = Some(Port::try_new(7119).unwrap());

        // Should use config port when args port is None
        assert_eq!(args.listen_addr(config_port), "0.0.0.0:7119");
        assert_eq!(
            args.effective_port(config_port),
            Some(Port::try_new(7119).unwrap())
        );
    }

    #[test]
    fn test_common_args_custom_host_and_port() {
        let args = CommonArgs {
            port: Some(Port::try_new(9119).unwrap()),
            host: Some("192.168.1.1".to_string()),
            routing_mode: RoutingMode::Hybrid,
            config: ConfigPath::new("config.toml").unwrap(),
            threads: None,
        };

        assert_eq!(args.listen_addr(None), "192.168.1.1:9119");
        assert_eq!(args.effective_host(), "192.168.1.1");
        assert_eq!(
            args.effective_port(None),
            Some(Port::try_new(9119).unwrap())
        );
    }

    #[test]
    fn test_cache_args_defaults() {
        let args = CacheArgs {
            cache_capacity: CacheCapacity::try_new(67108864).unwrap(), // 64 MB
            cache_ttl: 3600,
        };

        assert_eq!(args.capacity(), 67108864);
        assert_eq!(args.cache_ttl, 3600);
        assert_eq!(args.ttl(), std::time::Duration::from_secs(3600));
    }

    #[test]
    fn test_cache_args_custom_values() {
        let args = CacheArgs {
            cache_capacity: CacheCapacity::try_new(134217728).unwrap(), // 128 MB
            cache_ttl: 7200,
        };

        assert_eq!(args.capacity(), 134217728);
        assert_eq!(args.cache_ttl, 7200);
        assert_eq!(args.ttl(), std::time::Duration::from_secs(7200));
    }

    #[test]
    fn test_routing_modes() {
        let hybrid = CommonArgs {
            routing_mode: RoutingMode::Hybrid,
            ..default_args()
        };
        assert_eq!(hybrid.routing_mode, RoutingMode::Hybrid);

        let stateful = CommonArgs {
            routing_mode: RoutingMode::Stateful,
            ..default_args()
        };
        assert_eq!(stateful.routing_mode, RoutingMode::Stateful);

        let per_command = CommonArgs {
            routing_mode: RoutingMode::PerCommand,
            ..default_args()
        };
        assert_eq!(per_command.routing_mode, RoutingMode::PerCommand);
    }

    #[test]
    fn test_thread_count() {
        let default_threads = CommonArgs {
            threads: None,
            ..default_args()
        };
        assert!(default_threads.threads.is_none());

        let single_thread = CommonArgs {
            threads: Some(ThreadCount::new(1).unwrap()),
            ..default_args()
        };
        assert_eq!(single_thread.threads, Some(ThreadCount::new(1).unwrap()));

        let multi_thread = CommonArgs {
            threads: Some(ThreadCount::new(4).unwrap()),
            ..default_args()
        };
        assert_eq!(multi_thread.threads, Some(ThreadCount::new(4).unwrap()));
    }

    // Helper to create default args for testing
    fn default_args() -> CommonArgs {
        CommonArgs {
            port: None,
            host: None,
            routing_mode: RoutingMode::Hybrid,
            config: ConfigPath::new("config.toml").unwrap(),
            threads: None,
        }
    }
}
