use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::mpsc;
use tracing::{error, info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use nntp_proxy::{CommonArgs, NntpProxy, RuntimeConfig, load_config_with_fallback, tui};

#[derive(Parser, Debug)]
#[command(author, version, about = "NNTP Proxy with TUI Dashboard", long_about = None)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,

    /// Disable TUI and run in headless mode
    #[arg(long, default_value = "false")]
    no_tui: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    // If TUI is enabled, we need special logging setup
    // If TUI is disabled, use normal logging
    let (_guard, log_buffer) = if args.no_tui {
        // Normal logging to stderr
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();
        (None, None)
    } else {
        // For TUI mode, capture logs in memory AND write to debug.log file
        use nntp_proxy::tui::{LogBuffer, LogMakeWriter};

        let log_buffer = LogBuffer::new();
        let log_writer = LogMakeWriter::new(log_buffer.clone());

        // Use RUST_LOG or default to "info" (NOT "debug")
        let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

        // Create file appender for debug.log
        let file_appender = tracing_appender::rolling::never(".", "debug.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        // Dual output: TUI log buffer + debug.log file (both respect RUST_LOG)
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(log_writer)
                    .with_ansi(false)
                    .with_target(false)
                    .compact(),
            )
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false),
            )
            .init();

        (Some(guard), Some(log_buffer))
    };

    // Build and configure runtime
    let runtime_config = RuntimeConfig::from_args(args.common.threads);
    let rt = runtime_config.build_runtime()?;

    rt.block_on(run_proxy(args, log_buffer))
}

async fn run_proxy(args: Args, log_buffer: Option<nntp_proxy::tui::LogBuffer>) -> Result<()> {
    // Load configuration with automatic fallback
    let (config, source) = load_config_with_fallback(args.common.config.as_str())?;

    info!("Loaded configuration from {}", source.description());
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

    // Create proxy with metrics enabled for TUI
    let proxy = Arc::new(
        NntpProxy::builder(config)
            .with_routing_mode(args.common.routing_mode)
            .with_metrics() // Enable metrics for TUI (causes ~45% perf penalty)
            .build()?,
    );

    // Create shutdown channel - TUI will control shutdown
    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    // Create channel for signaling TUI to shutdown from external signals
    let (tui_shutdown_tx, tui_shutdown_rx) = mpsc::channel::<()>(1);

    // Launch TUI FIRST if enabled (before prewarming for faster startup)
    // This allows seeing the dashboard immediately
    let tui_handle = if !args.no_tui {
        let mut tui_builder = if let Some(log_buffer) = log_buffer {
            tui::TuiAppBuilder::new(
                proxy.metrics().clone(),
                proxy.router().clone(),
                proxy.servers().to_vec().into(),
            )
            .with_log_buffer(log_buffer)
        } else {
            tui::TuiAppBuilder::new(
                proxy.metrics().clone(),
                proxy.router().clone(),
                proxy.servers().to_vec().into(),
            )
        };

        // Add location cache if enabled
        if let Some(cache) = proxy.location_cache() {
            tui_builder = tui_builder.with_location_cache(cache.clone());
        }

        let tui_app = tui_builder.build();

        let shutdown_tx_for_tui = shutdown_tx.clone();
        Some(tokio::spawn(async move {
            if let Err(e) = tui::run_tui(tui_app, shutdown_tx_for_tui, tui_shutdown_rx).await {
                error!("TUI error: {}", e);
            }
            // When TUI exits, signal shutdown
            info!("TUI exited, initiating shutdown");
        }))
    } else {
        None
    };

    // Prewarm connection pools in background (doesn't block TUI startup)
    let proxy_for_prewarm = proxy.clone();
    let prewarm_handle = tokio::spawn(async move {
        info!("Prewarming connection pools...");
        if let Err(e) = proxy_for_prewarm.prewarm_connections().await {
            warn!("Failed to prewarm connection pools: {}", e);
            return false;
        }
        info!("Connection pools ready");
        true
    });

    // Start listening
    let listen_addr = format!("{}:{}", listen_host, listen_port.get());
    let listener = TcpListener::bind(&listen_addr).await?;
    info!(
        "NNTP proxy listening on {} ({})",
        listen_addr, args.common.routing_mode
    );

    // Wait for prewarming to complete before accepting connections
    info!("Waiting for connection pools to be ready...");
    match prewarm_handle.await {
        Ok(true) => info!("Ready to accept connections"),
        Ok(false) => warn!("Prewarming failed, but continuing anyway"),
        Err(e) => warn!("Prewarming task failed: {}", e),
    }

    // Set up graceful shutdown signal handler for non-TUI mode or SIGTERM
    let proxy_for_shutdown = proxy.clone();
    let shutdown_tx_signal = shutdown_tx.clone();
    tokio::spawn(async move {
        shutdown_signal().await;

        info!("Shutdown signal received");

        // Signal TUI to exit (it will clean up terminal properly and immediately)
        let _ = tui_shutdown_tx.send(()).await;

        // Notify anyone listening
        let _ = shutdown_tx_signal.send(()).await;

        // Close idle connections
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

    // Accept connections (automatic dispatch based on routing mode)
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
                        // Automatic dispatch based on routing mode
                        tokio::spawn(async move {
                            if let Err(e) = proxy_clone.handle_client(stream, addr).await {
                                error!("Error handling client {}: {}", addr, e);
                            }
                        });
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
