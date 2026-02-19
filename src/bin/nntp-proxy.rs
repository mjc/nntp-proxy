#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use anyhow::Result;
use clap::Parser;
use std::sync::Arc;
use tracing::info;

use nntp_proxy::{CommonArgs, NntpProxy, RuntimeConfig, runtime};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let (mut config, _) = runtime::load_and_log_config(args.common.config.as_str())?;

    // Apply CLI argument overrides to config
    args.common.apply_overrides(&mut config);

    nntp_proxy::logging::init_dual_logging(&config.proxy.log_file_level);

    build_runtime(&args, &config)?.block_on(run_proxy(args, config))
}

fn build_runtime(
    args: &Args,
    config: &nntp_proxy::config::Config,
) -> Result<tokio::runtime::Runtime> {
    let threads = args.common.threads.or(Some(config.proxy.threads));
    RuntimeConfig::from_args(threads).build_runtime()
}

async fn run_proxy(args: Args, config: nntp_proxy::config::Config) -> Result<()> {
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

    // Build proxy with restored metrics
    let mut builder = NntpProxy::builder(config).with_routing_mode(routing_mode);
    if let Some(store) = metrics_store {
        builder = builder.with_metrics_store(store);
    }
    let proxy = Arc::new(builder.build().await?);

    let listener = runtime::bind_listener(&host, port, routing_mode).await?;

    // Prewarm connections BEFORE accepting clients (must complete first to avoid exceeding limits)
    info!("Prewarming connection pools...");
    proxy.prewarm_connections().await?;
    info!("Connection pools ready, accepting clients");

    runtime::spawn_stats_flusher(proxy.connection_stats());
    runtime::spawn_cache_stats_logger(&proxy);
    runtime::spawn_metrics_saver(&proxy, stats_path.clone(), server_names.clone());

    let shutdown_rx = runtime::spawn_shutdown_handler(&proxy, stats_path, server_names);

    runtime::run_accept_loop(proxy, listener, shutdown_rx, routing_mode).await
}
