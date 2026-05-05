//! WebSocket transport for attached and headless dashboard modes.

use crate::tui::TuiApp;
use crate::tui::dashboard::DashboardState;
use crate::tui::{AttachedDashboard, RemoteDashboardStatus};
use anyhow::Context;
use futures::{Sink, SinkExt, Stream, StreamExt, TryStreamExt, future, stream};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_tungstenite::{accept_async, connect_async, tungstenite::Message};
use tracing::{info, warn};

const DASHBOARD_LOG_LINE_LIMIT: usize = 256;

#[allow(clippy::cast_precision_loss)]
fn dashboard_message(state: &DashboardState) -> anyhow::Result<Message> {
    Ok(Message::Text(serde_json::to_string(state)?.into()))
}

fn dashboard_state_from_message(
    message: Result<Message, tokio_tungstenite::tungstenite::Error>,
) -> Option<DashboardState> {
    match message {
        Ok(Message::Text(text)) => serde_json::from_str::<DashboardState>(text.as_str()).ok(),
        _ => None,
    }
}

fn dashboard_state_stream(
    state_rx: watch::Receiver<Arc<DashboardState>>,
) -> impl Stream<Item = Arc<DashboardState>> {
    let initial_state = state_rx.borrow().clone();
    let updates = stream::unfold(state_rx, |mut state_rx| async move {
        if state_rx.changed().await.is_ok() {
            let state = state_rx.borrow().clone();
            Some((state, state_rx))
        } else {
            None
        }
    });

    stream::once(async move { initial_state }).chain(updates)
}

fn dashboard_source_stream(
    source: impl Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>,
) -> impl Stream<Item = DashboardState> {
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
    sink: S,
    state_rx: watch::Receiver<Arc<DashboardState>>,
) -> anyhow::Result<()>
where
    S: Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    dashboard_state_stream(state_rx)
        .map(|state| dashboard_message(state.as_ref()))
        .try_fold(sink, |mut sink, message| async move {
            sink.send(message).await?;
            Ok::<_, anyhow::Error>(sink)
        })
        .await
        .map(|_| ())
}

async fn drain_dashboard_source<S>(source: S) -> anyhow::Result<()>
where
    S: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    dashboard_source_stream(source)
        .for_each(|_| future::ready(()))
        .await;
    Ok(())
}

async fn handle_dashboard_client(
    stream: TcpStream,
    state_rx: watch::Receiver<Arc<DashboardState>>,
) -> anyhow::Result<()> {
    let peer_addr = stream.peer_addr().ok();
    info!(
        "Dashboard websocket client connected{}",
        dashboard_peer_label(peer_addr)
    );

    let ws_stream = accept_async(stream).await?;
    let (sink, source) = ws_stream.split();

    futures::try_join!(
        send_dashboard_state_stream(sink, state_rx),
        drain_dashboard_source(source)
    )?;

    info!(
        "Dashboard websocket client disconnected{}",
        dashboard_peer_label(peer_addr)
    );
    Ok(())
}

fn spawn_dashboard_client_task(stream: TcpStream, state_rx: watch::Receiver<Arc<DashboardState>>) {
    tokio::spawn(async move {
        if let Err(err) = handle_dashboard_client(stream, state_rx).await {
            warn!("Dashboard websocket client error: {err}");
        }
    });
}

fn publish_dashboard_snapshot(app: &mut TuiApp, state_tx: &watch::Sender<Arc<DashboardState>>) {
    app.update();
    let state = Arc::new(app.snapshot_state_with_log_limit(Some(DASHBOARD_LOG_LINE_LIMIT)));
    let _ = state_tx.send(state);
}

fn handle_dashboard_accept_result(
    accept_result: std::io::Result<(TcpStream, SocketAddr)>,
    state_tx: &watch::Sender<Arc<DashboardState>>,
) {
    match accept_result {
        Ok((stream, _)) => {
            let rx = state_tx.subscribe();
            spawn_dashboard_client_task(stream, rx);
        }
        Err(err) => {
            warn!(error = %err, "Dashboard websocket accept failed; continuing");
        }
    }
}

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

pub async fn run_dashboard_publisher_on_listener(
    mut app: TuiApp,
    listener: TcpListener,
    mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
) -> anyhow::Result<()> {
    let (state_tx, _state_rx) = watch::channel(Arc::new(
        app.snapshot_state_with_log_limit(Some(DASHBOARD_LOG_LINE_LIMIT)),
    ));
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
                handle_dashboard_accept_result(accept_result, &state_tx);
            }
        }
    }

    Ok(())
}

/// Run the headless dashboard websocket publisher.
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
) -> tokio::task::JoinHandle<()> {
    spawn_dashboard_reader_with_retry_delay(connect_addr, state_tx, Duration::from_secs(1))
}

