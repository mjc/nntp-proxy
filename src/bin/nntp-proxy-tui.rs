use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use nntp_proxy::{
    NntpProxy, RoutingMode, RuntimeConfig, load_config_with_fallback, tui,
    types::{ConfigPath, Port, ThreadCount},
};

#[derive(Parser, Debug)]
#[command(author, version, about = "NNTP Proxy with TUI Dashboard", long_about = None)]
struct Args {
    /// Port to listen on
    #[arg(short, long, default_value = "8119", env = "NNTP_PROXY_PORT")]
    port: Port,

    /// Routing mode: standard, per-command, or hybrid
    #[arg(
        short = 'm',
        long = "routing-mode",
        value_enum,
        default_value = "hybrid",
        env = "NNTP_PROXY_ROUTING_MODE"
    )]
    routing_mode: RoutingMode,

    /// Configuration file path
    #[arg(short, long, default_value = "config.toml", env = "NNTP_PROXY_CONFIG")]
    config: ConfigPath,

    /// Number of worker threads (defaults to number of CPU cores)
    #[arg(short, long, env = "NNTP_PROXY_THREADS")]
    threads: Option<ThreadCount>,

    /// Disable TUI and run in headless mode
    #[arg(long, default_value = "false")]
    no_tui: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // If TUI is enabled, we need special logging setup
    // If TUI is disabled, use normal logging
    let log_buffer = if args.no_tui {
        // Normal logging to stderr
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();
        None
    } else {
        // For TUI mode, capture logs in memory
        use nntp_proxy::tui::{LogBuffer, LogMakeWriter};

        let log_buffer = LogBuffer::new();
        let log_writer = LogMakeWriter::new(log_buffer.clone());

        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_writer(log_writer)
            .with_ansi(false) // No ANSI codes in TUI logs
            .with_target(false) // Remove module paths (nntp_proxy::proxy)
            .compact() // Compact format: HH:MM:SS.microseconds
            .init();

        Some(log_buffer)
    };

    // Build and configure runtime
    let runtime_config = RuntimeConfig::from_args(args.threads);
    let rt = runtime_config.build_runtime()?;
    
    rt.block_on(run_proxy(args, log_buffer))
}

async fn run_proxy(args: Args, log_buffer: Option<nntp_proxy::tui::LogBuffer>) -> Result<()> {
    // Load configuration with automatic fallback
    let (config, source) = load_config_with_fallback(args.config.as_str())?;
    
    info!("Loaded configuration from {}", source.description());
    info!("Loaded {} backend servers:", config.servers.len());
    for server in &config.servers {
        info!("  - {} ({}:{})", server.name, server.host, server.port);
    }

    // Create proxy with metrics enabled for TUI
    let proxy = Arc::new(
        NntpProxy::builder(config)
            .with_routing_mode(args.routing_mode)
            .with_metrics() // Enable metrics for TUI (causes ~45% perf penalty)
            .build()?,
    );

    // Prewarm connection pools
    info!("Prewarming connection pools...");
    if let Err(e) = proxy.prewarm_connections().await {
        warn!("Failed to prewarm connection pools: {}", e);
    }
    info!("Connection pools ready");

    // Create shutdown channel - TUI will control shutdown
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    // Launch TUI FIRST if enabled
    // This allows seeing the dashboard before connections start
    let tui_handle = if !args.no_tui {
        let tui_app = if let Some(log_buffer) = log_buffer {
            tui::TuiApp::with_log_buffer(
                proxy.metrics().clone(),
                proxy.router().clone(),
                proxy.servers().to_vec().into(),
                log_buffer,
            )
        } else {
            tui::TuiApp::new(
                proxy.metrics().clone(),
                proxy.router().clone(),
                proxy.servers().to_vec().into(),
            )
        };

        let shutdown_tx_for_tui = shutdown_tx.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = tui::run_tui(tui_app, shutdown_tx_for_tui).await {
                error!("TUI error: {}", e);
            }
            // When TUI exits, signal shutdown
            info!("TUI exited, initiating shutdown");
        }))
    } else {
        None
    };

    // Start listening
    let listen_addr = format!("0.0.0.0:{}", args.port.get());
    let listener = TcpListener::bind(&listen_addr).await?;
    info!(
        "NNTP proxy listening on {} ({})",
        listen_addr, args.routing_mode
    );

    // Set up graceful shutdown signal handler for non-TUI mode or SIGTERM
    let proxy_for_shutdown = proxy.clone();
    let shutdown_tx_signal = shutdown_tx.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("Shutdown signal received, closing idle connections...");

        // Notify anyone listening
        let _ = shutdown_tx_signal.send(()).await;

        proxy_for_shutdown.graceful_shutdown().await;
        info!("Graceful shutdown complete");
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

    // Accept connections
    let uses_per_command_routing = args.routing_mode.supports_per_command_routing();

    loop {
        tokio::select! {
            // Check for shutdown signal from TUI
            _ = shutdown_rx.recv() => {
                info!("Shutdown initiated, stopping accept loop");
                break;
            }
            // Accept new connections
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, addr)) => {
                        let proxy_clone = proxy.clone();
                        if uses_per_command_routing {
                            tokio::spawn(async move {
                                if let Err(e) = proxy_clone
                                    .handle_client_per_command_routing(stream, addr)
                                    .await
                                {
                                    error!("Error handling client {}: {}", addr, e);
                                }
                            });
                        } else {
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
    }

    // Graceful shutdown
    proxy.graceful_shutdown().await;
    info!("Proxy shutdown complete");

    // Wait for TUI to exit if it's running
    if let Some(handle) = tui_handle {
        let _ = handle.await;
    }

    Ok(())
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
