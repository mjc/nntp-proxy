use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{error, info, warn};

use nntp_proxy::{NntpProxy, create_default_config, load_config};

/// Pin current process to specific CPU cores for optimal performance
#[cfg(target_os = "linux")]
fn pin_to_cpu_cores(num_cores: usize) -> Result<()> {
    use nix::sched::{CpuSet, sched_setaffinity};
    use nix::unistd::Pid;

    // Pin to CPU cores based on number of worker threads
    // This reduces context switching and improves cache locality
    let mut cpu_set = CpuSet::new();
    for core in 0..num_cores {
        cpu_set.set(core)?;
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
#[command(author, version, about, long_about = None)]
struct Args {
    /// Port to listen on
    ///
    /// Can be overridden with NNTP_PROXY_PORT environment variable
    #[arg(short, long, default_value = "8119", env = "NNTP_PROXY_PORT")]
    port: u16,

    /// Enable per-command routing mode (each command can use a different backend)
    ///
    /// Can be overridden with NNTP_PROXY_PER_COMMAND_ROUTING environment variable
    #[arg(
        short = 'r',
        long,
        default_value = "false",
        env = "NNTP_PROXY_PER_COMMAND_ROUTING"
    )]
    per_command_routing: bool,

    /// Configuration file path
    ///
    /// Can be overridden with NNTP_PROXY_CONFIG environment variable
    #[arg(short, long, default_value = "config.toml", env = "NNTP_PROXY_CONFIG")]
    config: String,

    /// Number of worker threads (defaults to number of CPU cores)
    ///
    /// Can be overridden with NNTP_PROXY_THREADS environment variable
    #[arg(short, long, env = "NNTP_PROXY_THREADS")]
    threads: Option<usize>,
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

    // Log threading info
    let num_cpus = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1);
    let worker_threads = args.threads.unwrap_or(num_cpus);

    // Pin to specific CPU cores for optimal performance
    pin_to_cpu_cores(worker_threads)?;

    // Use different runtime based on thread count for optimal performance
    if worker_threads == 1 {
        info!("Starting NNTP proxy with single-threaded runtime for optimal performance");
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
    let config = if std::path::Path::new(&args.config).exists() {
        // File exists, try to load it
        match load_config(&args.config) {
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
    } else {
        // File doesn't exist, create default
        warn!(
            "Config file '{}' not found, creating default config",
            args.config
        );
        let default_config = create_default_config();
        let config_toml = toml::to_string_pretty(&default_config)?;
        std::fs::write(&args.config, &config_toml)?;
        info!("Created default config file: {}", args.config);
        default_config
    };

    info!("Loaded {} backend servers:", config.servers.len());
    for server in &config.servers {
        info!("  - {} ({}:{})", server.name, server.host, server.port);
    }

    // Create proxy (wrapped in Arc for sharing across tasks)
    let proxy = Arc::new(NntpProxy::new(config)?);

    // Prewarm connection pools before accepting clients
    info!("Prewarming connection pools...");
    if let Err(e) = proxy.prewarm_connections().await {
        warn!("Failed to prewarm connection pools: {}", e);
    }
    info!("Connection pools ready");

    // Start listening
    let listen_addr = format!("0.0.0.0:{}", args.port);
    let listener = TcpListener::bind(&listen_addr).await?;
    if args.per_command_routing {
        info!(
            "NNTP proxy listening on {} (per-command routing mode)",
            listen_addr
        );
    } else {
        info!("NNTP proxy listening on {} (1:1 mode)", listen_addr);
    }

    // Set up graceful shutdown
    let proxy_for_shutdown = proxy.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("Shutdown signal received, closing idle connections...");
        proxy_for_shutdown.graceful_shutdown().await;
        info!("Graceful shutdown complete");
        std::process::exit(0);
    });

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let proxy_clone = proxy.clone();
                if args.per_command_routing {
                    // Per-command routing mode
                    tokio::spawn(async move {
                        if let Err(e) = proxy_clone
                            .handle_client_per_command_routing(stream, addr)
                            .await
                        {
                            error!("Error handling client {}: {}", addr, e);
                        }
                    });
                } else {
                    // Traditional 1:1 mode
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
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
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
