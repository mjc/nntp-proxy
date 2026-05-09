//! WebSocket transport for attached and headless dashboard modes.

use crate::tui::TuiApp;
use crate::tui::dashboard::DashboardState;
use crate::tui::log_capture::LogBuffer;
use crate::tui::{AttachedDashboard, RemoteDashboardStatus};
use anyhow::Context;
use futures::{Sink, SinkExt, Stream, StreamExt, TryStreamExt, future};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_tungstenite::{accept_async, connect_async, tungstenite::Message};
use tracing::{info, warn};

pub(crate) const REMOTE_DASHBOARD_LOG_LINE_LIMIT: usize = 8;
pub(crate) const REMOTE_DASHBOARD_FULLSCREEN_LOG_LINE_LIMIT: usize = 64;
const REMOTE_DASHBOARD_HISTORY_LIMIT: usize = 15;
const REMOTE_DASHBOARD_TOP_USER_LIMIT: usize = 5;

#[allow(clippy::cast_precision_loss)]
fn dashboard_message(state: &DashboardState) -> anyhow::Result<Message> {
    Ok(Message::Binary(
        bincode::serialize(state)
            .context("failed to serialize dashboard state")?
            .into(),
    ))
}

fn dashboard_state_from_message(
    message: Result<Message, tokio_tungstenite::tungstenite::Error>,
) -> Option<Arc<DashboardState>> {
    match message {
        Ok(Message::Binary(bytes)) => bincode::deserialize::<DashboardState>(bytes.as_ref())
            .ok()
            .map(Arc::new),
        Ok(Message::Text(text)) => serde_json::from_str::<DashboardState>(text.as_str())
            .ok()
            .map(Arc::new),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum DashboardClientRequest {
    SetLogTail { lines: usize },
}

fn dashboard_client_request_message(request: DashboardClientRequest) -> anyhow::Result<Message> {
    Ok(Message::Text(serde_json::to_string(&request)?.into()))
}

fn dashboard_client_request_from_message(
    message: Result<Message, tokio_tungstenite::tungstenite::Error>,
) -> Option<DashboardClientRequest> {
    match message {
        Ok(Message::Text(text)) => {
            serde_json::from_str::<DashboardClientRequest>(text.as_str()).ok()
        }
        _ => None,
    }
}

fn sanitize_log_tail_limit(lines: usize) -> usize {
    lines.clamp(
        REMOTE_DASHBOARD_LOG_LINE_LIMIT,
        REMOTE_DASHBOARD_FULLSCREEN_LOG_LINE_LIMIT,
    )
}

fn dashboard_source_stream(
    source: impl Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>,
) -> impl Stream<Item = Arc<DashboardState>> {
    source.filter_map(|message| async move { dashboard_state_from_message(message) })
}

fn dashboard_peer_label(peer_addr: Option<SocketAddr>) -> String {
    peer_addr
        .map(|addr| format!(" from {addr}"))
        .unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReaderSessionOutcome {
    RetryAfterDelay,
}

async fn send_dashboard_state_stream<S>(
    mut sink: S,
    mut state_rx: watch::Receiver<Arc<DashboardState>>,
    mut log_tail_rx: watch::Receiver<usize>,
    log_buffer: LogBuffer,
) -> anyhow::Result<()>
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let mut state = state_rx.borrow().clone();
    let mut log_tail = *log_tail_rx.borrow();

    loop {
        sink.send(dashboard_message_for_client(
            state.as_ref(),
            &log_buffer,
            log_tail,
        )?)
        .await?;

        tokio::select! {
            changed = state_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                state = state_rx.borrow().clone();
            }
            changed = log_tail_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                log_tail = *log_tail_rx.borrow();
            }
        }
    }

    Ok(())
}

async fn receive_dashboard_client_requests<S>(
    source: S,
    log_tail_tx: watch::Sender<usize>,
) -> anyhow::Result<()>
where
    S: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    source
        .filter_map(|message| future::ready(dashboard_client_request_from_message(message)))
        .for_each(|request| {
            let log_tail_tx = log_tail_tx.clone();
            async move {
                let DashboardClientRequest::SetLogTail { lines } = request;
                let _ = log_tail_tx.send(sanitize_log_tail_limit(lines));
            }
        })
        .await;
    Ok(())
}

