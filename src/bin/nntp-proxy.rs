use anyhow::Result;
use clap::Parser;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
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
    args.common.validate_runtime_mode()?;
    let ui_mode = args.common.effective_ui_mode();
    let capture_headless_tui_buffer =
        ui_mode == UiMode::Headless && args.common.tui_listen.is_some();

    if ui_mode == UiMode::Tui
        && let Some(connect_addr) = args.common.tui_attach
    {
        let file_level = nntp_proxy::Config::default().proxy.log_file_level;
        let _ = nntp_proxy::logging::init_logging(UiMode::Tui, &file_level, false, false);
        let threads = args.common.threads;
        return RuntimeConfig::from_args(threads)
            .build_runtime()?
            .block_on(tui::run_attached_tui(connect_addr));
    }

    let startup_tui = if ui_mode == UiMode::Tui {
        Some(tui::start_startup_tui_session()?)
    } else {
        None
    };

    let (mut config, _) = runtime::load_and_log_config(args.common.config.as_str())?;

    // Apply CLI argument overrides to config
    args.common.apply_overrides(&mut config);

    let write_debug_log = nntp_proxy::logging::should_write_debug_log(
        ui_mode,
        args.common.tui_attach.is_some(),
        &config.proxy.log_file_level,
    );
    let capture_in_memory_logs = nntp_proxy::logging::should_capture_in_memory_logs(
        ui_mode,
        args.common.tui_attach.is_some(),
        capture_headless_tui_buffer,
    );
    let log_buffer = nntp_proxy::logging::init_logging(
        ui_mode,
        &config.proxy.log_file_level,
        capture_in_memory_logs,
        write_debug_log,
    );

    let threads = args.common.threads.or(Some(config.proxy.threads));
    RuntimeConfig::from_args(threads)
        .build_runtime()?
        .block_on(run_proxy(args, config, ui_mode, log_buffer, startup_tui))
}

async fn run_proxy(
    args: Args,
    config: nntp_proxy::config::Config,
    ui_mode: UiMode,
    log_buffer: Option<tui::LogBuffer>,
    startup_tui: Option<tui::StartupTuiSession>,
) -> Result<()> {
    let launch = prepare_proxy_launch(&args, &config);
    let response_write_metrics_period = config
        .proxy
        .response_write_metrics_secs
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs);
    let client_writer_lock_metrics_period = config
        .proxy
        .client_writer_lock_metrics_secs
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs);
    args.common
        .validate_dashboard_listen(&launch.host, launch.port)?;

    let proxy = build_proxy(config, launch.routing_mode, launch.metrics_store).await?;

    let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

    if let Some(path) = launch.availability_path.as_ref() {
        let _ = runtime::load_availability_from_disk(proxy.cache(), path);
    }

    let listener = runtime::bind_listener(&launch.host, launch.port, launch.routing_mode).await?;

    runtime::spawn_stats_flusher(proxy.connection_stats());
    runtime::spawn_cache_stats_logger(&proxy);
    runtime::spawn_metrics_saver(
        &proxy,
        launch.stats_path.clone(),
        launch.server_names.clone(),
    );
    runtime::spawn_availability_saver(&proxy, launch.availability_path.clone());
    runtime::spawn_idle_connection_clearer(&proxy);
    runtime::spawn_response_write_metrics_logger(response_write_metrics_period);
    runtime::spawn_client_writer_lock_metrics_logger(client_writer_lock_metrics_period);
    runtime::spawn_tokio_runtime_metrics_logger();

    let (dashboard_handle, dashboard_shutdown_tx) =
        launch_dashboard_publisher(args.common.tui_listen, &proxy, log_buffer.clone()).await?;
    let (tui_handle, tui_shutdown_tx) = launch_tui(
        ui_mode,
        &proxy,
        log_buffer,
        shutdown_tx.clone(),
        startup_tui,
    );
    let error_tui_shutdown_tx = tui_shutdown_tx.clone();
    let error_dashboard_shutdown_tx = dashboard_shutdown_tx.clone();
    spawn_signal_forwarder(shutdown_tx, tui_shutdown_tx, dashboard_shutdown_tx);

    // Prewarm connections BEFORE accepting clients (must complete first to avoid exceeding limits).
    // The TUI is already running so interactive launches draw immediately while prewarm proceeds.
    info!("Prewarming connection pools...");
    let prewarm_result = tokio::select! {
        result = proxy.prewarm_connections() => result,
        _ = shutdown_rx.recv() => {
            info!("Shutdown requested before connection pools finished prewarming");
            shutdown_background_tasks(
                error_tui_shutdown_tx,
                error_dashboard_shutdown_tx,
                tui_handle,
                dashboard_handle,
            ).await?;
            return Ok(());
        }
    };

    if let Err(e) = prewarm_result {
        if let Some(tx) = error_tui_shutdown_tx {
            send_shutdown_signal(&tx);
        }
        if let Some(tx) = error_dashboard_shutdown_tx {
            send_shutdown_signal(&tx);
        }
        if let Some(handle) = tui_handle {
            handle.await?;
        }
        if let Some(handle) = dashboard_handle {
            handle.await?;
        }
        return Err(e);
    }
    info!("Connection pools ready, accepting clients");

    let accept_result =
        runtime::run_accept_loop(proxy.clone(), listener, shutdown_rx, launch.routing_mode).await;

    shutdown_background_tasks(
        error_tui_shutdown_tx,
        error_dashboard_shutdown_tx,
        tui_handle,
        dashboard_handle,
    )
    .await?;

    if let Err(e) = runtime::persist_runtime_state(
        &proxy,
        launch.stats_path,
        launch.availability_path,
        launch.server_names,
    )
    .await
    {
        warn!("Failed to persist runtime state after shutdown: {}", e);
    }

    accept_result
}

