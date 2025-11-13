use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{error, info, warn};

use nntp_proxy::{
    NntpProxy, RoutingMode, RuntimeConfig, load_config_with_fallback,
    types::{ConfigPath, Port, ThreadCount},
};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Port to listen on
    ///
    /// Can be overridden with NNTP_PROXY_PORT environment variable
    #[arg(short, long, default_value = "8119", env = "NNTP_PROXY_PORT")]
    port: Port,

    /// Routing mode: standard, per-command, or hybrid
    ///
    /// - standard: 1:1 mode, each client gets a dedicated backend connection
    /// - per-command: Each command can use a different backend (stateless only)
    /// - hybrid: Starts in per-command mode, auto-switches to stateful on first stateful command
    ///
    /// Can be overridden with NNTP_PROXY_ROUTING_MODE environment variable
    #[arg(
        short = 'm',
        long = "routing-mode",
        value_enum,
        default_value = "hybrid",
        env = "NNTP_PROXY_ROUTING_MODE"
    )]
    routing_mode: RoutingMode,

    /// Configuration file path
    ///
    /// Can be overridden with NNTP_PROXY_CONFIG environment variable
    #[arg(short, long, default_value = "config.toml", env = "NNTP_PROXY_CONFIG")]
    config: ConfigPath,

    /// Number of worker threads (defaults to number of CPU cores)
    ///
    /// Can be overridden with NNTP_PROXY_THREADS environment variable
    #[arg(short, long, env = "NNTP_PROXY_THREADS")]
    threads: Option<ThreadCount>,
}

fn main() -> Result<()> {
    // Initialize tracing with info level by default, respecting RUST_LOG if set
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    // Build and configure runtime
    let runtime_config = RuntimeConfig::from_args(args.threads);
    let rt = runtime_config.build_runtime()?;

    rt.block_on(run_proxy(args))
}

async fn run_proxy(args: Args) -> Result<()> {
    // Load configuration with automatic fallback
    let (config, source) = load_config_with_fallback(args.config.as_str())?;

    info!("Loaded configuration from {}", source.description());
    info!("Loaded {} backend servers:", config.servers.len());
    for server in &config.servers {
        info!("  - {} ({}:{})", server.name, server.host, server.port);
    }

    // Create proxy (wrapped in Arc for sharing across tasks)
    let proxy = Arc::new(NntpProxy::new(config, args.routing_mode)?);

    // Prewarm connection pools before accepting clients
    info!("Prewarming connection pools...");
    if let Err(e) = proxy.prewarm_connections().await {
        warn!("Failed to prewarm connection pools: {}", e);
    }
    info!("Connection pools ready");

    // Start listening
    let listen_addr = format!("0.0.0.0:{}", args.port.get());
    let listener = TcpListener::bind(&listen_addr).await?;
    info!(
        "NNTP proxy listening on {} ({})",
        listen_addr, args.routing_mode
    );

    // Set up graceful shutdown
    let proxy_for_shutdown = proxy.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("Shutdown signal received, closing idle connections...");
        proxy_for_shutdown.graceful_shutdown().await;
        info!("Graceful shutdown complete");
        std::process::exit(0);
    });

    // Start periodic connection stats flusher (every 30 seconds)
    let connection_stats = proxy.connection_stats().clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            connection_stats.flush();
        }
    });

    // Determine which handler to use based on routing mode
    let uses_per_command_routing = args.routing_mode.supports_per_command_routing();

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let proxy_clone = proxy.clone();
                if uses_per_command_routing {
                    // Per-command or Hybrid mode
                    tokio::spawn(async move {
                        if let Err(e) = proxy_clone
                            .handle_client_per_command_routing(stream, addr)
                            .await
                        {
                            error!("Error handling client {}: {}", addr, e);
                        }
                    });
                } else {
                    // Standard 1:1 mode
                    tokio::spawn(async move {
                        if let Err(e) = proxy_clone.handle_client(stream, addr).await {
                            error!("Error handling client {}: {}", addr, e);
                        }
                    });
                }
            }
            Err(e) => {
                error!("Failed to accept connection: {}", e);
            }
        }
    }
}

/// Wait for shutdown signal
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
