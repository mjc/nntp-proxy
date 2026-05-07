//! Command-line argument parsing for the unified NNTP proxy binary.
//!
//! Provides shared argument structures and value enums for CLI parsing.

use crate::config::{BackendSelectionStrategy, Config, RoutingMode};
use crate::types::{CacheCapacity, ConfigPath, Port, ThreadCount};
use anyhow::{Result, bail};
use clap::{Parser, ValueEnum};
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::time::Duration;

/// Parse port from command line argument
fn parse_port(s: &str) -> Result<Port, String> {
    let port: u16 = s.parse().map_err(|e| format!("Invalid port number: {e}"))?;
    Port::try_new(port).map_err(|e| format!("Invalid port: {e}"))
}

/// User interface/runtime mode for the unified `nntp-proxy` binary.
#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum UiMode {
    /// Run as a headless server and log to stdout.
    Headless,
    /// Run the server with the terminal dashboard enabled.
    Tui,
}

impl UiMode {
    /// Default UI mode for the unified binary.
    pub const DEFAULT: Self = Self::Headless;

    /// Returns true when the interactive terminal dashboard should be launched.
    #[must_use]
    pub const fn uses_tui(self) -> bool {
        matches!(self, Self::Tui)
    }
}

/// Common command-line arguments for the `nntp-proxy` binary.
///
/// Use `#[command(flatten)]` in the top-level CLI args to include these fields.
#[derive(Parser, Debug, Clone)]
pub struct CommonArgs {
    /// Configuration file path
    #[arg(
        short,
        long,
        default_value = "config.toml",
        env = "NNTP_PROXY_CONFIG",
        help_heading = "General"
    )]
    pub config: ConfigPath,

    /// Runtime UI mode: headless server logs or terminal dashboard.
    #[arg(
        long,
        value_enum,
        value_name = "MODE",
        default_value_t = UiMode::DEFAULT,
        env = "NNTP_PROXY_UI",
        help_heading = "General"
    )]
    pub ui: UiMode,

    /// Disable the terminal dashboard and run in headless mode.
    #[arg(long, hide = true, help_heading = "General")]
    pub no_tui: bool,

    /// Bind the dashboard websocket publisher to a free loopback IP:PORT distinct from the proxy listener.
    #[arg(
        long = "tui-listen",
        value_name = "IP:PORT",
        env = "NNTP_PROXY_TUI_LISTEN",
        help_heading = "General"
    )]
    pub tui_listen: Option<SocketAddr>,

    /// Connect the read-only TUI client to a loopback dashboard websocket at IP:PORT.
    #[arg(
        long = "tui-attach",
        value_name = "IP:PORT",
        env = "NNTP_PROXY_TUI_ATTACH",
        help_heading = "General"
    )]
    pub tui_attach: Option<SocketAddr>,

    /// Port to listen on (overrides config file)
    #[arg(
        short,
        long,
        env = "NNTP_PROXY_PORT",
        value_parser = parse_port,
        help_heading = "Network"
    )]
    pub port: Option<Port>,

    /// Host to bind to (overrides config file)
    #[arg(long, env = "NNTP_PROXY_HOST", help_heading = "Network")]
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
        env = "NNTP_PROXY_ROUTING_MODE",
        help_heading = "Routing"
    )]
    pub routing_mode: Option<RoutingMode>,

    /// Backend selection strategy
    #[arg(
        long = "backend-selection",
        alias = "backend-strategy",
        value_enum,
        env = "NNTP_PROXY_BACKEND_SELECTION",
        help_heading = "Routing"
    )]
    pub backend_selection: Option<BackendSelectionStrategy>,

    /// Article cache capacity in memory (e.g., "64mb", "1gb")
    #[arg(
        long = "article-cache-capacity",
        alias = "cache-capacity",
        env = "NNTP_PROXY_ARTICLE_CACHE_CAPACITY",
        help_heading = "Cache"
    )]
    pub article_cache_capacity: Option<CacheCapacity>,

    /// Article cache TTL in seconds
    #[arg(
        long = "article-cache-ttl",
        alias = "cache-ttl",
        alias = "ttl-secs",
        env = "NNTP_PROXY_ARTICLE_CACHE_TTL_SECS",
        help_heading = "Cache"
    )]
    pub article_cache_ttl_secs: Option<u64>,

    /// Store full article bodies in the article cache (true) or track availability only (false)
    #[arg(
        long = "store-article-bodies",
        alias = "cache-articles",
        alias = "store-articles",
        env = "NNTP_PROXY_STORE_ARTICLE_BODIES",
        help_heading = "Cache"
    )]
    pub store_article_bodies: Option<bool>,

    /// Number of worker threads (default: 1, use 0 for CPU cores)
    #[arg(short, long, env = "NNTP_PROXY_THREADS", help_heading = "Performance")]
    pub threads: Option<ThreadCount>,

    /// Enable TCP command pipelining for all backends
    #[arg(
        long = "backend-pipelining",
        alias = "enable-pipelining",
        env = "NNTP_PROXY_BACKEND_PIPELINING",
        help_heading = "Performance"
    )]
    pub backend_pipelining: Option<bool>,
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

    /// Get the effective UI/runtime mode.
    ///
    /// `--no-tui` remains as a hidden compatibility alias for headless mode.
    #[must_use]
    pub const fn effective_ui_mode(&self) -> UiMode {
        if self.no_tui {
            UiMode::Headless
        } else {
            self.ui
        }
    }

    /// Validate combinations of UI-related runtime flags.
    ///
    /// # Errors
    /// Returns an error when attach/listen flags are combined illegally or when
    /// attached mode targets a non-loopback address.
    pub fn validate_runtime_mode(&self) -> Result<()> {
        let ui_mode = self.effective_ui_mode();

        if self.tui_attach.is_some() && ui_mode != UiMode::Tui {
            bail!("--tui-attach requires --ui tui");
        }

        if self.tui_listen.is_some() && ui_mode != UiMode::Headless {
            bail!("--tui-listen requires headless mode; use --ui headless");
        }

        if self.tui_attach.is_some() && self.tui_listen.is_some() {
            bail!("--tui-attach cannot be combined with --tui-listen");
        }

        if let Some(attach_addr) = self.tui_attach
            && !attach_addr.ip().is_loopback()
        {
            bail!(
                "--tui-attach {attach_addr} must connect to a loopback address; use 127.0.0.1 or ::1"
            );
        }

        Ok(())
    }

    /// Validate that the dashboard websocket listener does not reuse the proxy listen socket.
    ///
    /// # Errors
    /// Returns an error when the dashboard listener is non-loopback or would
    /// collide with the proxy listener on the same socket.
    pub fn validate_dashboard_listen(&self, proxy_host: &str, proxy_port: Port) -> Result<()> {
        let Some(dashboard_listen) = self.tui_listen else {
            return Ok(());
        };

        if !dashboard_listen.ip().is_loopback() {
            bail!(
                "--tui-listen {dashboard_listen} must bind to a loopback address; use 127.0.0.1 or ::1"
            );
        }

        if dashboard_listen.port() != proxy_port.get() {
            return Ok(());
        }

        if proxy_listener_conflicts(proxy_host, proxy_port, dashboard_listen) {
            bail!(
                "--tui-listen {} conflicts with the proxy listener {}:{}; use a different port",
                dashboard_listen,
                proxy_host,
                proxy_port.get()
            );
        }

        Ok(())
    }

    fn legacy_env_var<E>(env_get: &E, keys: &[&str]) -> Option<String>
    where
        E: Fn(&str) -> Option<String>,
    {
        keys.iter().find_map(|key| env_get(key))
    }

    fn env_bool<E>(env_get: &E, keys: &[&str]) -> Option<bool>
    where
        E: Fn(&str) -> Option<String>,
    {
        Self::legacy_env_var(env_get, keys)?.parse().ok()
    }

    fn env_u64<E>(env_get: &E, keys: &[&str]) -> Option<u64>
    where
        E: Fn(&str) -> Option<String>,
    {
        Self::legacy_env_var(env_get, keys)?.parse().ok()
    }

    fn env_cache_capacity<E>(env_get: &E, keys: &[&str]) -> Option<CacheCapacity>
    where
        E: Fn(&str) -> Option<String>,
    {
        Self::legacy_env_var(env_get, keys)?.parse().ok()
    }

    fn apply_overrides_with_env<E>(&self, config: &mut Config, env_get: E)
    where
        E: Fn(&str) -> Option<String>,
    {
        // Routing overrides
        if let Some(strategy) = self.backend_selection {
            config.routing.backend_selection = strategy;
        }
        if let Some(rm) = self.routing_mode {
            config.routing.routing_mode = rm;
        }

        let article_cache_capacity = self
            .article_cache_capacity
            .or_else(|| Self::env_cache_capacity(&env_get, &["NNTP_PROXY_CACHE_CAPACITY"]));
        let article_cache_ttl_secs = self
            .article_cache_ttl_secs
            .or_else(|| Self::env_u64(&env_get, &["NNTP_PROXY_CACHE_TTL"]));
        let store_article_bodies = self
            .store_article_bodies
            .or_else(|| Self::env_bool(&env_get, &["NNTP_PROXY_CACHE_ARTICLES"]));

        // Cache overrides — create cache section if needed
        if article_cache_capacity.is_some()
            || article_cache_ttl_secs.is_some()
            || store_article_bodies.is_some()
        {
            let cache = config
                .cache
                .get_or_insert_with(crate::config::Cache::default);
            if let Some(cap) = article_cache_capacity {
                cache.article_cache_capacity = cap;
            }
            if let Some(ttl) = article_cache_ttl_secs {
                cache.article_cache_ttl_secs = Duration::from_secs(ttl);
            }
            if let Some(articles) = store_article_bodies {
                cache.store_article_bodies = articles;
            }
        }

        // Pipelining global override — applies to all servers
        if let Some(enable) = self
            .backend_pipelining
            .or_else(|| Self::env_bool(&env_get, &["NNTP_PROXY_ENABLE_PIPELINING"]))
        {
            for server in &mut config.servers {
                server.backend_pipelining = enable;
            }
        }
    }

    /// Apply CLI argument overrides to loaded config
    ///
    /// This method modifies the provided config by applying any CLI arguments
    /// that were explicitly set by the user. Arguments left at their default
    /// (None) values do not modify the config.
    pub fn apply_overrides(&self, config: &mut Config) {
        self.apply_overrides_with_env(config, |key| std::env::var(key).ok());
    }
}

