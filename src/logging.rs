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

fn stdout_log_writer() -> (NonBlocking, WorkerGuard) {
    tracing_appender::non_blocking(std::io::stdout())
}

fn leak_guard(guard: WorkerGuard) {
    // SAFETY: Intentionally leak the WorkerGuard to keep the file appender
    // alive for the program lifetime. Drop would flush and close the writer.
    std::mem::forget(guard);
}

fn init_headless_subscriber(capture_in_memory_logs: bool) -> Option<crate::tui::LogBuffer> {
    let log_buffer = capture_in_memory_logs.then(crate::tui::LogBuffer::new);
    let (stdout_log, guard) = stdout_log_writer();

    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(stdout_log)
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

    leak_guard(guard);
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
/// Headless mode logs to non-blocking stdout only.
pub fn init_dual_logging(file_level: &str) {
    let _ = init_logging(UiMode::Headless, file_level, false, false);
}

/// Initialize logging for the unified binary.
///
/// Headless mode logs to non-blocking stdout only.
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

/// Log events recorded before the process-wide subscriber exists.
///
/// The binary needs the loaded config (log level, UI mode) to build its
/// subscriber, so anything logged while loading it (ignored config keys,
/// legacy-schema migrations, the backend list) would otherwise have no
/// subscriber and be dropped.
#[derive(Debug, Default)]
#[must_use = "replay the events once logging is initialized, or print them if startup fails"]
pub struct StartupEvents(Vec<(tracing::Level, String)>);

/// Run `load` with every log event it emits held back for [`StartupEvents::replay`].
pub fn capture_startup_events<T>(load: impl FnOnce() -> T) -> (T, StartupEvents) {
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(StartupCaptureLayer(events.clone()));
    let result = tracing::subscriber::with_default(subscriber, load);
    let events = std::mem::take(
        &mut *events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    (result, StartupEvents(events))
}

impl StartupEvents {
    /// Re-emit the held events through the now-initialized subscriber, which
    /// applies its own filters.
    pub fn replay(self) {
        for (level, text) in self.0 {
            match level {
                tracing::Level::ERROR => tracing::error!(target: STARTUP_TARGET, "{text}"),
                tracing::Level::WARN => tracing::warn!(target: STARTUP_TARGET, "{text}"),
                tracing::Level::INFO => tracing::info!(target: STARTUP_TARGET, "{text}"),
                tracing::Level::DEBUG => tracing::debug!(target: STARTUP_TARGET, "{text}"),
                tracing::Level::TRACE => tracing::trace!(target: STARTUP_TARGET, "{text}"),
            }
        }
    }

    /// Startup failed before logging could be set up: print warnings and
    /// errors so the reason is not lost.
    pub fn print_to_stderr(self) {
        for (level, text) in self.0 {
            if level <= tracing::Level::WARN {
                eprintln!("{level:>5} {text}");
            }
        }
    }
}

const STARTUP_TARGET: &str = "nntp_proxy::startup";

struct StartupCaptureLayer(std::sync::Arc<std::sync::Mutex<Vec<(tracing::Level, String)>>>);

impl<S: tracing::Subscriber> Layer<S> for StartupCaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut text = EventText::default();
        event.record(&mut text);
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((*event.metadata().level(), text.0));
    }
}

#[derive(Default)]
struct EventText(String);

impl tracing::field::Visit for EventText {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        let _ = if field.name() == "message" {
            write!(self.0, "{value:?}")
        } else {
            write!(self.0, "{}={value:?}", field.name())
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn startup_events_are_held_until_replayed() {
        let (value, events) = capture_startup_events(|| {
            tracing::warn!("Ignoring unknown config key '{}'", "servers.0.tierr");
            tracing::info!(count = 2, "Loaded backends");
            7
        });
        assert_eq!(value, 7);
        assert_eq!(
            events.0,
            [
                (
                    tracing::Level::WARN,
                    "Ignoring unknown config key 'servers.0.tierr'".to_string()
                ),
                (tracing::Level::INFO, "Loaded backends count=2".to_string()),
            ]
        );
    }

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

    #[test]
    fn stdout_log_writer_is_constructible() {
        let _ = stdout_log_writer();
    }
}
