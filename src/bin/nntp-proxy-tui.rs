use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use nntp_proxy::{
    NntpProxy, RoutingMode, create_default_config, has_server_env_vars, load_config,
    load_config_from_env, tui,
    types::{ConfigPath, Port, ThreadCount},
};

/// Pin current process to specific CPU cores for optimal performance
#[cfg(target_os = "linux")]
fn pin_to_cpu_cores(num_cores: usize) -> Result<()> {
    use nix::sched::{CpuSet, sched_setaffinity};
    use nix::unistd::Pid;

    let mut cpu_set = CpuSet::new();
    for core in 0..num_cores {
        let _ = cpu_set.set(core);
    }

    match sched_setaffinity(Pid::from_raw(0), &cpu_set) {
        Ok(_) => {
            info!(
                "Successfully pinned process to {} CPU cores for optimal performance",
                num_cores
            );
        }
        Err(e) => {
            warn!(
                "Failed to set CPU affinity: {}, continuing without pinning",
                e
            );
        }
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn pin_to_cpu_cores(_num_cores: usize) -> Result<()> {
    info!("CPU pinning not available on this platform");
    Ok(())
}

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
    if args.no_tui {
        // Normal logging to stderr
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();
    } else {
        // For TUI mode, redirect logs to a file to avoid interference
        use tracing_subscriber::fmt::writer::MakeWriterExt;

        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("nntp-proxy-tui.log")?;

        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .with_writer(log_file.with_max_level(tracing::Level::INFO))
            .init();
    }

    let num_cpus = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1);
    let worker_threads = args.threads.map(|t| t.get()).unwrap_or(num_cpus);

    pin_to_cpu_cores(worker_threads)?;

    if worker_threads == 1 {
        info!("Starting NNTP proxy with single-threaded runtime");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(run_proxy(args))
    } else {
        info!(
            "Starting NNTP proxy with {} worker threads (detected {} CPUs)",
            worker_threads, num_cpus
        );
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .enable_all()
            .build()?;
        rt.block_on(run_proxy(args))
    }
}

async fn run_proxy(args: Args) -> Result<()> {
    // Load configuration
    let config = if std::path::Path::new(args.config.as_str()).exists() {
        match load_config(args.config.as_str()) {
            Ok(config) => config,
            Err(e) => {
                error!(
                    "Failed to load existing config file '{}': {}",
                    args.config, e
                );
                error!("Please check your config file syntax and try again");
                return Err(e);
            }
        }
    } else if has_server_env_vars() {
        match load_config_from_env() {
            Ok(config) => {
                info!("Using configuration from environment variables (no config file)");
                config
            }
            Err(e) => {
                error!(
                    "Failed to load configuration from environment variables: {}",
                    e
                );
                return Err(e);
            }
        }
    } else {
        warn!(
            "Config file '{}' not found and no NNTP_SERVER_* environment variables set",
            args.config
        );
        warn!("Creating default config file - please edit it to add your backend servers");
        let default_config = create_default_config();
        let config_toml = toml::to_string_pretty(&default_config)?;
        std::fs::write(args.config.as_str(), &config_toml)?;
        info!("Created default config file: {}", args.config);
        default_config
    };

    info!("Loaded {} backend servers:", config.servers.len());
    for server in &config.servers {
        info!("  - {} ({}:{})", server.name, server.host, server.port);
    }

    // Create proxy
    let proxy = Arc::new(NntpProxy::new(config, args.routing_mode)?);

    // Prewarm connection pools
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

    // Create shutdown channel for TUI
    let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

    // Launch TUI if enabled
    let _tui_handle = if !args.no_tui {
        let tui_app = tui::TuiApp::new(proxy.metrics().clone(), proxy.servers().to_vec().into());

        let shutdown_rx_tui = shutdown_rx;
        Some(tokio::spawn(async move {
            if let Err(e) = tui::run_tui(tui_app, shutdown_rx_tui).await {
                error!("TUI error: {}", e);
            }
        }))
    } else {
        // Still need to handle shutdown_rx even without TUI
        drop(shutdown_rx);
        None
    };

    // Set up graceful shutdown signal handler
    let proxy_for_shutdown = proxy.clone();
    let shutdown_tx_clone = shutdown_tx.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("Shutdown signal received, closing idle connections...");

        // Notify TUI to exit
        let _ = shutdown_tx_clone.send(()).await;

        proxy_for_shutdown.graceful_shutdown().await;
        info!("Graceful shutdown complete");
        std::process::exit(0);
    });

    // Accept connections
    let uses_per_command_routing = args.routing_mode.supports_per_command_routing();

    loop {
        match listener.accept().await {
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

    // Note: The loop above runs forever, so this code is unreachable
    // The process exits via the shutdown signal handler
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
