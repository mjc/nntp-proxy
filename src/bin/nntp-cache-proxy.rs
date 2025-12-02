use anyhow::{Context, Result};
use clap::Parser;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info};

use nntp_proxy::auth::AuthHandler;
use nntp_proxy::cache::ArticleCache;
use nntp_proxy::config::Cache;
use nntp_proxy::network::{NetworkOptimizer, TcpOptimizer};
use nntp_proxy::{CacheArgs, CommonArgs, NntpProxy, RuntimeConfig, runtime};

#[derive(Parser, Debug)]
#[command(author, version, about = "NNTP Caching Proxy Server", long_about = None)]
struct Args {
    #[command(flatten)]
    common: CommonArgs,

    #[command(flatten)]
    cache: CacheArgs,
}

fn main() -> Result<()> {
    init_logging();

    let args = Args::parse();

    RuntimeConfig::from_args(args.common.threads)
        .build_runtime()?
        .block_on(run_caching_proxy(args))
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

async fn run_caching_proxy(args: Args) -> Result<()> {
    let (config, _) = runtime::load_and_log_config(args.common.config.as_str())?;

    let cache_config = resolve_cache_config(&config, &args);
    let cache = create_article_cache(&cache_config);
    let proxy = create_caching_proxy(config.clone())?;
    let auth_handler = create_auth_handler(&config)?;

    let listen_addr = args.common.listen_addr(Some(config.proxy.port));
    let listener = tokio::net::TcpListener::bind(&listen_addr).await?;
    info!(
        "NNTP caching proxy listening on {} (caching mode)",
        listen_addr
    );

    let _shutdown_rx = runtime::spawn_shutdown_handler(&proxy);
    spawn_cache_stats_logger(&cache);

    accept_caching_clients(listener, proxy, cache, auth_handler).await
}

fn resolve_cache_config(config: &nntp_proxy::config::Config, args: &Args) -> Cache {
    config.cache.clone().unwrap_or_else(|| Cache {
        max_capacity: args.cache.cache_capacity,
        ttl: args.cache.ttl(),
    })
}

fn create_article_cache(cache_config: &Cache) -> Arc<ArticleCache> {
    info!(
        "Cache configuration: max_capacity={}, ttl={:?}",
        cache_config.max_capacity.get(),
        cache_config.ttl
    );

    Arc::new(ArticleCache::new(
        cache_config.max_capacity.get() as u64,
        cache_config.ttl,
    ))
}

fn create_caching_proxy(config: nntp_proxy::config::Config) -> Result<Arc<NntpProxy>> {
    Ok(Arc::new(NntpProxy::new(
        config,
        nntp_proxy::RoutingMode::PerCommand,
    )?))
}

fn create_auth_handler(config: &nntp_proxy::config::Config) -> Result<Arc<AuthHandler>> {
    AuthHandler::new(
        config.client_auth.username.clone(),
        config.client_auth.password.clone(),
    )
    .map(Arc::new)
    .with_context(|| {
        "Invalid authentication configuration. \
         If you set username/password in config, they cannot be empty. \
         Remove them entirely to disable authentication."
    })
}

fn spawn_cache_stats_logger(cache: &Arc<ArticleCache>) {
    let cache = Arc::clone(cache);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            let stats = cache.stats().await;
            info!(
                "Cache stats: entries={}, size={}",
                stats.entry_count, stats.weighted_size
            );
        }
    });
}

async fn accept_caching_clients(
    listener: tokio::net::TcpListener,
    proxy: Arc<NntpProxy>,
    cache: Arc<ArticleCache>,
    auth_handler: Arc<AuthHandler>,
) -> Result<()> {
    loop {
        let (stream, addr) = listener.accept().await?;
        let (proxy, cache, auth) = (proxy.clone(), cache.clone(), auth_handler.clone());

        tokio::spawn(async move {
            handle_caching_client(proxy, cache, auth, stream, addr)
                .await
                .unwrap_or_else(|e| error!("Error handling client {}: {}", addr, e))
        });
    }
}

async fn handle_caching_client(
    proxy: Arc<NntpProxy>,
    cache: Arc<ArticleCache>,
    _auth_handler: Arc<AuthHandler>,
    stream: tokio::net::TcpStream,
    addr: std::net::SocketAddr,
) -> Result<()> {
    debug!("New caching client connection from {}", addr);

    TcpOptimizer::new(&stream)
        .optimize()
        .unwrap_or_else(|e| debug!("Failed to optimize client socket: {}", e));

    proxy
        .handle_client_with_cache(stream, addr.into(), cache)
        .await
}