fn proxy_listener_conflicts(
    proxy_host: &str,
    proxy_port: Port,
    dashboard_listen: SocketAddr,
) -> bool {
    let dashboard_ip = dashboard_listen.ip();
    let proxy_target = (proxy_host, proxy_port.get());

    proxy_target.to_socket_addrs().ok().map_or_else(
        || dashboard_ip.is_unspecified(),
        |addrs| {
            addrs
                .map(|addr| addr.ip())
                .any(|proxy_ip| listener_ips_conflict(proxy_ip, dashboard_ip))
        },
    )
}

fn listener_ips_conflict(proxy_ip: IpAddr, dashboard_ip: IpAddr) -> bool {
    proxy_ip.is_unspecified() || dashboard_ip.is_unspecified() || proxy_ip == dashboard_ip
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_common_args_defaults() {
        let args = default_args();

        assert_eq!(args.listen_addr(None), "0.0.0.0:8119");
        assert_eq!(args.effective_host(), "0.0.0.0");
        assert!(args.effective_port(None).is_none());
        assert_eq!(args.effective_ui_mode(), UiMode::Headless);
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

    #[test]
    fn test_common_args_parse_primary_and_legacy_flags() {
        let args = CommonArgs::parse_from([
            "nntp-proxy",
            "--ui",
            "tui",
            "--article-cache-capacity",
            "128mb",
            "--backend-selection",
            "least-loaded",
            "--article-cache-ttl",
            "7200",
            "--store-article-bodies",
            "true",
            "--backend-pipelining",
            "false",
        ]);

        assert_eq!(
            args.backend_selection,
            Some(BackendSelectionStrategy::LeastLoaded)
        );
        assert_eq!(
            args.article_cache_capacity,
            Some(CacheCapacity::try_new(128_000_000).unwrap())
        );
        assert_eq!(args.article_cache_ttl_secs, Some(7200));
        assert_eq!(args.store_article_bodies, Some(true));
        assert_eq!(args.backend_pipelining, Some(false));
        assert_eq!(args.effective_ui_mode(), UiMode::Tui);

        let legacy_args = CommonArgs::parse_from([
            "nntp-proxy",
            "--no-tui",
            "--cache-capacity",
            "256mb",
            "--backend-strategy",
            "weighted-round-robin",
            "--cache-ttl",
            "3600",
            "--store-articles",
            "false",
            "--enable-pipelining",
            "true",
        ]);

        assert_eq!(
            legacy_args.backend_selection,
            Some(BackendSelectionStrategy::WeightedRoundRobin)
        );
        assert_eq!(
            legacy_args.article_cache_capacity,
            Some(CacheCapacity::try_new(256_000_000).unwrap())
        );
        assert_eq!(legacy_args.article_cache_ttl_secs, Some(3600));
        assert_eq!(legacy_args.store_article_bodies, Some(false));
        assert_eq!(legacy_args.backend_pipelining, Some(true));
        assert_eq!(legacy_args.effective_ui_mode(), UiMode::Headless);
    }

    #[test]
    fn test_validate_runtime_mode_accepts_attach_client() {
        let args = CommonArgs {
            ui: UiMode::Tui,
            tui_attach: Some("127.0.0.1:8119".parse().unwrap()),
            ..default_args()
        };

        args.validate_runtime_mode().unwrap();
    }

    #[test]
    fn test_validate_runtime_mode_rejects_attach_without_tui() {
        let args = CommonArgs {
            tui_attach: Some("127.0.0.1:8119".parse().unwrap()),
            ..default_args()
        };

        assert!(args.validate_runtime_mode().is_err());
    }

    #[test]
    fn test_validate_runtime_mode_rejects_attach_and_listen() {
        let args = CommonArgs {
            ui: UiMode::Tui,
            tui_attach: Some("127.0.0.1:8119".parse().unwrap()),
            tui_listen: Some("127.0.0.1:8120".parse().unwrap()),
            ..default_args()
        };

        assert!(args.validate_runtime_mode().is_err());
    }

    #[test]
    fn test_validate_runtime_mode_rejects_non_loopback_attach() {
        let args = CommonArgs {
            ui: UiMode::Tui,
            tui_attach: Some("10.0.0.5:8119".parse().unwrap()),
            ..default_args()
        };

        assert!(args.validate_runtime_mode().is_err());
    }

    #[test]
    fn test_validate_runtime_mode_rejects_tui_listen_with_local_tui() {
        let args = CommonArgs {
            ui: UiMode::Tui,
            tui_listen: Some("127.0.0.1:8119".parse().unwrap()),
            ..default_args()
        };

        assert!(args.validate_runtime_mode().is_err());
    }

    #[test]
    fn test_validate_dashboard_listen_rejects_same_socket() {
        let args = CommonArgs {
            tui_listen: Some("127.0.0.1:8119".parse().unwrap()),
            ..default_args()
        };

        assert!(
            args.validate_dashboard_listen("127.0.0.1", Port::try_new(8119).unwrap())
                .is_err()
        );
    }

    #[test]
    fn test_validate_dashboard_listen_allows_distinct_socket() {
        let args = CommonArgs {
            tui_listen: Some("127.0.0.1:8120".parse().unwrap()),
            ..default_args()
        };

        args.validate_dashboard_listen("127.0.0.1", Port::try_new(8119).unwrap())
            .unwrap();
    }

    #[test]
    fn test_validate_dashboard_listen_rejects_localhost_alias_collision() {
        let args = CommonArgs {
            tui_listen: Some("127.0.0.1:8119".parse().unwrap()),
            ..default_args()
        };

        assert!(
            args.validate_dashboard_listen("localhost", Port::try_new(8119).unwrap())
                .is_err()
        );
    }

    #[test]
    fn test_validate_dashboard_listen_allows_distinct_loopback_ips() {
        let args = CommonArgs {
            tui_listen: Some("[::1]:8119".parse().unwrap()),
            ..default_args()
        };

        args.validate_dashboard_listen("127.0.0.1", Port::try_new(8119).unwrap())
            .unwrap();
    }

    #[test]
    fn test_validate_dashboard_listen_rejects_non_loopback_bind() {
        let args = CommonArgs {
            tui_listen: Some("0.0.0.0:8119".parse().unwrap()),
            ..default_args()
        };

        assert!(
            args.validate_dashboard_listen("127.0.0.1", Port::try_new(9120).unwrap())
                .is_err()
        );
    }

    #[test]
    fn test_help_shows_tui_socket_format() {
        let mut command = CommonArgs::command();
        let mut help = Vec::new();
        command.write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();

        assert!(help.contains("--tui-listen <IP:PORT>"));
        assert!(help.contains("--tui-attach <IP:PORT>"));
    }

    // Helper to create default args for testing
    fn default_args() -> CommonArgs {
        CommonArgs {
            config: ConfigPath::new("config.toml").unwrap(),
            ui: UiMode::Headless,
            no_tui: false,
            tui_listen: None,
            tui_attach: None,
            port: None,
            host: None,
            routing_mode: None,
            backend_selection: None,
            article_cache_capacity: None,
            article_cache_ttl_secs: None,
            store_article_bodies: None,
            threads: None,
            backend_pipelining: None,
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

        assert_eq!(config.routing.routing_mode, RoutingMode::PerCommand);
        assert_eq!(
            config.routing.backend_selection,
            BackendSelectionStrategy::LeastLoaded
        );
    }

    #[test]
    fn test_apply_overrides_cache() {
        let args = CommonArgs {
            article_cache_capacity: Some(CacheCapacity::try_new(128 * 1024 * 1024).unwrap()),
            article_cache_ttl_secs: Some(7200),
            store_article_bodies: Some(false),
            ..default_args()
        };
        let mut config = Config {
            cache: None, // Start with no cache section
            ..Config::default()
        };

        args.apply_overrides(&mut config);

        let cache = config.cache.as_ref().unwrap();
        assert_eq!(cache.article_cache_capacity.get(), 128 * 1024 * 1024);
        assert_eq!(
            cache.article_cache_ttl_secs,
            crate::constants::duration_polyfill::from_hours(2)
        );
        assert!(!cache.store_article_bodies);
    }

    #[test]
    fn test_apply_overrides_legacy_env_compat() {
        let args = default_args();
        let mut config = Config {
            cache: None,
            servers: vec![
                crate::config::Server::builder("server1.example.com", Port::try_new(119).unwrap())
                    .backend_pipelining(false)
                    .build()
                    .unwrap(),
            ],
            ..Config::default()
        };

        args.apply_overrides_with_env(&mut config, |key| match key {
            "NNTP_PROXY_CACHE_CAPACITY" => Some("128mb".to_string()),
            "NNTP_PROXY_CACHE_TTL" => Some("7200".to_string()),
            "NNTP_PROXY_CACHE_ARTICLES" => Some("false".to_string()),
            "NNTP_PROXY_ENABLE_PIPELINING" => Some("true".to_string()),
            _ => None,
        });

        let cache = config.cache.as_ref().unwrap();
        assert_eq!(cache.article_cache_capacity.get(), 128_000_000);
        assert_eq!(
            cache.article_cache_ttl_secs,
            crate::constants::duration_polyfill::from_hours(2)
        );
        assert!(!cache.store_article_bodies);
        assert!(config.servers[0].backend_pipelining);
    }

    #[test]
    fn test_apply_overrides_pipelining_all_servers() {
        use crate::types::Port;

        let args = CommonArgs {
            backend_pipelining: Some(false),
            ..default_args()
        };
        let mut config = Config::default();
        config.servers = vec![
            crate::config::Server::builder("server1.example.com", Port::try_new(119).unwrap())
                .backend_pipelining(true)
                .build()
                .unwrap(),
            crate::config::Server::builder("server2.example.com", Port::try_new(119).unwrap())
                .backend_pipelining(true)
                .build()
                .unwrap(),
        ];

        args.apply_overrides(&mut config);

        // All servers should have pipelining disabled
        for server in &config.servers {
            assert!(!server.backend_pipelining);
        }
    }
}
