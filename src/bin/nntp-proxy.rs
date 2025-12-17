use anyhow::Result;
use clap::Parser;
use std::sync::Arc;

use nntp_proxy::{CommonArgs, NntpProxy, RuntimeConfig, runtime};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,
}

fn main() -> Result<()> {
    nntp_proxy::logging::init_dual_logging();

    let args = Args::parse();
    let (config, _) = runtime::load_and_log_config(args.common.config.as_str())?;

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
    let routing_mode = args.common.routing_mode;
    let (host, port) =
        runtime::resolve_listen_address(args.common.host.as_deref(), args.common.port, &config);

    let proxy = Arc::new(NntpProxy::new(config, routing_mode)?);
    let listener = runtime::bind_listener(&host, port, routing_mode).await?;

    runtime::spawn_connection_prewarming(&proxy);
    runtime::spawn_stats_flusher(proxy.connection_stats());
    runtime::spawn_cache_stats_logger(&proxy);

    let shutdown_rx = runtime::spawn_shutdown_handler(&proxy);

    runtime::run_accept_loop(proxy, listener, shutdown_rx, routing_mode).await
}
