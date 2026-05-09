//! Terminal User Interface (TUI) for the NNTP proxy
//!
//! Provides real-time visualization of proxy metrics when running in an interactive terminal.

mod app;
mod constants;
mod dashboard;
mod helpers;
pub mod log_capture;
mod rate_estimator;
mod remote;
mod system_stats;
mod types;
mod ui;
#[cfg(test)]
mod ui_tests;

use crate::tui::constants::styles;
pub use app::{TuiApp, TuiAppBuilder, ViewMode};
pub use dashboard::{BackendView, BufferPoolStats, DashboardState};
pub use log_capture::{LogBuffer, LogMakeWriter};
pub(crate) use remote::spawn_dashboard_reader;
pub use remote::{
    bind_dashboard_listener, run_dashboard_publisher, run_dashboard_publisher_on_listener,
};
pub use system_stats::{SystemMonitor, SystemStats};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::future::BoxFuture;
use ratatui::{Terminal, backend::CrosstermBackend};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Stylize},
    text::Line,
    widgets::Paragraph,
};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;
const LOCAL_TUI_LOG_LINE_LIMIT: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteDashboardStatus {
    Connecting {
        target: SocketAddr,
    },
    Connected {
        target: SocketAddr,
    },
    Reconnecting {
        target: SocketAddr,
        retry_delay: Duration,
        last_error: String,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct AttachedDashboard {
    pub latest_state: Option<Arc<DashboardState>>,
    pub status: RemoteDashboardStatus,
}

impl AttachedDashboard {
    fn connecting(target: SocketAddr) -> Self {
        Self {
            latest_state: None,
            status: RemoteDashboardStatus::Connecting { target },
        }
    }

    fn connected(target: SocketAddr, latest_state: Option<Arc<DashboardState>>) -> Self {
        Self {
            latest_state,
            status: RemoteDashboardStatus::Connected { target },
        }
    }

    fn reconnecting(
        target: SocketAddr,
        retry_delay: Duration,
        latest_state: Option<Arc<DashboardState>>,
        last_error: impl Into<String>,
    ) -> Self {
        Self {
            latest_state,
            status: RemoteDashboardStatus::Reconnecting {
                target,
                retry_delay,
                last_error: last_error.into(),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TuiInputAction {
    None,
    Quit,
    ToggleLogFullscreen,
    ToggleDetails,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AttachedViewOverrides {
    view_mode: Option<ViewMode>,
    show_details: Option<bool>,
}

impl AttachedViewOverrides {
    fn toggle_log_fullscreen(&mut self, current_state: Option<&DashboardState>) {
        let current_mode = self
            .view_mode
            .or_else(|| current_state.map(|state| state.view_mode))
            .unwrap_or(ViewMode::Normal);
        self.view_mode = Some(match current_mode {
            ViewMode::Normal => ViewMode::LogFullscreen,
            ViewMode::LogFullscreen => ViewMode::Normal,
        });
    }

    fn toggle_details(&mut self, current_state: Option<&DashboardState>) {
        let show_details = self
            .show_details
            .or_else(|| current_state.map(|state| state.show_details))
            .unwrap_or(false);
        self.show_details = Some(!show_details);
    }
}

/// Setup the terminal for TUI rendering
fn setup_terminal() -> Result<TuiTerminal> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

/// Restore the terminal to its original state
fn restore_terminal(terminal: &mut TuiTerminal) -> Result<()> {
    // Clear the terminal first to prevent escape sequences leaking to shell
    terminal.clear()?;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
}

async fn with_terminal_session<T>(
    run: impl for<'a> FnOnce(&'a mut TuiTerminal) -> BoxFuture<'a, Result<T>>,
) -> Result<T> {
    let mut terminal = setup_terminal()?;

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    let result = run(&mut terminal).await;

    restore_terminal(&mut terminal)?;
    result
}

/// Run the TUI event loop
///
/// This function takes ownership of the terminal and runs until the user presses 'q' or Ctrl+C,
/// or until a shutdown signal is received externally.
/// When the TUI exits, it sends a shutdown signal via the provided channel.
///
/// # Arguments
/// * `app` - The TUI application state
/// * `shutdown_tx` - Sender to signal shutdown when TUI exits
/// * `shutdown_rx` - Receiver to listen for external shutdown signals
///
/// # Returns
/// Ok(()) when the TUI exits normally, or an error if terminal operations fail
///
/// # Errors
/// Returns any terminal setup, drawing, input, shutdown-signal, or restore error
/// encountered while running the interactive UI.
pub async fn run_tui(
    mut app: TuiApp,
    shutdown_tx: mpsc::Sender<()>,
    mut shutdown_rx: mpsc::Receiver<()>,
) -> Result<()> {
    let result = with_terminal_session(move |terminal| {
        Box::pin(async move { run_app(terminal, &mut app, &mut shutdown_rx).await })
    })
    .await;

    // Signal shutdown when TUI exits
    let _ = shutdown_tx.send(()).await;

    result
}

/// Run the TUI as a remote dashboard client connected to a headless publisher.
///
/// # Errors
/// Returns an error when the target is non-loopback or when terminal setup,
/// drawing, input, or restore operations fail.
pub async fn run_attached_tui(connect_addr: SocketAddr) -> Result<()> {
    anyhow::ensure!(
        connect_addr.ip().is_loopback(),
        "Attached dashboard client must connect to a loopback address: {connect_addr}"
    );
    let (state_tx, mut state_rx) =
        tokio::sync::watch::channel(AttachedDashboard::connecting(connect_addr));
    let _reader = spawn_dashboard_reader(connect_addr, state_tx);
    with_terminal_session(move |terminal| {
        Box::pin(async move { run_attached_app(terminal, &mut state_rx).await })
    })
    .await
}

/// Main TUI event loop
async fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut TuiApp,
    shutdown_rx: &mut mpsc::Receiver<()>,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    // Create update interval (4 times per second for responsive UI)
    let mut update_interval = tokio::time::interval(Duration::from_millis(250));
    let mut input_rx = spawn_tui_input_reader(true);

    // Initial render
    terminal
        .draw(|f| ui::render_ui(f, &renderable_dashboard_state(app), None, None, None, None))?;

    loop {
        tokio::select! {
            // External shutdown signal
            _ = shutdown_rx.recv() => {
                break;
            }
            Some(action) = input_rx.recv() => {
                if handle_local_tui_action(action, app) {
                    break;
                }
                terminal.draw(|f| {
                    ui::render_ui(
                        f,
                        &renderable_dashboard_state(app),
                        None,
                        None,
                        None,
                        None,
                    )
                })?;
            }
            // Update timer - check for events only on ticks to reduce overhead
            _ = update_interval.tick() => {
                app.update();
                terminal.draw(|f| {
                    ui::render_ui(
                        f,
                        &renderable_dashboard_state(app),
                        None,
                        None,
                        None,
                        None,
                    )
                })?;
            }
        }
    }

    Ok(())
}

fn renderable_dashboard_state(app: &TuiApp) -> DashboardState {
    app.snapshot_state_with_log_limit(Some(LOCAL_TUI_LOG_LINE_LIMIT))
}

async fn run_attached_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state_rx: &mut tokio::sync::watch::Receiver<AttachedDashboard>,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let mut update_interval = tokio::time::interval(Duration::from_millis(250));
    let mut input_rx = spawn_tui_input_reader(true);
    let mut view_overrides = AttachedViewOverrides::default();

    terminal.draw(|f| {
        let attached = state_rx.borrow();
        draw_attached_dashboard(f, &attached, &view_overrides);
    })?;

    loop {
        tokio::select! {
            Some(action) = input_rx.recv() => {
                if handle_attached_tui_action(
                    action,
                    &mut view_overrides,
                    state_rx.borrow().latest_state.as_deref(),
                ) {
                    break;
                }

                terminal.draw(|f| {
                    let attached = state_rx.borrow();
                    draw_attached_dashboard(f, &attached, &view_overrides);
                })?;
            }
            _ = update_interval.tick() => {
                terminal.draw(|f| {
                    let attached = state_rx.borrow();
                    draw_attached_dashboard(f, &attached, &view_overrides);
                })?;
            }
        }
    }

    Ok(())
}

fn draw_attached_dashboard(
    f: &mut ratatui::Frame,
    attached: &AttachedDashboard,
    view_overrides: &AttachedViewOverrides,
) {
    if let Some(rendered) = renderable_attached_state(attached, view_overrides) {
        ui::render_ui(
            f,
            rendered.state,
            None,
            Some(&attached.status),
            Some(rendered.view_mode),
            Some(rendered.show_details),
        );
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(f.area());

    let title = Paragraph::new(Line::from(vec![
        "NNTP Proxy "
            .fg(crate::tui::constants::styles::BORDER_ACTIVE)
            .bold(),
        "- Attached Dashboard".fg(Color::White),
    ]))
    .alignment(Alignment::Center);

    let body = Paragraph::new(build_attached_placeholder_lines(&attached.status))
        .alignment(Alignment::Center);

    let footer = Paragraph::new(Line::from(vec![
        "Press ".fg(crate::tui::constants::styles::LABEL),
        "q".fg(crate::tui::constants::styles::VALUE_INFO).bold(),
        " or ".fg(crate::tui::constants::styles::LABEL),
        "Esc".fg(crate::tui::constants::styles::VALUE_INFO).bold(),
        " to exit".fg(crate::tui::constants::styles::LABEL),
    ]))
    .alignment(Alignment::Center);

    f.render_widget(title, chunks[0]);
    f.render_widget(body, chunks[1]);
    f.render_widget(footer, chunks[2]);
}

#[derive(Debug, Clone, Copy)]
struct RenderableAttachedState<'a> {
    state: &'a DashboardState,
    view_mode: ViewMode,
    show_details: bool,
}

fn renderable_attached_state<'a>(
    attached: &'a AttachedDashboard,
    view_overrides: &AttachedViewOverrides,
) -> Option<RenderableAttachedState<'a>> {
    let state = attached.latest_state.as_deref()?;
    Some(RenderableAttachedState {
        state,
        view_mode: view_overrides.view_mode.unwrap_or(state.view_mode),
        show_details: view_overrides.show_details.unwrap_or(state.show_details),
    })
}

fn build_attached_placeholder_lines(status: &RemoteDashboardStatus) -> Vec<Line<'static>> {
    match status {
        RemoteDashboardStatus::Connecting { target } => vec![
            Line::from(vec![
                "Connecting to ".fg(styles::LABEL),
                target.to_string().fg(styles::VALUE_INFO).bold(),
                "...".fg(styles::LABEL),
            ]),
            Line::from("Waiting for the first dashboard snapshot."),
        ],
        RemoteDashboardStatus::Connected { target } => vec![
            Line::from(vec![
                "Connected to ".fg(styles::LABEL),
                target.to_string().fg(styles::VALUE_PRIMARY).bold(),
            ]),
            Line::from("Waiting for the first dashboard snapshot."),
        ],
        RemoteDashboardStatus::Reconnecting {
            target,
            retry_delay,
            last_error,
        } => vec![
            Line::from(vec![
                "Reconnecting to ".fg(styles::LABEL),
                target.to_string().fg(Color::Yellow).bold(),
                format!(" in {:.1}s", retry_delay.as_secs_f32()).fg(styles::LABEL),
            ]),
            Line::from(vec![
                "Last error: ".fg(styles::LABEL),
                last_error.clone().fg(Color::Yellow),
            ]),
        ],
    }
}

fn key_event_action(
    key: crossterm::event::KeyEvent,
    allow_dashboard_actions: bool,
) -> TuiInputAction {
    if key.kind == KeyEventKind::Release {
        return TuiInputAction::None;
    }

    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => TuiInputAction::Quit,
        KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
            TuiInputAction::Quit
        }
        KeyCode::Char('l') if allow_dashboard_actions => TuiInputAction::ToggleLogFullscreen,
        KeyCode::Char('d') if allow_dashboard_actions => TuiInputAction::ToggleDetails,
        _ => TuiInputAction::None,
    }
}

