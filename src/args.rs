//! Command-line argument parsing for NNTP proxy binaries
//!
//! Provides shared argument structures to avoid duplication across binaries.

use crate::config::{BackendSelectionStrategy, Config, RoutingMode};
use crate::types::{CacheCapacity, ConfigPath, Port, ThreadCount};
use clap::Parser;
use std::time::Duration;

/// Parse port from command line argument
fn parse_port(s: &str) -> Result<Port, String> {
    let port: u16 = s.parse().map_err(|e| format!("Invalid port number: {e}"))?;
    Port::try_new(port).map_err(|e| format!("Invalid port: {e}"))
}

/// Common command-line arguments for NNTP proxy binaries
///
/// Use `#[command(flatten)]` in binary-specific Args to include these fields.
#[derive(Parser, Debug, Clone)]
pub struct CommonArgs {
    /// Configuration file path
    #[arg(
        short,
        long,
        default_value = "config.toml",
        env,
        help_heading = "General"
    )]
    pub config: ConfigPath,

    /// Port to listen on (overrides config file)
    #[arg(short, long, env, value_parser = parse_port, help_heading = "Network")]
    pub port: Option<Port>,

    /// Host to bind to (overrides config file)
    #[arg(long, env, help_heading = "Network")]
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
        env,
        help_heading = "Routing"
    )]
    pub routing_mode: Option<RoutingMode>,

    /// Backend selection strategy
    #[arg(long = "backend-selection", value_enum, env, help_heading = "Routing")]
    pub backend_selection: Option<BackendSelectionStrategy>,

    /// Memory cache capacity (e.g., "64mb", "1gb")
    #[arg(long = "cache-capacity", env, help_heading = "Cache")]
    pub cache_capacity: Option<CacheCapacity>,

    /// Cache TTL in seconds
    #[arg(long = "cache-ttl", env, help_heading = "Cache")]
    pub cache_ttl: Option<u64>,

    /// Cache full article bodies (true) or availability-only (false)
    #[arg(long = "cache-articles", env, help_heading = "Cache")]
    pub cache_articles: Option<bool>,

    /// Number of worker threads (default: 1, use 0 for CPU cores)
    #[arg(short, long, env, help_heading = "Performance")]
    pub threads: Option<ThreadCount>,

    /// Enable TCP command pipelining for all backends
    #[arg(long = "enable-pipelining", env, help_heading = "Performance")]
    pub enable_pipelining: Option<bool>,
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

    /// Apply CLI argument overrides to loaded config
    ///
    /// This method modifies the provided config by applying any CLI arguments
    /// that were explicitly set by the user. Arguments left at their default
    /// (None) values do not modify the config.
    pub fn apply_overrides(&self, config: &mut Config) {
        // Routing overrides
        if let Some(strategy) = self.backend_selection {
            config.proxy.backend_selection = strategy;
        }
        if let Some(rm) = self.routing_mode {
            config.proxy.routing_mode = rm;
        }

        // Cache overrides — create cache section if needed
        if self.cache_capacity.is_some()
            || self.cache_ttl.is_some()
            || self.cache_articles.is_some()
        {
            let cache = config
                .cache
                .get_or_insert_with(crate::config::Cache::default);
            if let Some(cap) = self.cache_capacity {
                cache.max_capacity = cap;
            }
            if let Some(ttl) = self.cache_ttl {
                cache.ttl = Duration::from_secs(ttl);
            }
            if let Some(articles) = self.cache_articles {
                cache.cache_articles = articles;
            }
        }

        // Pipelining global override — applies to all servers
        if let Some(enable) = self.enable_pipelining {
            for server in &mut config.servers {
                server.enable_pipelining = enable;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_common_args_defaults() {
        let args = default_args();

        assert_eq!(args.listen_addr(None), "0.0.0.0:8119");
        assert_eq!(args.effective_host(), "0.0.0.0");
        assert!(args.effective_port(None).is_none());
    }

    #[test]
    fn test_common_args_with_port() {
        let args = CommonArgs {
            port: Some(Port::try_new(9119).unwrap()),
            ..default_args()
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
            host: Some("127.0.0.1".to_string()),
            ..default_args()
        };

        assert_eq!(args.listen_addr(None), "127.0.0.1:8119");
        assert_eq!(args.effective_host(), "127.0.0.1");
    }

    #[test]
    fn test_common_args_port_override() {
        let args = CommonArgs {
            port: Some(Port::try_new(9119).unwrap()),
            ..default_args()
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
        let args = default_args();
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
            ..default_args()
        };

        assert_eq!(args.listen_addr(None), "192.168.1.1:9119");
        assert_eq!(args.effective_host(), "192.168.1.1");
        assert_eq!(
            args.effective_port(None),
            Some(Port::try_new(9119).unwrap())
        );
    }

    #[test]
    fn test_routing_modes() {
        let hybrid = CommonArgs {
            routing_mode: Some(RoutingMode::Hybrid),
            ..default_args()
        };
        assert_eq!(hybrid.routing_mode, Some(RoutingMode::Hybrid));

        let stateful = CommonArgs {
            routing_mode: Some(RoutingMode::Stateful),
            ..default_args()
        };
        assert_eq!(stateful.routing_mode, Some(RoutingMode::Stateful));

        let per_command = CommonArgs {
            routing_mode: Some(RoutingMode::PerCommand),
            ..default_args()
        };
        assert_eq!(per_command.routing_mode, Some(RoutingMode::PerCommand));
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
            config: ConfigPath::new("config.toml").unwrap(),
            port: None,
            host: None,
            routing_mode: None,
            backend_selection: None,
            cache_capacity: None,
            cache_ttl: None,
            cache_articles: None,
            threads: None,
            enable_pipelining: None,
        }
    }

    #[test]
    fn test_apply_overrides_none_preserves_config() {
        let args = default_args();
        let mut config = Config::default();
        config.proxy.backend_selection = BackendSelectionStrategy::WeightedRoundRobin;
        config.proxy.routing_mode = RoutingMode::Stateful;

        args.apply_overrides(&mut config);

        // Config unchanged when args are None
        assert_eq!(
            config.proxy.backend_selection,
            BackendSelectionStrategy::WeightedRoundRobin
        );
        assert_eq!(config.proxy.routing_mode, RoutingMode::Stateful);
    }

    #[test]
    fn test_apply_overrides_routing() {
        let args = CommonArgs {
            routing_mode: Some(RoutingMode::PerCommand),
            backend_selection: Some(BackendSelectionStrategy::LeastLoaded),
            ..default_args()
        };
        let mut config = Config::default();

        args.apply_overrides(&mut config);

        assert_eq!(config.proxy.routing_mode, RoutingMode::PerCommand);
        assert_eq!(
            config.proxy.backend_selection,
            BackendSelectionStrategy::LeastLoaded
        );
    }

    #[test]
    fn test_apply_overrides_cache() {
        let args = CommonArgs {
            cache_capacity: Some(CacheCapacity::try_new(128 * 1024 * 1024).unwrap()),
            cache_ttl: Some(7200),
            cache_articles: Some(false),
            ..default_args()
        };
        let mut config = Config {
            cache: None, // Start with no cache section
            ..Config::default()
        };

        args.apply_overrides(&mut config);

        let cache = config.cache.as_ref().unwrap();
        assert_eq!(cache.max_capacity.get(), 128 * 1024 * 1024);
        assert_eq!(cache.ttl, Duration::from_secs(7200));
        assert!(!cache.cache_articles);
    }

    #[test]
    fn test_apply_overrides_pipelining_all_servers() {
        use crate::types::Port;

        let args = CommonArgs {
            enable_pipelining: Some(false),
            ..default_args()
        };
        let mut config = Config::default();
        config.servers = vec![
            crate::config::Server::builder("server1.example.com", Port::try_new(119).unwrap())
                .enable_pipelining(true)
                .build()
                .unwrap(),
            crate::config::Server::builder("server2.example.com", Port::try_new(119).unwrap())
                .enable_pipelining(true)
                .build()
                .unwrap(),
        ];

        args.apply_overrides(&mut config);

        // All servers should have pipelining disabled
        for server in &config.servers {
            assert!(!server.enable_pipelining);
        }
    }
}
