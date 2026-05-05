//! Terminal User Interface (TUI) for the NNTP proxy
//!
//! Provides real-time visualization of proxy metrics when running in an interactive terminal.

mod app;
mod constants;
mod dashboard;
mod helpers;
pub mod log_capture;
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
use std::time::Duration;
use tokio::sync::mpsc;

type TuiTerminal = Terminal<CrosstermBackend<io::Stdout>>;

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
    pub latest_state: Option<DashboardState>,
    pub status: RemoteDashboardStatus,
}

impl AttachedDashboard {
    fn connecting(target: SocketAddr) -> Self {
        Self {
            latest_state: None,
            status: RemoteDashboardStatus::Connecting { target },
        }
    }

    fn connected(target: SocketAddr, latest_state: Option<DashboardState>) -> Self {
        Self {
            latest_state,
            status: RemoteDashboardStatus::Connected { target },
        }
    }

    fn reconnecting(
        target: SocketAddr,
        retry_delay: Duration,
        latest_state: Option<DashboardState>,
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
pub async fn run_attached_tui(connect_addr: SocketAddr) -> Result<()> {
    anyhow::ensure!(
        connect_addr.ip().is_loopback(),
        "Attached dashboard client must connect to a loopback address: {connect_addr}"
    );
    let (state_tx, mut state_rx) =
        tokio::sync::watch::channel(AttachedDashboard::connecting(connect_addr));
    let _reader = spawn_dashboard_reader(connect_addr, state_tx);
    let mut local_system_monitor = SystemMonitor::new();

    with_terminal_session(move |terminal| {
        Box::pin(async move {
            run_attached_app(terminal, &mut state_rx, &mut local_system_monitor).await
        })
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

    // Initial render
    terminal.draw(|f| ui::render_ui(f, &app.snapshot_state(), None, None))?;

    let mut should_quit = false;

    loop {
        tokio::select! {
            // External shutdown signal
            _ = shutdown_rx.recv() => {
                break;
            }
            // Update timer - check for events only on ticks to reduce overhead
            _ = update_interval.tick() => {
                app.update();

                if apply_tui_input(true, Some(app)).await? {
                    should_quit = true;
                }

                if should_quit {
                    break;
                }

                terminal.draw(|f| ui::render_ui(f, &app.snapshot_state(), None, None))?;
            }
        }
    }

    Ok(())
}

async fn run_attached_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state_rx: &mut tokio::sync::watch::Receiver<AttachedDashboard>,
    local_system_monitor: &mut SystemMonitor,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let mut update_interval = tokio::time::interval(Duration::from_millis(250));
    let mut should_quit = false;

    let state = state_rx.borrow().clone();
    let local_system_stats = local_system_monitor.update();
    terminal.draw(|f| draw_attached_dashboard(f, &state, &local_system_stats))?;

    loop {
        tokio::select! {
            _ = update_interval.tick() => {
                if apply_tui_input(false, None).await? {
                    should_quit = true;
                }

                if should_quit {
                    break;
                }

                let state = state_rx.borrow().clone();
                let local_system_stats = local_system_monitor.update();
                terminal.draw(|f| draw_attached_dashboard(f, &state, &local_system_stats))?;
            }
        }
    }

    Ok(())
}

fn draw_attached_dashboard(
    f: &mut ratatui::Frame,
    attached: &AttachedDashboard,
    local_system_stats: &SystemStats,
) {
    if let Some(state) = attached.latest_state.as_ref() {
        ui::render_ui(f, state, Some(local_system_stats), Some(&attached.status));
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
    if key.kind != KeyEventKind::Press {
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

async fn poll_tui_input(allow_dashboard_actions: bool) -> Result<TuiInputAction> {
    let has_events =
        tokio::task::spawn_blocking(|| event::poll(Duration::from_millis(0))).await??;

    if !has_events {
        return Ok(TuiInputAction::None);
    }

    let key_event = tokio::task::spawn_blocking(event::read).await??;
    Ok(match key_event {
        Event::Key(key) => key_event_action(key, allow_dashboard_actions),
        _ => TuiInputAction::None,
    })
}

async fn apply_tui_input(allow_dashboard_actions: bool, app: Option<&mut TuiApp>) -> Result<bool> {
    match poll_tui_input(allow_dashboard_actions).await? {
        TuiInputAction::Quit => Ok(true),
        TuiInputAction::ToggleLogFullscreen => {
            if let Some(app) = app {
                app.toggle_log_fullscreen();
            }
            Ok(false)
        }
        TuiInputAction::ToggleDetails => {
            if let Some(app) = app {
                app.toggle_details();
            }
            Ok(false)
        }
        TuiInputAction::None => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
