//! Centralized logging setup with dual output (stdout + debug.log)

use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::UiMode;

fn init_headless_subscriber(file_level: &str) {
    let file_appender = tracing_appender::rolling::never(".", "debug.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let file_filter = tracing_subscriber::EnvFilter::new(file_level);

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stdout)
                .with_filter(env_filter),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_filter(file_filter),
        )
        .init();

    // SAFETY: Intentionally leak the WorkerGuard to keep the file appender
    // alive for the program lifetime. Drop would flush and close the writer.
    std::mem::forget(guard);
}

/// Initialize logging with dual output: stdout + debug.log file
///
/// Stdout uses `RUST_LOG` (default "info"). The file layer captures events
/// at the specified `file_level` (default "warn") so that root-cause errors
/// are in debug.log even if `RUST_LOG` is set to a narrow filter.
///
/// The guard is forgotten to keep the file appender alive for the program lifetime.
pub fn init_dual_logging(file_level: &str) {
    let _ = init_logging(UiMode::Headless, file_level);
}

/// Initialize logging for the unified binary.
///
/// Headless mode logs to stdout and `debug.log`.
/// TUI mode logs to the in-memory TUI buffer and `debug.log`.
pub fn init_logging(ui_mode: UiMode, file_level: &str) -> Option<crate::tui::LogBuffer> {
    match ui_mode {
        UiMode::Headless => {
            init_headless_subscriber(file_level);
            None
        }
        UiMode::Tui => init_tui_logging(false, file_level),
    }
}

/// Initialize logging with TUI buffer + debug.log file
///
/// For TUI mode, logs go to the in-memory buffer for display.
/// For headless mode, logs go to stdout.
/// Both modes also write to debug.log at the specified `file_level`.
pub fn init_tui_logging(headless: bool, file_level: &str) -> Option<crate::tui::LogBuffer> {
    if headless {
        init_headless_subscriber(file_level);
        return None;
    }

    let file_appender = tracing_appender::rolling::never(".", "debug.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    // File captures events at configured level (e.g., "warn", "info", etc.)
    let file_filter = tracing_subscriber::EnvFilter::new(file_level);

    let log_buffer = crate::tui::LogBuffer::new();
    let log_writer = crate::tui::LogMakeWriter::new(log_buffer.clone());

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(log_writer)
                .with_ansi(false)
                .with_target(false)
                .compact()
                .with_filter(env_filter),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(non_blocking)
                .with_ansi(false)
                .with_filter(file_filter),
        )
        .init();

    // SAFETY: Intentionally leak the WorkerGuard to keep the file appender
    // alive for the program lifetime. Drop would flush and close the writer.
    std::mem::forget(guard);
    Some(log_buffer)
}