async fn handle_dashboard_client(
    stream: TcpStream,
    state_rx: watch::Receiver<Arc<DashboardState>>,
    log_buffer: LogBuffer,
) -> anyhow::Result<()> {
    let peer_addr = stream.peer_addr().ok();
    info!(
        "Dashboard websocket client connected{}",
        dashboard_peer_label(peer_addr)
    );

    let ws_stream = accept_async(stream).await?;
    let (sink, source) = ws_stream.split();
    let (log_tail_tx, log_tail_rx) = watch::channel(REMOTE_DASHBOARD_LOG_LINE_LIMIT);

    futures::try_join!(
        send_dashboard_state_stream(sink, state_rx, log_tail_rx, log_buffer),
        receive_dashboard_client_requests(source, log_tail_tx)
    )?;

    info!(
        "Dashboard websocket client disconnected{}",
        dashboard_peer_label(peer_addr)
    );
    Ok(())
}

fn spawn_dashboard_client_task(
    stream: TcpStream,
    state_rx: watch::Receiver<Arc<DashboardState>>,
    log_buffer: LogBuffer,
) {
    tokio::spawn(async move {
        if let Err(err) = handle_dashboard_client(stream, state_rx, log_buffer).await {
            warn!("Dashboard websocket client error: {err}");
        }
    });
}

fn dashboard_message_for_client(
    state: &DashboardState,
    log_buffer: &LogBuffer,
    log_tail: usize,
) -> anyhow::Result<Message> {
    if log_tail == REMOTE_DASHBOARD_LOG_LINE_LIMIT {
        return dashboard_message(state);
    }

    let mut tailored_state = state.clone();
    tailored_state.log_lines = log_buffer.recent_lines(log_tail);
    dashboard_message(&tailored_state)
}

fn publish_dashboard_snapshot(app: &mut TuiApp, state_tx: &watch::Sender<Arc<DashboardState>>) {
    app.update();
    let state = Arc::new(remote_dashboard_state(app));
    let _ = state_tx.send(state);
}

fn handle_dashboard_accept_result(
    accept_result: std::io::Result<(TcpStream, SocketAddr)>,
    state_tx: &watch::Sender<Arc<DashboardState>>,
    log_buffer: &LogBuffer,
) {
    match accept_result {
        Ok((stream, _)) => {
            let rx = state_tx.subscribe();
            spawn_dashboard_client_task(stream, rx, log_buffer.clone());
        }
        Err(err) => {
            warn!(error = %err, "Dashboard websocket accept failed; continuing");
        }
    }
}

/// Bind the dashboard websocket listener.
///
/// # Errors
/// Returns an error when the address is not loopback or the socket cannot be bound.
pub async fn bind_dashboard_listener(listen_addr: SocketAddr) -> anyhow::Result<TcpListener> {
    anyhow::ensure!(
        listen_addr.ip().is_loopback(),
        "Dashboard websocket listener must bind to a loopback address; got {listen_addr}"
    );
    TcpListener::bind(listen_addr).await.with_context(|| {
        format!(
            "Failed to bind dashboard websocket listener at {listen_addr}; ensure that socket is free and distinct from the proxy listener"
        )
    })
}

/// Run the headless dashboard publisher on an already-bound listener.
///
/// # Errors
/// Returns an error if websocket publishing fails after startup.
pub async fn run_dashboard_publisher_on_listener(
    mut app: TuiApp,
    listener: TcpListener,
    mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
) -> anyhow::Result<()> {
    let (state_tx, _state_rx) = watch::channel(Arc::new(remote_dashboard_state(&app)));
    let log_buffer = app.log_buffer().as_ref().clone();
    let mut update_interval = tokio::time::interval(Duration::from_millis(250));
    update_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                break;
            }
            _ = update_interval.tick() => {
                publish_dashboard_snapshot(&mut app, &state_tx);
            }
            accept_result = listener.accept() => {
                handle_dashboard_accept_result(accept_result, &state_tx, &log_buffer);
            }
        }
    }

    Ok(())
}

/// Run the headless dashboard websocket publisher.
///
/// # Errors
/// Returns an error if the listener cannot be bound or the publisher exits with
/// an unrecoverable websocket error.
pub async fn run_dashboard_publisher(
    app: TuiApp,
    listen_addr: SocketAddr,
    shutdown_rx: tokio::sync::mpsc::Receiver<()>,
) -> anyhow::Result<()> {
    let listener = bind_dashboard_listener(listen_addr).await?;
    run_dashboard_publisher_on_listener(app, listener, shutdown_rx).await
}