async fn shutdown_background_tasks(
    tui_shutdown_tx: Option<mpsc::Sender<()>>,
    dashboard_shutdown_tx: Option<mpsc::Sender<()>>,
    tui_handle: Option<TuiHandle>,
    dashboard_handle: Option<TuiHandle>,
) -> Result<()> {
    if let Some(tx) = tui_shutdown_tx {
        send_shutdown_signal(&tx);
    }
    if let Some(tx) = dashboard_shutdown_tx {
        send_shutdown_signal(&tx);
    }

    if let Some(handle) = tui_handle {
        await_ui_task_with_timeout(handle, "TUI").await?;
    }

    if let Some(handle) = dashboard_handle {
        await_ui_task_with_timeout(handle, "dashboard").await?;
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

type TuiHandle = tokio::task::JoinHandle<()>;

struct ProxyLaunch {
    routing_mode: nntp_proxy::RoutingMode,
    host: String,
    port: nntp_proxy::types::Port,
    stats_path: PathBuf,
    availability_path: Option<PathBuf>,
    server_names: Vec<String>,
    metrics_store: Option<MetricsStore>,
}

/// Resolve the runtime launch parameters from CLI arguments and config.
fn prepare_proxy_launch(args: &Args, config: &nntp_proxy::config::Config) -> ProxyLaunch {
    let routing_mode = args
        .common
        .routing_mode
        .unwrap_or(config.routing.routing_mode);
    let (host, port) =
        runtime::resolve_listen_address(args.common.host.as_deref(), args.common.port, config);
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

    ProxyLaunch {
        routing_mode,
        host,
        port,
        stats_path,
        availability_path,
        server_names,
        metrics_store,
    }
}

/// Launch the local in-process TUI when `--ui tui` is selected.
fn launch_tui(
    ui_mode: UiMode,
    proxy: &Arc<NntpProxy>,
    log_buffer: Option<tui::LogBuffer>,
    shutdown_tx: mpsc::Sender<()>,
    startup_tui: Option<tui::StartupTuiSession>,
) -> (Option<TuiHandle>, Option<mpsc::Sender<()>>) {
    if !ui_mode.uses_tui() {
        return (None, None);
    }

    let (tui_shutdown_tx, tui_shutdown_rx) = mpsc::channel::<()>(1);

    info!("Building TUI dashboard...");
    let cache = proxy.cache();
    info!(
        "Adding cache to TUI: entries={}, size={}",
        cache.entry_count(),
        cache.weighted_size()
    );
    let tui_app = build_dashboard_app(proxy, log_buffer);
    let handle = tokio::spawn(async move {
        info!("Initializing TUI dashboard...");
        let result = if let Some(session) = startup_tui {
            tui::run_tui_from_startup_session(session, tui_app, shutdown_tx, tui_shutdown_rx).await
        } else {
            tui::run_tui(tui_app, shutdown_tx, tui_shutdown_rx).await
        };
        if let Err(e) = result {
            error!("TUI error: {}", e);
        }
        info!("TUI closed");
    });

    (Some(handle), Some(tui_shutdown_tx))
}

/// Launch the websocket dashboard publisher when `--tui-listen` is configured.
///
/// # Errors
/// Returns an error if the dashboard listener cannot be bound or the task cannot
/// be prepared.
async fn launch_dashboard_publisher(
    listen_addr: Option<SocketAddr>,
    proxy: &Arc<NntpProxy>,
    log_buffer: Option<tui::LogBuffer>,
) -> Result<(Option<TuiHandle>, Option<mpsc::Sender<()>>)> {
    let Some(listen_addr) = listen_addr else {
        return Ok((None, None));
    };

    let (dashboard_shutdown_tx, dashboard_shutdown_rx) = mpsc::channel::<()>(1);

    info!("Building websocket dashboard publisher...");
    let tui_app = build_dashboard_app(proxy, log_buffer);
    let listener = tui::bind_dashboard_listener(listen_addr).await?;
    let handle = tokio::spawn(async move {
        info!("Initializing websocket dashboard on {}", listen_addr);
        if let Err(e) =
            tui::run_dashboard_publisher_on_listener(tui_app, listener, dashboard_shutdown_rx).await
        {
            error!("Websocket dashboard error: {}", e);
        }
        info!("Websocket dashboard closed");
    });

    Ok((Some(handle), Some(dashboard_shutdown_tx)))
}

fn build_dashboard_app(proxy: &Arc<NntpProxy>, log_buffer: Option<tui::LogBuffer>) -> tui::TuiApp {
    let mut builder = tui::TuiAppBuilder::new(
        proxy.metrics().clone(),
        proxy.router().clone(),
        Arc::from(proxy.servers()),
    );

    if let Some(buffer) = log_buffer {
        builder = builder.with_log_buffer(buffer);
    }

    builder
        .with_cache(proxy.cache().clone())
        .with_buffer_pool(proxy.buffer_pool().clone())
        .build()
}

fn spawn_signal_forwarder(
    shutdown_tx: mpsc::Sender<()>,
    tui_shutdown_tx: Option<mpsc::Sender<()>>,
    dashboard_shutdown_tx: Option<mpsc::Sender<()>>,
) {
    tokio::spawn(async move {
        runtime::shutdown_signal().await;
        info!("Shutdown signal received");

        if let Some(tx) = tui_shutdown_tx {
            send_shutdown_signal(&tx);
        }
        if let Some(tx) = dashboard_shutdown_tx {
            send_shutdown_signal(&tx);
        }
        send_shutdown_signal(&shutdown_tx);
    });
}

fn send_shutdown_signal(tx: &mpsc::Sender<()>) {
    match tx.try_send(()) {
        Ok(()) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {}
        Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
    }
}

async fn await_ui_task_with_timeout(handle: TuiHandle, label: &str) -> Result<()> {
    match tokio::time::timeout(
        nntp_proxy::constants::timeout::SHUTDOWN_UI_TASK_JOIN,
        handle,
    )
    .await
    {
        Ok(join_result) => {
            join_result?;
            Ok(())
        }
        Err(_) => {
            warn!(
                task = label,
                timeout_secs = nntp_proxy::constants::timeout::SHUTDOWN_UI_TASK_JOIN.as_secs(),
                "Timed out waiting for UI task during shutdown"
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nntp_proxy::config::Config;
    use nntp_proxy::types::Port;
    use std::path::PathBuf;
    use tokio::sync::mpsc;

    fn test_common_args() -> CommonArgs {
        CommonArgs {
            config: "config.toml".parse().unwrap(),
            ui: UiMode::Headless,
            tui: false,
            headless: false,
            no_tui: false,
            tui_listen: None,
            tui_attach: None,
            port: None,
            host: None,
            routing_mode: Some(nntp_proxy::RoutingMode::Stateful),
            backend_selection: None,
            article_cache_capacity: None,
            article_cache_ttl_secs: None,
            store_article_bodies: None,
            threads: None,
        }
    }

    #[test]
    fn prepare_proxy_launch_prefers_cli_routing_mode_and_host_port() {
        let args = Args {
            common: CommonArgs {
                routing_mode: Some(nntp_proxy::RoutingMode::Stateful),
                host: Some("127.0.0.1".to_string()),
                port: Some(Port::try_new(9120).unwrap()),
                ..test_common_args()
            },
        };
        let mut config = Config::default();
        config.proxy.stats_file = Some(PathBuf::from("proxy-stats.json"));
        config.servers = vec![
            nntp_proxy::config::Server::builder("server.example.com", Port::try_new(119).unwrap())
                .name("Primary")
                .build()
                .unwrap(),
        ];

        let launch = prepare_proxy_launch(&args, &config);

        assert_eq!(launch.routing_mode, nntp_proxy::RoutingMode::Stateful);
        assert_eq!(launch.host, "127.0.0.1");
        assert_eq!(launch.port.get(), 9120);
        assert_eq!(launch.server_names, vec!["Primary".to_string()]);
        assert!(launch.metrics_store.is_none());
    }

    #[tokio::test]
    async fn send_shutdown_signal_is_non_blocking_when_channel_is_full() {
        let (tx, mut rx) = mpsc::channel::<()>(1);
        tx.try_send(()).expect("fill channel");

        send_shutdown_signal(&tx);

        assert!(rx.recv().await.is_some());
    }
}
