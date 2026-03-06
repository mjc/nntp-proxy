#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use nntp_proxy::{CommonArgs, NntpProxy, RuntimeConfig, metrics::MetricsStore, runtime, tui};

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
    let (mut config, _) = runtime::load_and_log_config(args.common.config.as_str())?;

    // Apply CLI argument overrides to config
    args.common.apply_overrides(&mut config);

    let log_buffer =
        nntp_proxy::logging::init_tui_logging(args.no_tui, &config.proxy.log_file_level);

    // Fix thread config fallback: merge args with config before building runtime
    let threads = args.common.threads.or(Some(config.proxy.threads));

    RuntimeConfig::from_args(threads)
        .build_runtime()?
        .block_on(run_proxy(args, config, log_buffer))
}

async fn run_proxy(
    args: Args,
    config: nntp_proxy::config::Config,
    log_buffer: Option<tui::LogBuffer>,
) -> Result<()> {
    let routing_mode = args
        .common
        .routing_mode
        .unwrap_or(config.proxy.routing_mode);
    let (host, port) =
        runtime::resolve_listen_address(args.common.host.as_deref(), args.common.port, &config);

    // Resolve stats file path and load metrics if available
    let stats_path = runtime::resolve_stats_file_path(
        args.common.config.as_str(),
        config.proxy.stats_file.as_ref(),
    );
    let server_names: Vec<String> = config
        .servers
        .iter()
        .map(|s| s.name.as_ref().to_string())
        .collect();
    let metrics_store = runtime::load_metrics_from_disk(&stats_path, &server_names);

    let proxy = build_proxy(config, routing_mode, metrics_store).await?;
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);
    let (tui_shutdown_tx, tui_shutdown_rx) = mpsc::channel::<()>(1);

    let tui_handle = launch_tui(
        &args,
        &proxy,
        log_buffer,
        shutdown_tx.clone(),
        tui_shutdown_rx,
    )?;
    let listener = runtime::bind_listener(&host, port, routing_mode).await?;

    // Prewarm connections BEFORE accepting clients (must complete first to avoid exceeding limits)
    info!("Prewarming connection pools...");
    proxy.prewarm_connections().await?;
    info!("Connection pools ready, accepting clients");

    runtime::spawn_stats_flusher(proxy.connection_stats());
    runtime::spawn_cache_stats_logger(&proxy);
    runtime::spawn_metrics_saver(&proxy, stats_path.clone(), server_names.clone());

    spawn_tui_shutdown_handler(
        &proxy,
        &shutdown_tx,
        tui_shutdown_tx,
        stats_path,
        server_names,
    );

    runtime::run_accept_loop(proxy, listener, shutdown_rx, routing_mode).await?;

    if let Some(handle) = tui_handle {
        handle.await?;
    }

    Ok(())
}

async fn build_proxy(
    config: nntp_proxy::config::Config,
    routing_mode: nntp_proxy::RoutingMode,
    metrics_store: Option<MetricsStore>,
) -> Result<Arc<NntpProxy>> {
    let mut builder = NntpProxy::builder(config).with_routing_mode(routing_mode);
    if let Some(store) = metrics_store {
        builder = builder.with_metrics_store(store);
    }
    Ok(Arc::new(builder.build().await?))
}

fn launch_tui(
    args: &Args,
    proxy: &Arc<NntpProxy>,
    log_buffer: Option<tui::LogBuffer>,
    shutdown_tx: mpsc::Sender<()>,
    tui_shutdown_rx: mpsc::Receiver<()>,
) -> Result<Option<tokio::task::JoinHandle<()>>> {
    (!args.no_tui)
        .then(|| {
            info!("Building TUI dashboard...");
            let mut builder = tui::TuiAppBuilder::new(
                proxy.metrics().clone(),
                proxy.router().clone(),
                proxy.servers().to_vec().into(),
            );

            // Add log buffer if available
            if let Some(buffer) = log_buffer {
                builder = builder.with_log_buffer(buffer);
            }

            // Add cache (always available now)
            let cache = proxy.cache();
            info!(
                "Adding cache to TUI: entries={}, size={}",
                cache.entry_count(),
                cache.weighted_size()
            );
            builder = builder.with_cache(cache.clone());

            // Add buffer pool for monitoring
            builder = builder.with_buffer_pool(proxy.buffer_pool().clone());

            let tui_app = builder.build();

            tokio::spawn(async move {
                info!("Initializing TUI dashboard...");
                if let Err(e) = tui::run_tui(tui_app, shutdown_tx, tui_shutdown_rx).await {
                    error!("TUI error: {}", e);
                }
                info!("TUI closed. Shutting down proxy...");
                info!("TUI exited, initiating shutdown");
            })
        })
        .map(Ok)
        .transpose()
}

fn spawn_tui_shutdown_handler(
    proxy: &Arc<NntpProxy>,
    shutdown_tx: &mpsc::Sender<()>,
    tui_shutdown_tx: mpsc::Sender<()>,
    stats_path: PathBuf,
    server_names: Vec<String>,
) {
    let proxy = Arc::clone(proxy);
    let shutdown_tx = shutdown_tx.clone();

    tokio::spawn(async move {
        runtime::shutdown_signal().await;
        info!("Shutdown signal received");

        let _ = tui_shutdown_tx.send(()).await;
        let _ = shutdown_tx.send(()).await;

        // Save metrics before shutting down
        if let Err(e) = proxy.metrics().save_to_disk(&stats_path, &server_names) {
            warn!("Failed to save metrics on shutdown: {}", e);
        } else {
            info!("Metrics saved to {}", stats_path.display());
        }

        proxy.graceful_shutdown().await;
        info!("Graceful shutdown complete");
    });
}