fn spawn_tui_input_reader(
    allow_dashboard_actions: bool,
) -> mpsc::UnboundedReceiver<TuiInputAction> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::task::spawn_blocking(move || {
        loop {
            match event::poll(Duration::from_millis(50)) {
                Ok(true) => {
                    let Ok(event) = event::read() else {
                        break;
                    };
                    let Event::Key(key) = event else {
                        continue;
                    };
                    let action = key_event_action(key, allow_dashboard_actions);
                    if action != TuiInputAction::None && tx.send(action).is_err() {
                        break;
                    }
                }
                Ok(false) => {
                    if tx.is_closed() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    rx
}

fn handle_local_tui_action(action: TuiInputAction, app: &mut TuiApp) -> bool {
    match action {
        TuiInputAction::Quit => true,
        TuiInputAction::ToggleLogFullscreen => {
            app.toggle_log_fullscreen();
            false
        }
        TuiInputAction::ToggleDetails => {
            app.toggle_details();
            false
        }
        TuiInputAction::None => false,
    }
}

fn handle_attached_tui_action(
    action: TuiInputAction,
    view_overrides: &mut AttachedViewOverrides,
    current_state: Option<&DashboardState>,
) -> bool {
    match action {
        TuiInputAction::Quit => true,
        TuiInputAction::ToggleLogFullscreen => {
            view_overrides.toggle_log_fullscreen(current_state);
            false
        }
        TuiInputAction::ToggleDetails => {
            view_overrides.toggle_details(current_state);
            false
        }
        TuiInputAction::None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Server;
    use crate::metrics::MetricsCollector;
    use crate::router::BackendSelector;
    use crate::types::Port;
    use std::sync::Arc;

    #[test]
    fn key_event_action_quits_on_q_and_escape() {
        let q = crossterm::event::KeyEvent::new(KeyCode::Char('q'), event::KeyModifiers::NONE);
        let esc = crossterm::event::KeyEvent::new(KeyCode::Esc, event::KeyModifiers::NONE);

        assert_eq!(key_event_action(q, true), TuiInputAction::Quit);
        assert_eq!(key_event_action(esc, true), TuiInputAction::Quit);
    }

    #[test]
    fn key_event_action_supports_dashboard_toggles_only_when_enabled() {
        let log_toggle =
            crossterm::event::KeyEvent::new(KeyCode::Char('l'), event::KeyModifiers::NONE);
        let details_toggle =
            crossterm::event::KeyEvent::new(KeyCode::Char('d'), event::KeyModifiers::NONE);

        assert_eq!(
            key_event_action(log_toggle, true),
            TuiInputAction::ToggleLogFullscreen
        );
        assert_eq!(
            key_event_action(details_toggle, true),
            TuiInputAction::ToggleDetails
        );
        assert_eq!(key_event_action(log_toggle, false), TuiInputAction::None);
        assert_eq!(
            key_event_action(details_toggle, false),
            TuiInputAction::None
        );
    }

    #[test]
    fn key_event_action_ignores_key_releases_and_other_input() {
        let released = crossterm::event::KeyEvent {
            code: KeyCode::Char('q'),
            modifiers: event::KeyModifiers::NONE,
            kind: KeyEventKind::Release,
            state: event::KeyEventState::NONE,
        };
        let other = crossterm::event::KeyEvent::new(KeyCode::Char('x'), event::KeyModifiers::NONE);

        assert_eq!(key_event_action(released, true), TuiInputAction::None);
        assert_eq!(key_event_action(other, true), TuiInputAction::None);
    }

    #[test]
    fn key_event_action_accepts_repeat_events_for_attached_tui() {
        let repeated = crossterm::event::KeyEvent {
            code: KeyCode::Char('d'),
            modifiers: event::KeyModifiers::NONE,
            kind: KeyEventKind::Repeat,
            state: event::KeyEventState::NONE,
        };

        assert_eq!(
            key_event_action(repeated, true),
            TuiInputAction::ToggleDetails
        );
    }

    #[test]
    fn handle_attached_tui_action_applies_overrides() {
        let state = DashboardState {
            metrics: crate::tui::dashboard::DashboardMetrics::default(),
            backend_views: Vec::new(),
            top_users: Vec::new(),
            client_history: Vec::new(),
            system_stats: SystemStats::default(),
            view_mode: ViewMode::Normal,
            show_details: false,
            log_lines: vec!["line".to_string()],
            buffer_pool: None,
        };
        let mut overrides = AttachedViewOverrides::default();

        assert!(!handle_attached_tui_action(
            TuiInputAction::ToggleDetails,
            &mut overrides,
            Some(&state),
        ));
        assert!(!handle_attached_tui_action(
            TuiInputAction::ToggleLogFullscreen,
            &mut overrides,
            Some(&state),
        ));

        let attached =
            AttachedDashboard::connected("127.0.0.1:8120".parse().unwrap(), Some(Arc::new(state)));
        let rendered = renderable_attached_state(&attached, &overrides).expect("dashboard state");
        assert!(rendered.show_details);
        assert_eq!(rendered.view_mode, ViewMode::LogFullscreen);
        assert_eq!(rendered.state.log_lines, vec!["line".to_string()]);
    }

    #[test]
    fn attached_placeholder_lines_reflect_connection_state() {
        let connecting = build_attached_placeholder_lines(&RemoteDashboardStatus::Connecting {
            target: "127.0.0.1:8120".parse().unwrap(),
        })
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
        let reconnecting = build_attached_placeholder_lines(&RemoteDashboardStatus::Reconnecting {
            target: "127.0.0.1:8120".parse().unwrap(),
            retry_delay: Duration::from_secs(1),
            last_error: "connection refused".to_string(),
        })
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>();

        assert!(
            connecting
                .iter()
                .any(|line| line.contains("Connecting to 127.0.0.1:8120"))
        );
        assert!(
            reconnecting
                .iter()
                .any(|line| line.contains("Reconnecting to 127.0.0.1:8120 in 1.0s"))
        );
        assert!(
            reconnecting
                .iter()
                .any(|line| line.contains("Last error: connection refused"))
        );
    }

    #[tokio::test]
    async fn run_attached_tui_rejects_non_loopback_target() {
        let err = run_attached_tui("10.0.0.5:8120".parse().unwrap())
            .await
            .expect_err("non-loopback attach should fail");

        assert!(format!("{err:#}").contains("must connect to a loopback address"));
    }

    #[test]
    fn renderable_dashboard_state_limits_local_log_tail() {
        let log_buffer = LogBuffer::new();
        for i in 0..(LOCAL_TUI_LOG_LINE_LIMIT + 10) {
            log_buffer.push(format!("line {i}"));
        }

        let app = TuiAppBuilder::new(
            MetricsCollector::new(1),
            Arc::new(BackendSelector::new()),
            Arc::from(vec![
                Server::builder("backend.example.com", Port::try_new(119).unwrap())
                    .name("Backend")
                    .build()
                    .unwrap(),
            ]),
        )
        .with_log_buffer(log_buffer)
        .build();

        let state = renderable_dashboard_state(&app);

        assert_eq!(state.log_lines.len(), LOCAL_TUI_LOG_LINE_LIMIT);
        assert_eq!(state.log_lines.first().map(String::as_str), Some("line 10"));
        assert_eq!(state.log_lines.last().map(String::as_str), Some("line 265"));
    }

    #[test]
    fn renderable_attached_state_applies_local_overrides() {
        let state = DashboardState {
            metrics: crate::tui::dashboard::DashboardMetrics::default(),
            backend_views: Vec::new(),
            top_users: Vec::new(),
            client_history: Vec::new(),
            system_stats: SystemStats::default(),
            view_mode: ViewMode::Normal,
            show_details: false,
            log_lines: vec!["line".to_string()],
            buffer_pool: None,
        };
        let attached =
            AttachedDashboard::connected("127.0.0.1:8120".parse().unwrap(), Some(Arc::new(state)));
        let mut overrides = AttachedViewOverrides::default();

        overrides.toggle_details(attached.latest_state.as_deref());
        overrides.toggle_log_fullscreen(attached.latest_state.as_deref());

        let rendered = renderable_attached_state(&attached, &overrides).expect("dashboard state");
        assert!(rendered.show_details);
        assert_eq!(rendered.view_mode, ViewMode::LogFullscreen);
        assert_eq!(rendered.state.log_lines, vec!["line".to_string()]);
    }

    #[test]
    fn renderable_attached_state_preserves_local_overrides_across_new_remote_snapshots() {
        let state = DashboardState {
            metrics: crate::tui::dashboard::DashboardMetrics::default(),
            backend_views: Vec::new(),
            top_users: Vec::new(),
            client_history: Vec::new(),
            system_stats: SystemStats::default(),
            view_mode: ViewMode::Normal,
            show_details: false,
            log_lines: vec!["first".to_string()],
            buffer_pool: None,
        };
        let mut overrides = AttachedViewOverrides::default();
        overrides.toggle_details(Some(&state));
        overrides.toggle_log_fullscreen(Some(&state));

        let next_state = DashboardState {
            log_lines: vec!["second".to_string()],
            ..state
        };
        let attached = AttachedDashboard::connected(
            "127.0.0.1:8120".parse().unwrap(),
            Some(Arc::new(next_state)),
        );

        let rendered = renderable_attached_state(&attached, &overrides).expect("dashboard state");
        assert!(rendered.show_details);
        assert_eq!(rendered.view_mode, ViewMode::LogFullscreen);
        assert_eq!(rendered.state.log_lines, vec!["second".to_string()]);
    }
}
