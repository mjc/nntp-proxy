#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use nntp_proxy::{
    CommonArgs, NntpProxy, RuntimeConfig, UiMode, metrics::MetricsStore, runtime, tui,
};

#[derive(Parser, Debug)]
#[command(author, version, about = "NNTP Proxy server", long_about = None)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let (mut config, _) = runtime::load_and_log_config(args.common.config.as_str())?;

    // Apply CLI argument overrides to config
    args.common.apply_overrides(&mut config);

    let ui_mode = args.common.effective_ui_mode();
    let log_buffer = nntp_proxy::logging::init_logging(ui_mode, &config.proxy.log_file_level);

    let threads = args.common.threads.or(Some(config.proxy.threads));
    RuntimeConfig::from_args(threads)
        .build_runtime()?
        .block_on(run_proxy(args, config, ui_mode, log_buffer))
}

async fn run_proxy(
    args: Args,
    config: nntp_proxy::config::Config,
    ui_mode: UiMode,
    log_buffer: Option<tui::LogBuffer>,
) -> Result<()> {
    let routing_mode = args
        .common
        .routing_mode
        .unwrap_or(config.routing.routing_mode);
    let (host, port) =
        runtime::resolve_listen_address(args.common.host.as_deref(), args.common.port, &config);

    let stats_path = runtime::resolve_stats_file_path(
        args.common.config.as_str(),
        config.proxy.stats_file.as_ref(),
    );
    let availability_path =
        runtime::resolve_availability_file_path(args.common.config.as_str(), config.cache.as_ref());
    let server_names: Vec<String> = config
        .servers
        .iter()
        .map(|s| s.name.as_ref().to_string())
        .collect();
    let metrics_store = runtime::load_metrics_from_disk(&stats_path, &server_names);

    let proxy = build_proxy(config, routing_mode, metrics_store).await?;
    if let Some(path) = availability_path.as_ref() {
        let _ = runtime::load_availability_from_disk(proxy.cache(), path);
    }

    let listener = runtime::bind_listener(&host, port, routing_mode).await?;

    // Prewarm connections BEFORE accepting clients (must complete first to avoid exceeding limits)
    info!("Prewarming connection pools...");
    proxy.prewarm_connections().await?;
    info!("Connection pools ready, accepting clients");

    runtime::spawn_stats_flusher(proxy.connection_stats());
    runtime::spawn_cache_stats_logger(&proxy);
    runtime::spawn_metrics_saver(&proxy, stats_path.clone(), server_names.clone());
    runtime::spawn_availability_saver(&proxy, availability_path.clone());
    runtime::spawn_idle_connection_clearer(&proxy);

    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);
    let (tui_handle, tui_shutdown_tx) =
        launch_tui(ui_mode, &proxy, log_buffer, shutdown_tx.clone())?;
    let error_shutdown_tx = tui_shutdown_tx.clone();
    spawn_signal_forwarder(shutdown_tx, tui_shutdown_tx);

    let accept_result =
        runtime::run_accept_loop(proxy.clone(), listener, shutdown_rx, routing_mode).await;

    if accept_result.is_err()
        && let Some(tx) = error_shutdown_tx
    {
        let _ = tx.send(()).await;
    }

    if let Some(handle) = tui_handle {
        handle.await?;
    }

    if let Err(e) =
        runtime::persist_runtime_state(&proxy, stats_path, availability_path, server_names).await
    {
        warn!("Failed to persist runtime state after shutdown: {}", e);
    }

    accept_result
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

type TuiHandle = tokio::task::JoinHandle<()>;

fn launch_tui(
    ui_mode: UiMode,
    proxy: &Arc<NntpProxy>,
    log_buffer: Option<tui::LogBuffer>,
    shutdown_tx: mpsc::Sender<()>,
) -> Result<(Option<TuiHandle>, Option<mpsc::Sender<()>>)> {
    if !ui_mode.uses_tui() {
        return Ok((None, None));
    }

    let (tui_shutdown_tx, tui_shutdown_rx) = mpsc::channel::<()>(1);

    info!("Building TUI dashboard...");
    let mut builder = tui::TuiAppBuilder::new(
        proxy.metrics().clone(),
        proxy.router().clone(),
        Arc::from(proxy.servers()),
    );

    if let Some(buffer) = log_buffer {
        builder = builder.with_log_buffer(buffer);
    }

    let cache = proxy.cache();
    info!(
        "Adding cache to TUI: entries={}, size={}",
        cache.entry_count(),
        cache.weighted_size()
    );
    builder = builder.with_cache(cache.clone());
    builder = builder.with_buffer_pool(proxy.buffer_pool().clone());

    let tui_app = builder.build();
    let handle = tokio::spawn(async move {
        info!("Initializing TUI dashboard...");
        if let Err(e) = tui::run_tui(tui_app, shutdown_tx, tui_shutdown_rx).await {
            error!("TUI error: {}", e);
        }
        info!("TUI closed");
    });

    Ok((Some(handle), Some(tui_shutdown_tx)))
}

fn spawn_signal_forwarder(
    shutdown_tx: mpsc::Sender<()>,
    tui_shutdown_tx: Option<mpsc::Sender<()>>,
) {
    tokio::spawn(async move {
        runtime::shutdown_signal().await;
        info!("Shutdown signal received");

        if let Some(tx) = tui_shutdown_tx {
            let _ = tx.send(()).await;
        }
        let _ = shutdown_tx.send(()).await;
    });
}
