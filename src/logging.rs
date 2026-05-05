//! Centralized logging setup with dual output (`debug.log` plus console or TUI).

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::UiMode;

fn env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}

fn file_filter(file_level: &str) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::new(file_level)
}

fn debug_log_writer() -> (NonBlocking, WorkerGuard) {
    let file_appender = tracing_appender::rolling::never(".", "debug.log");
    tracing_appender::non_blocking(file_appender)
}

fn leak_guard(guard: WorkerGuard) {
    // SAFETY: Intentionally leak the WorkerGuard to keep the file appender
    // alive for the program lifetime. Drop would flush and close the writer.
    std::mem::forget(guard);
}

fn init_headless_subscriber(file_level: &str) {
    let (debug_log, guard) = debug_log_writer();

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(|| std::io::LineWriter::new(std::io::stdout()))
                .with_filter(env_filter()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(debug_log)
                .with_ansi(false)
                .with_filter(file_filter(file_level)),
        )
        .init();

    leak_guard(guard);
}

fn init_tui_subscriber(file_level: &str) -> crate::tui::LogBuffer {
    let (debug_log, guard) = debug_log_writer();
    let log_buffer = crate::tui::LogBuffer::new();
    let log_writer = crate::tui::LogMakeWriter::new(log_buffer.clone());

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(log_writer)
                .with_ansi(false)
                .with_target(false)
                .compact()
                .with_filter(env_filter()),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(debug_log)
                .with_ansi(false)
                .with_filter(file_filter(file_level)),
        )
        .init();

    leak_guard(guard);
    log_buffer
}

/// Initialize logging with dual output for legacy headless call sites.
///
/// Headless mode logs to line-buffered stdout and `debug.log`.
pub fn init_dual_logging(file_level: &str) {
    let _ = init_logging(UiMode::Headless, file_level);
}

/// Initialize logging for the unified binary.
///
/// Headless mode logs to line-buffered stdout and `debug.log`.
/// TUI mode logs to the in-memory TUI buffer and `debug.log`.
pub fn init_logging(ui_mode: UiMode, file_level: &str) -> Option<crate::tui::LogBuffer> {
    match ui_mode {
        UiMode::Headless => {
            init_headless_subscriber(file_level);
            None
        }
        UiMode::Tui => Some(init_tui_subscriber(file_level)),
    }
}
