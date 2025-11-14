//! Terminal User Interface (TUI) for the NNTP proxy
//!
//! Provides real-time visualization of proxy metrics when running in an interactive terminal.

mod app;
mod constants;
mod helpers;
pub mod log_capture;
mod types;
mod ui;
#[cfg(test)]
mod ui_tests;

pub use app::{TuiApp, TuiAppBuilder, ViewMode};
pub use log_capture::{LogBuffer, LogMakeWriter};
pub use ui::render_ui;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;
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

/// Main TUI event loop
async fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut TuiApp,
    shutdown_rx: &mut mpsc::Receiver<()>,
) -> Result<()> {
    // Create update interval (4 times per second for responsive UI)
    let mut update_interval = tokio::time::interval(Duration::from_millis(250));

    loop {
        // Render the UI
        terminal.draw(|f| ui::render_ui(f, app))?;

        tokio::select! {
            // External shutdown signal
            _ = shutdown_rx.recv() => {
                // External shutdown (e.g., Ctrl+C from signal handler)
                break;
            }
            // Update timer
            _ = update_interval.tick() => {
                app.update();

                // Check for keyboard input (non-blocking)
                if event::poll(Duration::from_millis(0))?
                    && let Event::Key(key) = event::read()?
                    && key.kind == KeyEventKind::Press
                {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            // User pressed 'q' - exit TUI and shut down app
                            break;
                        }
                        KeyCode::Char('c') if key.modifiers.contains(event::KeyModifiers::CONTROL) => {
                            // Ctrl-C pressed - exit TUI and shut down app
                            break;
                        }
                        KeyCode::Char('l') => {
                            // Toggle log fullscreen mode
                            app.toggle_log_fullscreen();
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    Ok(())
}