/// Spawn a background task that keeps the attached TUI synced to a websocket dashboard.
pub(crate) fn spawn_dashboard_reader(
    connect_addr: SocketAddr,
    state_tx: watch::Sender<AttachedDashboard>,
    log_tail_rx: watch::Receiver<usize>,
) -> tokio::task::JoinHandle<()> {
    spawn_dashboard_reader_with_retry_delay(
        connect_addr,
        state_tx,
        log_tail_rx,
        Duration::from_secs(1),
    )
}

fn spawn_dashboard_reader_with_retry_delay(
    connect_addr: SocketAddr,
    state_tx: watch::Sender<AttachedDashboard>,
    log_tail_rx: watch::Receiver<usize>,
    retry_delay: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let ws_url = format!("ws://{connect_addr}");

        info!("Connecting attached dashboard client to {ws_url}");

        loop {
            let session_result =
                dashboard_reader_session(&ws_url, state_tx.clone(), log_tail_rx.clone()).await;
            if !dashboard_reader_should_retry(&session_result) {
                if let Err(err) = session_result {
                    warn!("Attached dashboard websocket reader stopped for {ws_url}: {err}");
                }
                break;
            }

            let last_error = retry_reason(&session_result);
            let last_snapshot = state_tx.borrow().latest_state.clone();
            let _ = state_tx.send(AttachedDashboard::reconnecting(
                connect_addr,
                retry_delay,
                last_snapshot,
                last_error.clone(),
            ));
            warn!(
                "Attached dashboard websocket disconnected or unavailable; retrying in {retry_delay:?}: {last_error}"
            );
            tokio::time::sleep(retry_delay).await;
            info!("Reconnecting attached dashboard client to {ws_url}");
        }
    })
}

fn dashboard_reader_should_retry(outcome: &anyhow::Result<ReaderSessionOutcome>) -> bool {
    matches!(outcome, Ok(ReaderSessionOutcome::RetryAfterDelay))
}

async fn dashboard_reader_session(
    ws_url: &str,
    state_tx: watch::Sender<AttachedDashboard>,
    mut log_tail_rx: watch::Receiver<usize>,
) -> anyhow::Result<ReaderSessionOutcome> {
    let connect_addr = dashboard_target_from_ws_url(ws_url)?;
    let (ws_stream, _) = match connect_async(ws_url).await {
        Ok(connection) => connection,
        Err(err) => {
            warn!("Failed to connect attached dashboard client to {ws_url}: {err}");
            return Ok(ReaderSessionOutcome::RetryAfterDelay);
        }
    };

    info!("Attached dashboard websocket connected to {ws_url}");
    let last_snapshot = state_tx.borrow().latest_state.clone();
    let _ = state_tx.send(AttachedDashboard::connected(connect_addr, last_snapshot));
    let (sink, source) = ws_stream.split();
    let send_requests = send_dashboard_client_requests(sink, &mut log_tail_rx);
    let forward_states = forward_dashboard_states(source, state_tx);
    tokio::pin!(send_requests);
    tokio::pin!(forward_states);

    match future::select(send_requests, forward_states).await {
        future::Either::Left((result, _)) | future::Either::Right((result, _)) => result?,
    }

    warn!("Attached dashboard websocket stream ended for {ws_url}");
    Ok(ReaderSessionOutcome::RetryAfterDelay)
}

async fn send_dashboard_client_requests<S>(
    mut sink: S,
    log_tail_rx: &mut watch::Receiver<usize>,
) -> anyhow::Result<()>
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let mut current_tail = *log_tail_rx.borrow();
    if current_tail != REMOTE_DASHBOARD_LOG_LINE_LIMIT {
        sink.send(dashboard_client_request_message(
            DashboardClientRequest::SetLogTail {
                lines: current_tail,
            },
        )?)
        .await?;
    }

    loop {
        if log_tail_rx.changed().await.is_err() {
            break;
        }
        current_tail = *log_tail_rx.borrow();
        sink.send(dashboard_client_request_message(
            DashboardClientRequest::SetLogTail {
                lines: current_tail,
            },
        )?)
        .await?;
    }

    Ok(())
}

fn trim_remote_dashboard_state(state: &mut DashboardState) {
    state.top_users.truncate(REMOTE_DASHBOARD_TOP_USER_LIMIT);
}

fn remote_dashboard_state(app: &TuiApp) -> DashboardState {
    let mut state = app.snapshot_state_with_limits(
        Some(REMOTE_DASHBOARD_LOG_LINE_LIMIT),
        Some(REMOTE_DASHBOARD_HISTORY_LIMIT),
    );
    trim_remote_dashboard_state(&mut state);
    state
}

