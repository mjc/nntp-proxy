//! WebSocket transport for attached and headless dashboard modes.

use crate::tui::TuiApp;
use crate::tui::dashboard::DashboardState;
use anyhow::Context;
use futures::{Sink, SinkExt, Stream, StreamExt, TryStreamExt, future, stream};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio_tungstenite::{accept_async, connect_async, tungstenite::Message};

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
    let ws_stream = accept_async(stream).await?;
    let (sink, source) = ws_stream.split();

    futures::try_join!(
        send_dashboard_state_stream(sink, state_rx),
        drain_dashboard_source(source)
    )?;
    Ok(())
}

/// Run the headless dashboard websocket publisher.
pub async fn run_dashboard_publisher(
    mut app: TuiApp,
    listen_addr: SocketAddr,
    mut shutdown_rx: tokio::sync::mpsc::Receiver<()>,
) -> anyhow::Result<()> {
    let listener = TcpListener::bind(listen_addr)
        .await
        .with_context(|| format!("Failed to bind dashboard websocket listener at {listen_addr}"))?;
    let (state_tx, _state_rx) = watch::channel(Arc::new(app.snapshot_state()));
    let mut update_interval = tokio::time::interval(Duration::from_millis(250));
    update_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                break;
            }
            _ = update_interval.tick() => {
                app.update();
                let state = Arc::new(app.snapshot_state());
                let _ = state_tx.send(state);
            }
            accept_result = listener.accept() => {
                let (stream, _) = accept_result?;
                let rx = state_tx.subscribe();
                tokio::spawn(async move {
                    let _ = handle_dashboard_client(stream, rx).await;
                });
            }
        }
    }

    Ok(())
}

/// Spawn a background task that keeps the attached TUI synced to a websocket dashboard.
pub fn spawn_dashboard_reader(
    connect_addr: SocketAddr,
    state_tx: watch::Sender<Option<DashboardState>>,
) -> tokio::task::JoinHandle<()> {
    spawn_dashboard_reader_with_retry_delay(connect_addr, state_tx, Duration::from_secs(1))
}

fn spawn_dashboard_reader_with_retry_delay(
    connect_addr: SocketAddr,
    state_tx: watch::Sender<Option<DashboardState>>,
    retry_delay: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let ws_url = format!("ws://{connect_addr}");

        while matches!(
            dashboard_reader_session(&ws_url, state_tx.clone()).await,
            Ok(ReaderSessionOutcome::RetryAfterDelay)
        ) {
            tokio::time::sleep(retry_delay).await;
        }
    })
}

async fn dashboard_reader_session(
    ws_url: &str,
    state_tx: watch::Sender<Option<DashboardState>>,
) -> anyhow::Result<ReaderSessionOutcome> {
    let Ok((ws_stream, _)) = connect_async(ws_url).await else {
        return Ok(ReaderSessionOutcome::RetryAfterDelay);
    };

    let (_, source) = ws_stream.split();
    forward_dashboard_states(source, state_tx)
        .await
        .map(|_| ReaderSessionOutcome::RetryAfterDelay)
}

async fn forward_dashboard_states<S>(
    source: S,
    state_tx: watch::Sender<Option<DashboardState>>,
) -> anyhow::Result<()>
where
    S: Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    dashboard_source_stream(source)
        .map(Ok::<DashboardState, anyhow::Error>)
        .try_fold(state_tx, |state_tx, state| async move {
            state_tx
                .send(Some(state))
                .map_err(|_| anyhow::anyhow!("attached dashboard state receiver dropped"))?;
            Ok::<_, anyhow::Error>(state_tx)
        })
        .await
        .map(|_| ())
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
    }

    #[tokio::test]
    async fn reader_session_reports_retry_for_disconnected_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test port");
        let addr = listener.local_addr().expect("local addr");
        drop(listener);

        let (state_tx, _state_rx) = watch::channel(None::<DashboardState>);
        let outcome = dashboard_reader_session(&format!("ws://{addr}"), state_tx)
            .await
            .expect("reader session");

        assert_eq!(outcome, ReaderSessionOutcome::RetryAfterDelay);
    }

    async fn wait_for_log_lines(
        state_rx: &mut watch::Receiver<Option<DashboardState>>,
        expected: &[&str],
        timeout: Duration,
    ) -> DashboardState {
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(state) = state_rx.borrow().clone()
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
        drop(listener);

        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let publisher = tokio::spawn(run_dashboard_publisher(app, addr, shutdown_rx));

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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test port");
        let addr = listener.local_addr().expect("local addr");

        let (state_tx, mut state_rx) = watch::channel(None::<DashboardState>);
        let reader =
            spawn_dashboard_reader_with_retry_delay(addr, state_tx, Duration::from_millis(25));

        let (stream, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .expect("reader should connect")
            .expect("accept reader connection");
        let mut ws = accept_async(stream).await.expect("accept websocket");

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

        ws.send(Message::Text(
            serde_json::to_string(&first)
                .expect("serialize first")
                .into(),
        ))
        .await
        .expect("send first state");
        ws.send(Message::Text(
            serde_json::to_string(&second)
                .expect("serialize second")
                .into(),
        ))
        .await
        .expect("send second state");

        let observed = wait_for_log_lines(&mut state_rx, &["second"], Duration::from_secs(3)).await;

        assert_eq!(observed.log_lines, vec!["second".to_string()]);
        assert!(observed.show_details);

        let _ = ws.close(None).await;
        reader.abort();
        let _ = reader.await;
    }

    #[tokio::test]
    async fn reader_ignores_malformed_frames() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test port");
        let addr = listener.local_addr().expect("local addr");

        let (state_tx, mut state_rx) = watch::channel(None::<DashboardState>);
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

        let (state_tx, mut state_rx) = watch::channel(None::<DashboardState>);
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

        drop(first_ws);

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

        drop(second_ws);
        reader.abort();
        let _ = reader.await;
    }
}
