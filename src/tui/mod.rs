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

pub use app::{TuiApp, TuiAppBuilder, ViewMode};
pub use dashboard::{BackendView, BufferPoolStats, DashboardState};
pub use log_capture::{LogBuffer, LogMakeWriter};
pub use remote::{run_dashboard_publisher, spawn_dashboard_reader};
pub use system_stats::{SystemMonitor, SystemStats};
pub use ui::render_ui;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
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

/// Setup the terminal for TUI rendering
fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

/// Restore the terminal to its original state
fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    // Clear the terminal first to prevent escape sequences leaking to shell
    terminal.clear()?;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    Ok(())
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
    // Setup terminal
    let mut terminal = setup_terminal()?;

    // Setup panic hook to ensure terminal cleanup
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    // Run the app
    let result = run_app(&mut terminal, &mut app, &mut shutdown_rx).await;

    // Restore terminal
    restore_terminal(&mut terminal)?;

    // Signal shutdown when TUI exits
    let _ = shutdown_tx.send(()).await;

    result
}

/// Run the TUI as a remote dashboard client connected to a headless publisher.
pub async fn run_attached_tui(connect_addr: SocketAddr) -> Result<()> {
    let mut terminal = setup_terminal()?;

    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(panic_info);
    }));

    let (state_tx, mut state_rx) = tokio::sync::watch::channel(None::<DashboardState>);
    let _reader = spawn_dashboard_reader(connect_addr, state_tx);

    let result = run_attached_app(&mut terminal, &mut state_rx).await;

    restore_terminal(&mut terminal)?;
    result
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
    terminal.draw(|f| ui::render_ui(f, &app.snapshot_state()))?;

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

                // Check events on blocking thread pool to not block async runtime
                let has_events = tokio::task::spawn_blocking(|| {
                    event::poll(Duration::from_millis(0))
                }).await??;

                if has_events {
                    let key_event = tokio::task::spawn_blocking(|| {
                        event::read()
                    }).await??;

                    if let Event::Key(key) = key_event
                        && key.kind == KeyEventKind::Press
                    {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => should_quit = true,
                            KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                should_quit = true;
                            }
                            KeyCode::Char('l') => app.toggle_log_fullscreen(),
                            KeyCode::Char('d') => app.toggle_details(),
                            _ => {}
                        }
                    }
                }

                if should_quit {
                    break;
                }

                terminal.draw(|f| ui::render_ui(f, &app.snapshot_state()))?;
            }
        }
    }

    Ok(())
}

async fn run_attached_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state_rx: &mut tokio::sync::watch::Receiver<Option<DashboardState>>,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let mut update_interval = tokio::time::interval(Duration::from_millis(250));
    let mut should_quit = false;

    let state = state_rx.borrow().clone();
    terminal.draw(|f| draw_attached_dashboard(f, state.as_ref()))?;

    loop {
        tokio::select! {
            _ = update_interval.tick() => {
                let has_events = tokio::task::spawn_blocking(|| {
                    event::poll(Duration::from_millis(0))
                }).await??;

                if has_events {
                    let key_event = tokio::task::spawn_blocking(|| {
                        event::read()
                    }).await??;

                    if let Event::Key(key) = key_event
                        && key.kind == KeyEventKind::Press
                    {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => should_quit = true,
                            KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                                should_quit = true;
                            }
                            _ => {}
                        }
                    }
                }

                if should_quit {
                    break;
                }

                let state = state_rx.borrow().clone();
                terminal.draw(|f| draw_attached_dashboard(f, state.as_ref()))?;
            }
        }
    }

    Ok(())
}

fn draw_attached_dashboard(f: &mut ratatui::Frame, state: Option<&DashboardState>) {
    if let Some(state) = state {
        ui::render_ui(f, state);
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
        "- Remote Dashboard".fg(Color::White),
    ]))
    .alignment(Alignment::Center);

    let body =
        Paragraph::new(Line::from("Connecting to dashboard...")).alignment(Alignment::Center);

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