async fn forward_dashboard_states<S>(
    source: S,
    state_tx: watch::Sender<AttachedDashboard>,
) -> anyhow::Result<()>
where
    S: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    dashboard_source_stream(source)
        .map(Ok::<Arc<DashboardState>, anyhow::Error>)
        .try_fold(state_tx, |state_tx, state| async move {
            let connect_addr = match &state_tx.borrow().status {
                RemoteDashboardStatus::Connecting { target }
                | RemoteDashboardStatus::Connected { target }
                | RemoteDashboardStatus::Reconnecting { target, .. } => target.to_owned(),
            };
            state_tx
                .send(AttachedDashboard::connected(connect_addr, Some(state)))
                .map_err(|_| anyhow::anyhow!("attached dashboard state receiver dropped"))?;
            Ok::<_, anyhow::Error>(state_tx)
        })
        .await
        .map(|_| ())
}

fn retry_reason(outcome: &anyhow::Result<ReaderSessionOutcome>) -> String {
    outcome
        .as_ref()
        .map_or_else(std::string::ToString::to_string, |_| {
            "connection dropped".to_string()
        })
}

fn dashboard_target_from_ws_url(ws_url: &str) -> anyhow::Result<SocketAddr> {
    ws_url
        .strip_prefix("ws://")
        .context("dashboard websocket URL must start with ws://")?
        .parse()
        .with_context(|| {
            format!("dashboard websocket URL contains an invalid socket address: {ws_url}")
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Server;
    use crate::metrics::MetricsCollector;
    use crate::router::BackendSelector;
    use crate::tui::app::{ThroughputPoint, ViewMode};
    use crate::tui::dashboard::{BufferPoolStats, DashboardMetrics};
    use crate::tui::{LogBuffer, TuiAppBuilder};
    use crate::types::Port;
    use crate::types::tui::{HistorySize, Throughput, Timestamp};
    use futures::{FutureExt, stream};
    use std::sync::Arc;
    use tokio::sync::mpsc;

    fn create_test_servers(count: usize) -> Arc<[Server]> {
        (0..count)
            .map(|i| {
                Server::builder(
                    format!("backend{i}.example.com"),
                    Port::try_new(119).unwrap(),
                )
                .name(format!("Backend {i}"))
                .build()
                .unwrap()
            })
            .collect::<Vec<_>>()
            .into()
    }

    fn build_test_app() -> TuiApp {
        let metrics = MetricsCollector::new(1);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(1);

        TuiApp::new(metrics, router, servers)
    }

    fn empty_dashboard_state() -> DashboardState {
        DashboardState {
            metrics: DashboardMetrics::default(),
            backend_views: Vec::new(),
            top_users: Vec::new(),
            client_history: Vec::new(),
            system_stats: crate::tui::SystemStats::default(),
            view_mode: ViewMode::Normal,
            show_details: false,
            log_lines: Vec::new(),
            buffer_pool: None,
        }
    }

    #[test]
    fn dashboard_state_from_message_parses_text_frames() {
        let state = DashboardState {
            log_lines: vec!["hello".to_string()],
            ..empty_dashboard_state()
        };
        let message = Message::Binary(bincode::serialize(&state).unwrap().into());

        let parsed = dashboard_state_from_message(Ok(message)).expect("state should parse");
        assert_eq!(parsed.log_lines, vec!["hello".to_string()]);
    }

    #[test]
    fn dashboard_state_from_message_accepts_legacy_text_frames() {
        let state = DashboardState {
            log_lines: vec!["hello".to_string()],
            ..empty_dashboard_state()
        };
        let message = Message::Text(serde_json::to_string(&state).unwrap().into());

        let parsed = dashboard_state_from_message(Ok(message)).expect("state should parse");
        assert_eq!(parsed.log_lines, vec!["hello".to_string()]);
    }

    #[test]
    fn dashboard_state_from_message_ignores_non_state_and_invalid_payloads() {
        assert!(dashboard_state_from_message(Ok(Message::Binary(vec![1, 2, 3].into()))).is_none());
        assert!(dashboard_state_from_message(Ok(Message::Ping(vec![1, 2].into()))).is_none());
        assert!(
            dashboard_state_from_message(Err(
                tokio_tungstenite::tungstenite::Error::ConnectionClosed
            ))
            .is_none()
        );
        assert!(dashboard_state_from_message(Ok(Message::Text("{not-json}".into()))).is_none());
    }

    #[test]
    fn dashboard_client_request_round_trips() {
        let message = dashboard_client_request_message(DashboardClientRequest::SetLogTail {
            lines: REMOTE_DASHBOARD_FULLSCREEN_LOG_LINE_LIMIT,
        })
        .expect("serialize request");

        let parsed =
            dashboard_client_request_from_message(Ok(message)).expect("request should parse");

        assert_eq!(
            parsed,
            DashboardClientRequest::SetLogTail {
                lines: REMOTE_DASHBOARD_FULLSCREEN_LOG_LINE_LIMIT
            }
        );
    }

    #[test]
    fn sanitize_log_tail_limit_clamps_to_supported_range() {
        assert_eq!(sanitize_log_tail_limit(1), REMOTE_DASHBOARD_LOG_LINE_LIMIT);
        assert_eq!(
            sanitize_log_tail_limit(REMOTE_DASHBOARD_FULLSCREEN_LOG_LINE_LIMIT + 10),
            REMOTE_DASHBOARD_FULLSCREEN_LOG_LINE_LIMIT
        );
    }

    #[test]
    fn dashboard_peer_label_formats_optional_peer_addresses() {
        assert_eq!(dashboard_peer_label(None), "");
        assert_eq!(
            dashboard_peer_label(Some("127.0.0.1:1234".parse().unwrap())),
            " from 127.0.0.1:1234"
        );
    }

    #[test]
    fn dashboard_reader_should_retry_only_for_retry_outcomes() {
        assert!(dashboard_reader_should_retry(&Ok(
            ReaderSessionOutcome::RetryAfterDelay
        )));
        assert!(!dashboard_reader_should_retry(&Err(anyhow::anyhow!(
            "boom"
        ))));
    }

    #[test]
    fn retry_reason_prefers_source_error_text() {
        assert_eq!(
            retry_reason(&Ok(ReaderSessionOutcome::RetryAfterDelay)),
            "connection dropped"
        );
        assert_eq!(
            retry_reason(&Err(anyhow::anyhow!("connection refused"))),
            "connection refused"
        );
    }

    #[test]
    fn dashboard_target_from_ws_url_parses_socket_addresses() {
        let addr = dashboard_target_from_ws_url("ws://127.0.0.1:8120").expect("target addr");
        assert_eq!(addr, "127.0.0.1:8120".parse().unwrap());
        assert!(dashboard_target_from_ws_url("http://127.0.0.1:8120").is_err());
    }

    #[tokio::test]
    async fn publish_dashboard_snapshot_updates_watch_channel() {
        let mut app = build_test_app();
        let (state_tx, mut state_rx) = watch::channel(Arc::new(app.snapshot_state()));

        publish_dashboard_snapshot(&mut app, &state_tx);

        state_rx.changed().await.expect("snapshot should update");
        let snapshot = state_rx.borrow().clone();
        assert_eq!(snapshot.backend_views.len(), 1);
        assert_eq!(snapshot.view_mode, ViewMode::Normal);
    }

    #[tokio::test]
    async fn dashboard_publisher_bind_error_mentions_dashboard_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test port");
        let addr = listener.local_addr().expect("local addr");
        let (_shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

        let err = run_dashboard_publisher(build_test_app(), addr, shutdown_rx)
            .await
            .expect_err("bind should fail");

        let err = format!("{err:#}");
        assert!(err.contains("Failed to bind dashboard websocket listener"));
        assert!(err.contains("ensure that socket is free"));
    }

    #[test]
    fn dashboard_accept_errors_are_nonfatal() {
        let app = build_test_app();
        let (state_tx, _state_rx) = watch::channel(Arc::new(remote_dashboard_state(&app)));

        handle_dashboard_accept_result(
            Err(std::io::Error::other("synthetic accept error")),
            &state_tx,
            app.log_buffer().as_ref(),
        );
    }

    #[tokio::test]
    async fn publish_dashboard_snapshot_limits_remote_logs_and_history() {
        let metrics = MetricsCollector::new(1);
        let router = Arc::new(BackendSelector::new());
        let servers = create_test_servers(1);
        let log_buffer = LogBuffer::new();
        for i in 0..100 {
            log_buffer.push(format!("line {i}"));
        }

        let mut app = TuiAppBuilder::new(metrics, router, servers)
            .with_log_buffer(log_buffer)
            .with_history_size(HistorySize::new(32))
            .build();

        for _ in 0..20 {
            app.update();
        }

        let (state_tx, mut state_rx) = watch::channel(Arc::new(app.snapshot_state()));
        publish_dashboard_snapshot(&mut app, &state_tx);
        state_rx.changed().await.expect("snapshot should update");

        let snapshot = state_rx.borrow().clone();
        assert_eq!(snapshot.log_lines.len(), REMOTE_DASHBOARD_LOG_LINE_LIMIT);
        assert_eq!(
            snapshot.log_lines.first().map(String::as_str),
            Some("line 92")
        );
        assert_eq!(
            snapshot.client_history.len(),
            REMOTE_DASHBOARD_HISTORY_LIMIT,
        );
        assert_eq!(
            snapshot.backend_views[0].history.len(),
            REMOTE_DASHBOARD_HISTORY_LIMIT,
        );
    }

    #[test]
    fn trim_remote_dashboard_state_limits_top_users() {
        let mut state = DashboardState {
            metrics: crate::tui::dashboard::DashboardMetrics::default(),
            backend_views: Vec::new(),
            top_users: (0..10)
                .map(|idx| crate::tui::dashboard::DashboardUserStats {
                    username: format!("user-{idx}"),
                    active_connections: 0,
                    total_connections: crate::types::TotalConnections::new(0),
                    bytes_sent: crate::types::BytesSent::ZERO,
                    bytes_received: crate::types::BytesReceived::ZERO,
                    bytes_sent_per_sec: crate::types::BytesPerSecondRate::new(0),
                    bytes_received_per_sec: crate::types::BytesPerSecondRate::new(0),
                    total_commands: crate::metrics::CommandCount::new(0),
                    errors: crate::metrics::ErrorCount::new(0),
                })
                .collect(),
            client_history: Vec::new(),
            system_stats: crate::tui::SystemStats::default(),
            view_mode: ViewMode::Normal,
            show_details: false,
            log_lines: Vec::new(),
            buffer_pool: None,
        };

        trim_remote_dashboard_state(&mut state);

        assert_eq!(state.top_users.len(), REMOTE_DASHBOARD_TOP_USER_LIMIT);
        assert_eq!(
            state.top_users.first().map(|user| user.username.as_str()),
            Some("user-0")
        );
    }

    #[tokio::test]
    async fn dashboard_publisher_rejects_non_loopback_listener() {
        let err = bind_dashboard_listener("0.0.0.0:8120".parse().unwrap())
            .await
            .expect_err("non-loopback bind should fail");

        assert!(format!("{err:#}").contains("must bind to a loopback address"));
    }

    #[tokio::test]
    async fn reader_session_reports_retry_for_disconnected_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test port");
        let addr = listener.local_addr().expect("local addr");
        drop(listener);

        let (state_tx, _state_rx) = watch::channel(AttachedDashboard::connecting(addr));
        let (_log_tail_tx, log_tail_rx) = watch::channel(REMOTE_DASHBOARD_LOG_LINE_LIMIT);
        let outcome = dashboard_reader_session(&format!("ws://{addr}"), state_tx, log_tail_rx)
            .await
            .expect("reader session");

        assert_eq!(outcome, ReaderSessionOutcome::RetryAfterDelay);
    }

    async fn wait_for_log_lines(
        state_rx: &mut watch::Receiver<AttachedDashboard>,
        expected: &[&str],
        timeout: Duration,
    ) -> Arc<DashboardState> {
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(state) = state_rx.borrow().latest_state.clone()
                    && state
                        .log_lines
                        .iter()
                        .map(String::as_str)
                        .eq(expected.iter().copied())
                {
                    return state;
                }
                state_rx.changed().await.expect("reader still alive");
            }
        })
        .await
        .expect("observe expected dashboard state")
    }

    #[tokio::test]
    async fn publisher_serves_initial_snapshot() {
        let app = build_test_app();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test port");
        let addr = listener.local_addr().expect("local addr");

        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let publisher = tokio::spawn(run_dashboard_publisher_on_listener(
            app,
            listener,
            shutdown_rx,
        ));

        let (ws_stream, _) = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match tokio_tungstenite::connect_async(format!("ws://{addr}")).await {
                    Ok(result) => break result,
                    Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                }
            }
        })
        .await
        .expect("publisher should accept connections");
        let (mut sink, mut source) = ws_stream.split();

        let message = tokio::time::timeout(Duration::from_secs(3), source.next())
            .await
            .expect("receive snapshot")
            .expect("ws message")
            .expect("valid ws frame");

        let Message::Binary(bytes) = message else {
            panic!("expected binary frame");
        };
        let state = bincode::deserialize::<DashboardState>(bytes.as_ref())
            .expect("deserialize dashboard state");

        assert_eq!(state.backend_views.len(), 1);
        assert_eq!(state.view_mode, ViewMode::Normal);
        assert!(state.buffer_pool.is_none());

        sink.close().await.expect("close websocket");
        let _ = shutdown_tx.send(()).await;
        tokio::time::timeout(Duration::from_secs(3), publisher)
            .await
            .expect("publisher task should exit")
            .expect("publisher join")
            .expect("publisher should exit cleanly");
    }

    #[tokio::test]
    async fn reader_applies_latest_state() {
        let connect_addr: SocketAddr = "127.0.0.1:8120".parse().unwrap();
        let (state_tx, mut state_rx) = watch::channel(AttachedDashboard::connecting(connect_addr));

        let first = DashboardState {
            backend_views: Vec::new(),
            top_users: Vec::new(),
            client_history: vec![ThroughputPoint::new_client(
                Timestamp::now(),
                Throughput::new(1.0),
                Throughput::new(2.0),
            )],
            metrics: DashboardMetrics::default(),
            system_stats: crate::tui::SystemStats::default(),
            view_mode: ViewMode::Normal,
            show_details: false,
            log_lines: vec!["first".to_string()],
            buffer_pool: Some(BufferPoolStats {
                available: 1,
                in_use: 0,
                total: 1,
            }),
        };
        let mut second = first.clone();
        second.log_lines = vec!["second".to_string()];
        second.show_details = true;

        let source = stream::iter(vec![
            Ok(Message::Binary(
                bincode::serialize(&first).expect("serialize first").into(),
            )),
            Ok(Message::Binary(
                bincode::serialize(&second)
                    .expect("serialize second")
                    .into(),
            )),
        ]);

        forward_dashboard_states(source, state_tx)
            .await
            .expect("forward dashboard states");

        let observed = wait_for_log_lines(&mut state_rx, &["second"], Duration::from_secs(3)).await;

        assert_eq!(observed.log_lines, vec!["second".to_string()]);
        assert!(observed.show_details);
        assert_eq!(
            state_rx.borrow().status,
            RemoteDashboardStatus::Connected {
                target: connect_addr
            }
        );
    }

    #[tokio::test]
    async fn reader_ignores_malformed_frames() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test port");
        let addr = listener.local_addr().expect("local addr");

        let (state_tx, mut state_rx) = watch::channel(AttachedDashboard::connecting(addr));
        let (_log_tail_tx, log_tail_rx) = watch::channel(REMOTE_DASHBOARD_LOG_LINE_LIMIT);
        let reader = spawn_dashboard_reader_with_retry_delay(
            addr,
            state_tx,
            log_tail_rx,
            Duration::from_millis(25),
        );

        let (stream, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .expect("reader should connect")
            .expect("accept reader connection");
        let mut ws = accept_async(stream).await.expect("accept websocket");

        let good = DashboardState {
            log_lines: vec!["good".to_string()],
            ..empty_dashboard_state()
        };

        ws.send(Message::Binary(
            bincode::serialize(&good).expect("serialize good").into(),
        ))
        .await
        .expect("send good state");

        let observed = wait_for_log_lines(&mut state_rx, &["good"], Duration::from_secs(3)).await;
        assert_eq!(observed.log_lines, vec!["good".to_string()]);

        ws.send(Message::Binary(vec![1, 2, 3].into()))
            .await
            .expect("send binary frame");
        ws.send(Message::Text("{not-json}".into()))
            .await
            .expect("send invalid json");

        tokio::time::sleep(Duration::from_millis(100)).await;
        let latest = state_rx
            .borrow()
            .latest_state
            .clone()
            .expect("state should remain present");
        assert_eq!(latest.log_lines, vec!["good".to_string()]);

        drop(ws);
        reader.abort();
        let _ = reader.await;
    }

    #[tokio::test]
    async fn reader_recovers_after_disconnect() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test port");
        let addr = listener.local_addr().expect("local addr");

        let (state_tx, mut state_rx) = watch::channel(AttachedDashboard::connecting(addr));
        let (log_tail_tx, log_tail_rx) = watch::channel(REMOTE_DASHBOARD_LOG_LINE_LIMIT);
        let reader = spawn_dashboard_reader_with_retry_delay(
            addr,
            state_tx,
            log_tail_rx,
            Duration::from_millis(25),
        );

        let first_conn = tokio::time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .expect("reader should connect initially")
            .expect("accept initial connection");
        let mut first_ws = accept_async(first_conn.0)
            .await
            .expect("accept initial websocket");

        let first = DashboardState {
            log_lines: vec!["first".to_string()],
            ..empty_dashboard_state()
        };
        first_ws
            .send(Message::Binary(
                bincode::serialize(&first).expect("serialize first").into(),
            ))
            .await
            .expect("send first state");

        let first_observed =
            wait_for_log_lines(&mut state_rx, &["first"], Duration::from_secs(3)).await;
        assert_eq!(first_observed.log_lines, vec!["first".to_string()]);
        assert_eq!(
            state_rx.borrow().status,
            RemoteDashboardStatus::Connected { target: addr }
        );

        drop(first_ws);

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if matches!(
                    &state_rx.borrow().status,
                    RemoteDashboardStatus::Reconnecting { target, .. } if *target == addr
                ) {
                    break;
                }
                state_rx.changed().await.expect("reader still alive");
            }
        })
        .await
        .expect("reader should enter reconnecting state");

        let second_conn = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .expect("reader should reconnect")
            .expect("accept reconnect connection");
        let mut second_ws = accept_async(second_conn.0)
            .await
            .expect("accept reconnect websocket");
        assert!(
            second_ws.next().now_or_never().is_none(),
            "default log tail should not send an initial control frame"
        );
        let _ = log_tail_tx.send(REMOTE_DASHBOARD_FULLSCREEN_LOG_LINE_LIMIT);
        let request = tokio::time::timeout(Duration::from_secs(3), second_ws.next())
            .await
            .expect("receive log-tail request")
            .expect("request frame")
            .expect("valid ws frame");
        assert_eq!(
            dashboard_client_request_from_message(Ok(request)),
            Some(DashboardClientRequest::SetLogTail {
                lines: REMOTE_DASHBOARD_FULLSCREEN_LOG_LINE_LIMIT
            })
        );

        let second = DashboardState {
            view_mode: ViewMode::LogFullscreen,
            show_details: true,
            log_lines: vec!["second".to_string()],
            buffer_pool: Some(BufferPoolStats {
                available: 2,
                in_use: 1,
                total: 3,
            }),
            ..empty_dashboard_state()
        };
        second_ws
            .send(Message::Binary(
                bincode::serialize(&second)
                    .expect("serialize second")
                    .into(),
            ))
            .await
            .expect("send second state");

        let second_observed =
            wait_for_log_lines(&mut state_rx, &["second"], Duration::from_secs(5)).await;

        assert_eq!(second_observed.log_lines, vec!["second".to_string()]);
        assert_eq!(second_observed.view_mode, ViewMode::LogFullscreen);
        assert!(second_observed.show_details);
        assert_eq!(
            second_observed.buffer_pool.as_ref().map(|pool| pool.total),
            Some(3)
        );
        assert_eq!(
            state_rx.borrow().status,
            RemoteDashboardStatus::Connected { target: addr }
        );

        drop(second_ws);
        reader.abort();
        let _ = reader.await;
    }

    #[test]
    fn dashboard_message_for_client_expands_log_tail_on_request() {
        let log_buffer = LogBuffer::new();
        for i in 0..100 {
            log_buffer.push(format!("line {i}"));
        }

        let state = DashboardState {
            log_lines: log_buffer.recent_lines(REMOTE_DASHBOARD_LOG_LINE_LIMIT),
            ..empty_dashboard_state()
        };

        let message = dashboard_message_for_client(
            &state,
            &log_buffer,
            REMOTE_DASHBOARD_FULLSCREEN_LOG_LINE_LIMIT,
        )
        .expect("serialize tailored state");
        let Message::Binary(bytes) = message else {
            panic!("expected binary frame");
        };
        let tailored =
            bincode::deserialize::<DashboardState>(bytes.as_ref()).expect("deserialize state");

        assert_eq!(
            tailored.log_lines.len(),
            REMOTE_DASHBOARD_FULLSCREEN_LOG_LINE_LIMIT
        );
        assert_eq!(
            tailored.log_lines.first().map(String::as_str),
            Some("line 36")
        );
        assert_eq!(
            tailored.log_lines.last().map(String::as_str),
            Some("line 99")
        );
    }
}