fn spawn_dashboard_reader_with_retry_delay(
    connect_addr: SocketAddr,
    state_tx: watch::Sender<AttachedDashboard>,
    retry_delay: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let ws_url = format!("ws://{connect_addr}");

        info!("Connecting attached dashboard client to {ws_url}");

        loop {
            let session_result = dashboard_reader_session(&ws_url, state_tx.clone()).await;
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
    let (_, source) = ws_stream.split();
    forward_dashboard_states(source, state_tx).await.map(|_| {
        warn!("Attached dashboard websocket stream ended for {ws_url}");
        ReaderSessionOutcome::RetryAfterDelay
    })
}

async fn forward_dashboard_states<S>(
    source: S,
    state_tx: watch::Sender<AttachedDashboard>,
) -> anyhow::Result<()>
where
    S: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    dashboard_source_stream(source)
        .map(Ok::<DashboardState, anyhow::Error>)
        .try_fold(state_tx, |state_tx, state| async move {
            let connect_addr = match &state_tx.borrow().status {
                RemoteDashboardStatus::Connecting { target }
                | RemoteDashboardStatus::Connected { target }
                | RemoteDashboardStatus::Reconnecting { target, .. } => *target,
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
        .map(|_| "connection dropped".to_string())
        .unwrap_or_else(|err| err.to_string())
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
    use crate::metrics::{MetricsCollector, MetricsSnapshot};
    use crate::router::BackendSelector;
    use crate::tui::app::{ThroughputPoint, ViewMode};
    use crate::tui::dashboard::BufferPoolStats;
    use crate::types::Port;
    use crate::types::tui::{Throughput, Timestamp};
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

    #[test]
    fn dashboard_state_from_message_parses_text_frames() {
        let state = DashboardState {
            snapshot: MetricsSnapshot::default(),
            backend_views: Vec::new(),
            client_history: Vec::new(),
            system_stats: crate::tui::SystemStats::default(),
            view_mode: ViewMode::Normal,
            show_details: false,
            log_lines: vec!["hello".to_string()],
            buffer_pool: None,
        };
        let message = Message::Text(serde_json::to_string(&state).unwrap().into());

        let parsed = dashboard_state_from_message(Ok(message)).expect("state should parse");
        assert_eq!(parsed.log_lines, vec!["hello".to_string()]);
    }

    #[test]
    fn dashboard_state_from_message_ignores_non_text_and_invalid_json() {
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
        let (state_tx, _state_rx) = watch::channel(Arc::new(
            app.snapshot_state_with_log_limit(Some(DASHBOARD_LOG_LINE_LIMIT)),
        ));

        handle_dashboard_accept_result(
            Err(std::io::Error::other("synthetic accept error")),
            &state_tx,
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
        let outcome = dashboard_reader_session(&format!("ws://{addr}"), state_tx)
            .await
            .expect("reader session");

        assert_eq!(outcome, ReaderSessionOutcome::RetryAfterDelay);
    }

    async fn wait_for_log_lines(
        state_rx: &mut watch::Receiver<AttachedDashboard>,
        expected: &[&str],
        timeout: Duration,
    ) -> DashboardState {
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

        let Message::Text(text) = message else {
            panic!("expected text frame");
        };
        let state = serde_json::from_str::<DashboardState>(text.as_str())
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
            snapshot: MetricsSnapshot::default(),
            backend_views: Vec::new(),
            client_history: vec![ThroughputPoint::new_client(
                Timestamp::now(),
                Throughput::new(1.0),
                Throughput::new(2.0),
            )],
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
            Ok(Message::Text(
                serde_json::to_string(&first)
                    .expect("serialize first")
                    .into(),
            )),
            Ok(Message::Text(
                serde_json::to_string(&second)
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
        let reader =
            spawn_dashboard_reader_with_retry_delay(addr, state_tx, Duration::from_millis(25));

        let (stream, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .expect("reader should connect")
            .expect("accept reader connection");
        let mut ws = accept_async(stream).await.expect("accept websocket");

        let good = DashboardState {
            snapshot: MetricsSnapshot::default(),
            backend_views: Vec::new(),
            client_history: Vec::new(),
            system_stats: crate::tui::SystemStats::default(),
            view_mode: ViewMode::Normal,
            show_details: false,
            log_lines: vec!["good".to_string()],
            buffer_pool: None,
        };

        ws.send(Message::Text(
            serde_json::to_string(&good).expect("serialize good").into(),
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
        let reader =
            spawn_dashboard_reader_with_retry_delay(addr, state_tx, Duration::from_millis(25));

        let first_conn = tokio::time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .expect("reader should connect initially")
            .expect("accept initial connection");
        let mut first_ws = accept_async(first_conn.0)
            .await
            .expect("accept initial websocket");

        let first = DashboardState {
            snapshot: MetricsSnapshot::default(),
            backend_views: Vec::new(),
            client_history: Vec::new(),
            system_stats: crate::tui::SystemStats::default(),
            view_mode: ViewMode::Normal,
            show_details: false,
            log_lines: vec!["first".to_string()],
            buffer_pool: None,
        };
        first_ws
            .send(Message::Text(
                serde_json::to_string(&first)
                    .expect("serialize first")
                    .into(),
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

        let second = DashboardState {
            snapshot: MetricsSnapshot::default(),
            backend_views: Vec::new(),
            client_history: Vec::new(),
            system_stats: crate::tui::SystemStats::default(),
            view_mode: ViewMode::LogFullscreen,
            show_details: true,
            log_lines: vec!["second".to_string()],
            buffer_pool: Some(BufferPoolStats {
                available: 2,
                in_use: 1,
                total: 3,
            }),
        };
        second_ws
            .send(Message::Text(
                serde_json::to_string(&second)
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
}
