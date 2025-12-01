use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::{error, info, warn};

use nntp_proxy::{
    CommonArgs, NntpProxy, RuntimeConfig, load_config_with_fallback, shutdown_signal,
};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
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

    // Load configuration first to get thread count
    let (config, _) = load_config_with_fallback(args.common.config.as_str())?;

    // Use CLI arg if provided, else config value (0 means use CPU cores)
    let threads = args.common.threads.or(Some(config.proxy.threads));

    // Build and configure runtime
    let runtime_config = RuntimeConfig::from_args(threads);
    let rt = runtime_config.build_runtime()?;

    rt.block_on(run_proxy(args, config))
}

async fn run_proxy(args: Args, config: nntp_proxy::config::Config) -> Result<()> {
    // Config already loaded in main()
    info!("Loaded {} backend servers:", config.servers.len());
    for server in &config.servers {
        info!("  - {} ({}:{})", server.name, server.host, server.port);
    }

    // Extract listen address before moving config
    let listen_host = args
        .common
        .host
        .unwrap_or_else(|| config.proxy.host.clone());
    let listen_port = args.common.port.unwrap_or(config.proxy.port);

    // Create proxy (wrapped in Arc for sharing across tasks)
    let proxy = Arc::new(NntpProxy::new(config, args.common.routing_mode)?);

    // Prewarm connection pools before accepting clients
    info!("Prewarming connection pools...");
    if let Err(e) = proxy.prewarm_connections().await {
        warn!("Failed to prewarm connection pools: {}", e);
    }
    info!("Connection pools ready");

    // Start listening
    let listen_addr = format!("{}:{}", listen_host, *listen_port);
    let listener = TcpListener::bind(&listen_addr).await?;
    info!(
        "NNTP proxy listening on {} ({})",
        listen_addr, args.common.routing_mode
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
    let uses_per_command_routing = args.common.routing_mode.supports_per_command_routing();

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
