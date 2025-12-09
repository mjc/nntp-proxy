//! Centralized logging setup with dual output (stdout + debug.log)

use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Initialize logging with dual output: stdout + debug.log file
///
/// Both outputs use the same log level from RUST_LOG environment variable.
/// Defaults to "info" level if RUST_LOG is not set.
///
/// The _guard is forgotten to keep the file appender alive for the program lifetime.
pub fn init_dual_logging() {
    let file_appender = tracing_appender::rolling::never(".", "debug.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let file_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

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

    // Keep guard alive for the program lifetime
    std::mem::forget(_guard);
}

/// Initialize logging with TUI buffer + debug.log file
///
/// For TUI mode, logs go to the in-memory buffer for display.
/// For headless mode, logs go to stdout.
/// Both modes also write to debug.log.
pub fn init_tui_logging(headless: bool) -> Option<crate::tui::LogBuffer> {
    let file_appender = tracing_appender::rolling::never(".", "debug.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let file_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    if headless {
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
        std::mem::forget(_guard);
        return None;
    }

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

    std::mem::forget(_guard);
    Some(log_buffer)
}
