//! Terminal User Interface (TUI) for the NNTP proxy
//!
//! Provides real-time visualization of proxy metrics when running in an interactive terminal.

mod app;
mod ui;

pub use app::TuiApp;
pub use ui::render_ui;

use anyhow::Result;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io;
use std::time::Duration;
use tokio::sync::mpsc;

/// Run the TUI event loop
///
/// This function takes ownership of the terminal and runs until the user presses 'q' or Ctrl+C.
/// When the TUI exits, it sends a shutdown signal via the provided channel.
///
/// # Arguments
/// * `app` - The TUI application state
/// * `shutdown_tx` - Sender to signal shutdown when TUI exits
///
/// # Returns
/// Ok(()) when the TUI exits normally, or an error if terminal operations fail
pub async fn run_tui(mut app: TuiApp, shutdown_tx: mpsc::Sender<()>) -> Result<()> {
    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Clear the terminal
    terminal.clear()?;

    let result = run_app(&mut terminal, &mut app).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    // Signal shutdown when TUI exits
    let _ = shutdown_tx.send(()).await;

    result
}

/// Main TUI event loop
async fn run_app<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut TuiApp,
) -> Result<()> {
    // Create update interval (4 times per second for responsive UI)
    let mut update_interval = tokio::time::interval(Duration::from_millis(250));

    loop {
        // Render the UI
        terminal.draw(|f| ui::render_ui(f, app))?;

        tokio::select! {
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
                        _ => {}
                    }
                }
            }
        }
    }

    Ok(())
}
