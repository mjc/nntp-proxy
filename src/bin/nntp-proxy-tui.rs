use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};

use nntp_proxy::{CommonArgs, NntpProxy, RuntimeConfig, runtime, tui};

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
    let log_buffer = nntp_proxy::logging::init_tui_logging(args.no_tui);

    RuntimeConfig::from_args(args.common.threads)
        .build_runtime()?
        .block_on(run_proxy(args, log_buffer))
}

async fn run_proxy(args: Args, log_buffer: Option<tui::LogBuffer>) -> Result<()> {
    let (config, _) = runtime::load_and_log_config(args.common.config.as_str())?;
    let routing_mode = args.common.routing_mode;
    let (host, port) =
        runtime::resolve_listen_address(args.common.host.as_deref(), args.common.port, &config);

    let proxy = build_proxy_with_metrics(config, routing_mode).await?;
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

    runtime::spawn_connection_prewarming(&proxy);
    runtime::spawn_stats_flusher(proxy.connection_stats());
    runtime::spawn_cache_stats_logger(&proxy);
    spawn_tui_shutdown_handler(&proxy, &shutdown_tx, tui_shutdown_tx);

    runtime::run_accept_loop(proxy, listener, shutdown_rx, routing_mode).await?;

    if let Some(handle) = tui_handle {
        handle.await?;
    }

    Ok(())
}

async fn build_proxy_with_metrics(
    config: nntp_proxy::config::Config,
    routing_mode: nntp_proxy::RoutingMode,
) -> Result<Arc<NntpProxy>> {
    Ok(Arc::new(
        NntpProxy::builder(config)
            .with_routing_mode(routing_mode)
            .with_metrics()
            .build()
            .await?,
    ))
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

            let tui_app = builder.build();

            tokio::spawn(async move {
                if let Err(e) = tui::run_tui(tui_app, shutdown_tx, tui_shutdown_rx).await {
                    error!("TUI error: {}", e);
                }
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
) {
    let proxy = Arc::clone(proxy);
    let shutdown_tx = shutdown_tx.clone();

    tokio::spawn(async move {
        runtime::shutdown_signal().await;
        info!("Shutdown signal received");

        let _ = tui_shutdown_tx.send(()).await;
        let _ = shutdown_tx.send(()).await;

        proxy.graceful_shutdown().await;
        info!("Graceful shutdown complete");
    });
}
