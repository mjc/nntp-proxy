use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::signal;
use tracing::{error, info, warn};

use nntp_proxy::{create_default_config, load_config, NntpProxy};
use nntp_proxy::cache::ArticleCache;
use nntp_proxy::cache::CachingSession;
use nntp_proxy::config::CacheConfig;
use nntp_proxy::network::SocketOptimizer;
use nntp_proxy::protocol::NNTP_BACKEND_UNAVAILABLE;
use nntp_proxy::types::ClientId;

/// Pin current process to specific CPU cores for optimal performance
#[cfg(target_os = "linux")]
fn pin_to_cpu_cores(num_cores: usize) -> Result<()> {
    use nix::sched::{CpuSet, sched_setaffinity};
    use nix::unistd::Pid;

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
#[command(author, version, about = "NNTP Caching Proxy Server", long_about = None)]
struct Args {
    /// Port to listen on
    #[arg(short, long, default_value = "8120")]
    port: u16,

    /// Configuration file path
    #[arg(short, long, default_value = "cache-config.toml")]
    config: String,

    /// Number of worker threads (defaults to number of CPU cores)
    #[arg(short, long)]
    threads: Option<usize>,

    /// Cache max capacity (number of articles)
    #[arg(long, default_value = "10000")]
    cache_capacity: u64,

    /// Cache TTL in seconds
    #[arg(long, default_value = "3600")]
    cache_ttl: u64,
}

fn main() -> Result<()> {
    // Initialize tracing first
    tracing_subscriber::fmt::init();

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
        info!("Starting NNTP caching proxy with single-threaded runtime for optimal performance");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(run_caching_proxy(args))
    } else {
        info!(
            "Starting NNTP caching proxy with {} worker threads (detected {} CPUs)",
            worker_threads, num_cpus
        );
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .enable_all()
            .build()?;
        rt.block_on(run_caching_proxy(args))
    }
}

async fn run_caching_proxy(args: Args) -> Result<()> {
    // Load configuration
    let config = if std::path::Path::new(&args.config).exists() {
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

    // Set up cache configuration
    let cache_config = config.cache.clone().unwrap_or(CacheConfig {
        max_capacity: args.cache_capacity,
        ttl_secs: args.cache_ttl,
    });

    info!(
        "Cache configuration: max_capacity={}, ttl={}s",
        cache_config.max_capacity, cache_config.ttl_secs
    );

    // Create article cache
    let cache = Arc::new(ArticleCache::new(
        cache_config.max_capacity,
        Duration::from_secs(cache_config.ttl_secs),
    ));

    info!("Loaded {} backend servers:", config.servers.len());
    for server in &config.servers {
        info!("  - {} ({}:{})", server.name, server.host, server.port);
    }

    // Create proxy
    let proxy = Arc::new(NntpProxy::new(config)?);

    // Start listening
    let listen_addr = format!("0.0.0.0:{}", args.port);
    let listener = TcpListener::bind(&listen_addr).await?;
    info!(
        "NNTP caching proxy listening on {} (caching mode)",
        listen_addr
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

    // Periodically log cache statistics
    let cache_for_stats = cache.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let stats = cache_for_stats.stats().await;
            info!(
                "Cache stats: entries={}, size={}",
                stats.entry_count, stats.weighted_size
            );
        }
    });

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let proxy_clone = proxy.clone();
                let cache_clone = cache.clone();

                tokio::spawn(async move {
                    if let Err(e) = handle_caching_client(proxy_clone, cache_clone, stream, addr).await {
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

async fn handle_caching_client(
    proxy: Arc<NntpProxy>,
    cache: Arc<ArticleCache>,
    mut client_stream: tokio::net::TcpStream,
    client_addr: std::net::SocketAddr,
) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    use tracing::debug;

    debug!("New caching client connection from {}", client_addr);

    // Select backend using round-robin
    let client_id = ClientId::new();
    let backend_id = proxy.router().route_command_sync(client_id, "")?;
    let server_idx = backend_id.as_index();
    let servers = proxy.servers();
    let server = &servers[server_idx];

    info!(
        "Routing client {} to backend {:?} ({}:{})",
        client_addr, backend_id, server.host, server.port
    );

    // Send greeting
    nntp_proxy::protocol::send_proxy_greeting(&mut client_stream, client_addr).await?;

    // Get pooled backend connection
    let backend_conn = match proxy.connection_providers()[server_idx]
        .get_pooled_connection()
        .await
    {
        Ok(conn) => {
            debug!("Got pooled connection for {}", server.name);
            conn
        }
        Err(e) => {
            error!("Failed to get pooled connection for {}: {}", server.name, e);
            let _ = client_stream.write_all(NNTP_BACKEND_UNAVAILABLE).await;
            return Err(e);
        }
    };

    // Apply socket optimizations
    if let Err(e) = SocketOptimizer::apply_to_streams(&client_stream, &backend_conn) {
        debug!("Failed to apply socket optimizations: {}", e);
    }

    // Create caching session and handle connection
    let session = CachingSession::new(
        client_addr,
        cache,
    );

    debug!("Starting caching session for client {}", client_addr);

    let copy_result = session
        .handle_with_pooled_backend(client_stream, backend_conn)
        .await;

    debug!("Caching session completed for client {}", client_addr);

    // Complete the routing
    proxy.router().complete_command_sync(backend_id);

    // Log session results
    match copy_result {
        Ok((client_to_backend_bytes, backend_to_client_bytes)) => {
            info!(
                "Connection closed for client {}: {} bytes sent, {} bytes received",
                client_addr, client_to_backend_bytes, backend_to_client_bytes
            );
        }
        Err(e) => {
            warn!("Session error for client {}: {}", client_addr, e);
        }
    }

    debug!("Connection returned to pool for {}", server.name);
    Ok(())
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
