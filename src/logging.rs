//! Centralized logging setup for stdout, local TUI capture, and optional debug files.

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

use crate::UiMode;

#[cfg(feature = "tokio-console")]
fn tokio_console_enabled() -> bool {
    std::env::var_os("NNTP_PROXY_TOKIO_CONSOLE").is_some_and(|value| value != "0")
}

#[cfg(feature = "tokio-console")]
fn tokio_console_layer<S>() -> Option<Box<dyn Layer<S> + Send + Sync + 'static>>
where
    S: tracing::Subscriber,
    S: for<'span> LookupSpan<'span>,
{
    tokio_console_enabled().then(|| {
        eprintln!(
            "tokio-console enabled; console-subscriber environment controls its bind address"
        );
        console_subscriber::ConsoleLayer::builder()
            .with_default_env()
            .spawn()
            .boxed()
    })
}

#[cfg(not(feature = "tokio-console"))]
fn tokio_console_layer<S>() -> Option<Box<dyn Layer<S> + Send + Sync + 'static>>
where
    S: tracing::Subscriber,
    S: for<'span> LookupSpan<'span>,
{
    None
}

fn env_filter() -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
}

fn file_filter(file_level: &str) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::new(file_level)
}

fn wants_debug_log_file(file_level: &str) -> bool {
    file_level
        .split(',')
        .filter_map(|directive| directive.rsplit('=').next())
        .map(str::trim)
        .any(|level| level.eq_ignore_ascii_case("debug") || level.eq_ignore_ascii_case("trace"))
}

#[must_use]
pub fn should_write_debug_log(ui_mode: UiMode, is_attached_tui: bool, file_level: &str) -> bool {
    ui_mode == UiMode::Tui && !is_attached_tui && wants_debug_log_file(file_level)
}

#[must_use]
pub const fn should_capture_in_memory_logs(
    ui_mode: UiMode,
    is_attached_tui: bool,
    capture_headless_tui_buffer: bool,
) -> bool {
    match ui_mode {
        UiMode::Headless => capture_headless_tui_buffer,
        UiMode::Tui => !is_attached_tui,
    }
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

fn init_headless_subscriber(capture_in_memory_logs: bool) -> Option<crate::tui::LogBuffer> {
    let log_buffer = capture_in_memory_logs.then(crate::tui::LogBuffer::new);

    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(|| std::io::LineWriter::new(std::io::stdout()))
                .with_filter(env_filter()),
        )
        .with(tokio_console_layer());

    match log_buffer.clone() {
        Some(log_buffer) => subscriber
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(crate::tui::LogMakeWriter::new(log_buffer))
                    .with_ansi(false)
                    .with_target(false)
                    .compact()
                    .with_filter(env_filter()),
            )
            .init(),
        None => subscriber.init(),
    }

    log_buffer
}

fn init_tui_subscriber(
    file_level: &str,
    capture_in_memory_logs: bool,
    write_debug_log: bool,
) -> Option<crate::tui::LogBuffer> {
    match (capture_in_memory_logs, write_debug_log) {
        (true, true) => {
            let log_buffer = crate::tui::LogBuffer::new();
            let (debug_log, guard) = debug_log_writer();
            tracing_subscriber::registry()
                .with(tokio_console_layer())
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(crate::tui::LogMakeWriter::new(log_buffer.clone()))
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
            Some(log_buffer)
        }
        (true, false) => {
            let log_buffer = crate::tui::LogBuffer::new();
            tracing_subscriber::registry()
                .with(tokio_console_layer())
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(crate::tui::LogMakeWriter::new(log_buffer.clone()))
                        .with_ansi(false)
                        .with_target(false)
                        .compact()
                        .with_filter(env_filter()),
                )
                .init();
            Some(log_buffer)
        }
        (false, true) => {
            let (debug_log, guard) = debug_log_writer();
            tracing_subscriber::registry()
                .with(tokio_console_layer())
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(debug_log)
                        .with_ansi(false)
                        .with_filter(file_filter(file_level)),
                )
                .init();
            leak_guard(guard);
            None
        }
        (false, false) => {
            tracing_subscriber::registry()
                .with(tokio_console_layer())
                .init();
            None
        }
    }
}

/// Initialize logging with dual output for legacy headless call sites.
///
/// Headless mode logs to line-buffered stdout only.
pub fn init_dual_logging(file_level: &str) {
    let _ = init_logging(UiMode::Headless, file_level, false, false);
}

/// Initialize logging for the unified binary.
///
/// Headless mode logs to line-buffered stdout only.
/// TUI mode logs to the in-memory TUI buffer and may also mirror to `debug.log`
/// when explicitly enabled for local debugging sessions.
#[must_use]
pub fn init_logging(
    ui_mode: UiMode,
    file_level: &str,
    capture_in_memory_logs: bool,
    write_debug_log: bool,
) -> Option<crate::tui::LogBuffer> {
    match ui_mode {
        UiMode::Headless => init_headless_subscriber(capture_in_memory_logs),
        UiMode::Tui => init_tui_subscriber(file_level, capture_in_memory_logs, write_debug_log),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_write_debug_log_requires_local_tui_and_debug_level() {
        assert!(should_write_debug_log(UiMode::Tui, false, "debug"));
        assert!(should_write_debug_log(UiMode::Tui, false, "trace"));
        assert!(should_write_debug_log(
            UiMode::Tui,
            false,
            "info,mycrate=debug"
        ));
        assert!(!should_write_debug_log(UiMode::Headless, false, "debug"));
        assert!(!should_write_debug_log(UiMode::Tui, true, "debug"));
        assert!(!should_write_debug_log(UiMode::Tui, false, "info"));
        assert!(!should_write_debug_log(
            UiMode::Tui,
            false,
            "warn,mycrate=info"
        ));
    }

    #[test]
    fn should_capture_in_memory_logs_skips_attached_clients() {
        assert!(should_capture_in_memory_logs(UiMode::Tui, false, false));
        assert!(!should_capture_in_memory_logs(UiMode::Tui, true, false));
        assert!(should_capture_in_memory_logs(UiMode::Headless, false, true));
        assert!(!should_capture_in_memory_logs(
            UiMode::Headless,
            false,
            false
        ));
    }
}
