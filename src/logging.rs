//! Centralized logging setup for stdout, local TUI capture, and optional debug files.

use tracing_appender::non_blocking::{NonBlocking, WorkerGuard};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

use crate::UiMode;
use crate::config::LogFormat;

pub mod event_gate;
pub use event_gate::{BurstLimiter, Cleared, EventGate, OPERATOR_EVENT_WINDOW, SummaryWindow};

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

/// Whether stdout log lines carry ANSI colour escapes.
///
/// Colour helps a person reading a terminal, but in a container log driver or
/// a pipe the escapes land in the stored text: `\x1b[31mERROR` no longer
/// matches a search for `ERROR`, and field values such as `pool=backend-a`
/// stop being whole words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Ansi,
    Plain,
}

impl ColorMode {
    /// Decide from whether stdout is a terminal and the `NO_COLOR` value.
    ///
    /// Follows <https://no-color.org>: a set, non-empty `NO_COLOR` disables
    /// colour even on a terminal; an empty value is ignored.
    #[must_use]
    pub fn detect(stdout_is_terminal: bool, no_color: Option<&std::ffi::OsStr>) -> Self {
        let no_color_requested = no_color.is_some_and(|value| !value.is_empty());
        if stdout_is_terminal && !no_color_requested {
            Self::Ansi
        } else {
            Self::Plain
        }
    }

    fn for_stdout() -> Self {
        use std::io::IsTerminal;
        Self::detect(
            std::io::stdout().is_terminal(),
            std::env::var_os("NO_COLOR").as_deref(),
        )
    }

    const fn uses_ansi(self) -> bool {
        matches!(self, Self::Ansi)
    }
}

/// Build the formatting layer for operator-facing log output.
///
/// JSON puts the message and event fields at the top level (`flatten_event`)
/// so a log store can index `event`, `backend` and similar fields without a
/// nested path, and keeps the innermost span under `span` for context.
fn output_layer<S, W>(
    format: LogFormat,
    color: ColorMode,
    writer: W,
) -> Box<dyn Layer<S> + Send + Sync + 'static>
where
    S: tracing::Subscriber + for<'span> LookupSpan<'span>,
    W: for<'writer> tracing_subscriber::fmt::MakeWriter<'writer> + Send + Sync + 'static,
{
    match format {
        LogFormat::Text => tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(color.uses_ansi())
            .boxed(),
        LogFormat::Json => tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .with_writer(writer)
            .boxed(),
    }
}

fn init_headless_subscriber(
    format: LogFormat,
    capture_in_memory_logs: bool,
) -> Option<crate::tui::LogBuffer> {
    let log_buffer = capture_in_memory_logs.then(crate::tui::LogBuffer::new);
    let (stdout_log, guard) = stdout_log_writer();

    let subscriber = tracing_subscriber::registry()
        .with(output_layer(format, ColorMode::for_stdout(), stdout_log).with_filter(env_filter()))
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
    let _ = init_logging(UiMode::Headless, file_level, LogFormat::Text, false, false);
}

/// Initialize logging for the unified binary.
///
/// Headless mode logs to non-blocking stdout only.
/// TUI mode logs to the in-memory TUI buffer and may also mirror to `debug.log`
/// when explicitly enabled for local debugging sessions. `format` applies to
/// headless stdout only; the TUI buffer and `debug.log` are read by people.
#[must_use]
pub fn init_logging(
    ui_mode: UiMode,
    file_level: &str,
    format: LogFormat,
    capture_in_memory_logs: bool,
    write_debug_log: bool,
) -> Option<crate::tui::LogBuffer> {
    match ui_mode {
        UiMode::Headless => init_headless_subscriber(format, capture_in_memory_logs),
        UiMode::Tui => init_tui_subscriber(file_level, capture_in_memory_logs, write_debug_log),
    }
}

/// One event held back during startup.
///
/// Fields other than `event` are folded into `text`; `event` stays separate so
/// a replayed line keeps the field operators alert on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StartupEvent {
    level: tracing::Level,
    event: Option<String>,
    text: String,
}

/// Log events recorded before the process-wide subscriber exists.
///
/// The binary needs the loaded config (log level, UI mode) to build its
/// subscriber, so anything logged while loading it (ignored config keys,
/// legacy-schema migrations, the backend list) would otherwise have no
/// subscriber and be dropped.
#[derive(Debug, Default)]
#[must_use = "replay the events once logging is initialized, or print them if startup fails"]
pub struct StartupEvents(Vec<StartupEvent>);

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
        for StartupEvent { level, event, text } in self.0 {
            match (level, event) {
                (tracing::Level::ERROR, Some(event)) => {
                    tracing::error!(target: STARTUP_TARGET, event, "{text}");
                }
                (tracing::Level::WARN, Some(event)) => {
                    tracing::warn!(target: STARTUP_TARGET, event, "{text}");
                }
                (tracing::Level::INFO, Some(event)) => {
                    tracing::info!(target: STARTUP_TARGET, event, "{text}");
                }
                (tracing::Level::DEBUG, Some(event)) => {
                    tracing::debug!(target: STARTUP_TARGET, event, "{text}");
                }
                (tracing::Level::TRACE, Some(event)) => {
                    tracing::trace!(target: STARTUP_TARGET, event, "{text}");
                }
                (tracing::Level::ERROR, None) => tracing::error!(target: STARTUP_TARGET, "{text}"),
                (tracing::Level::WARN, None) => tracing::warn!(target: STARTUP_TARGET, "{text}"),
                (tracing::Level::INFO, None) => tracing::info!(target: STARTUP_TARGET, "{text}"),
                (tracing::Level::DEBUG, None) => tracing::debug!(target: STARTUP_TARGET, "{text}"),
                (tracing::Level::TRACE, None) => tracing::trace!(target: STARTUP_TARGET, "{text}"),
            }
        }
    }

    /// Startup failed before logging could be set up: print warnings and
    /// errors so the reason is not lost.
    pub fn print_to_stderr(self) {
        for StartupEvent { level, text, .. } in self.0 {
            if level <= tracing::Level::WARN {
                eprintln!("{level:>5} {text}");
            }
        }
    }
}

const STARTUP_TARGET: &str = "nntp_proxy::startup";

struct StartupCaptureLayer(std::sync::Arc<std::sync::Mutex<Vec<StartupEvent>>>);

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
            .push(StartupEvent {
                level: *event.metadata().level(),
                event: text.event,
                text: text.text,
            });
    }
}

#[derive(Default)]
struct EventText {
    text: String,
    event: Option<String>,
}

impl tracing::field::Visit for EventText {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "event" {
            self.event = Some(value.to_string());
        } else {
            self.record_debug(field, &value);
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write;
        if !self.text.is_empty() {
            self.text.push(' ');
        }
        let _ = if field.name() == "message" {
            write!(self.text, "{value:?}")
        } else {
            write!(self.text, "{}={value:?}", field.name())
        };
    }
}

/// Helpers for tests that inspect formatted log output.
#[cfg(test)]
pub(crate) mod test_support {
    use tracing_subscriber::layer::SubscriberExt;

    /// Collects formatted output so tests can inspect exact log lines.
    #[derive(Clone, Default)]
    pub(crate) struct CapturedOutput(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl CapturedOutput {
        pub(crate) fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }

        /// Every captured line parsed as JSON.
        pub(crate) fn json_lines(&self) -> Vec<serde_json::Value> {
            self.text()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect()
        }

        /// Captured lines whose `event` field equals `event`.
        pub(crate) fn events(&self, event: &str) -> Vec<serde_json::Value> {
            self.json_lines()
                .into_iter()
                .filter(|line| line["event"] == event)
                .collect()
        }
    }

    impl std::io::Write for CapturedOutput {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CapturedOutput {
        type Writer = Self;
        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    /// Route this thread's events, at every level, to JSON lines in the
    /// returned buffer until the guard drops. Works across `.await` in a
    /// current-thread runtime.
    pub(crate) fn capture_json() -> (CapturedOutput, tracing::subscriber::DefaultGuard) {
        let output = CapturedOutput::default();
        let subscriber = tracing_subscriber::registry().with(super::output_layer(
            crate::config::LogFormat::Json,
            super::ColorMode::Plain,
            output.clone(),
        ));
        let guard = tracing::subscriber::set_default(subscriber);
        (output, guard)
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
                StartupEvent {
                    level: tracing::Level::WARN,
                    event: None,
                    text: "Ignoring unknown config key 'servers.0.tierr'".to_string(),
                },
                StartupEvent {
                    level: tracing::Level::INFO,
                    event: None,
                    text: "Loaded backends count=2".to_string(),
                },
            ]
        );
    }

    #[test]
    fn replayed_startup_events_keep_the_event_field() {
        let ((), events) = capture_startup_events(|| {
            tracing::warn!(
                event = "config_key_ignored",
                key = "servers.0.tierr",
                "Ignoring unknown config key"
            );
        });
        let (output, guard) = test_support::capture_json();
        events.replay();
        drop(guard);

        let lines = output.events("config_key_ignored");
        assert_eq!(lines.len(), 1, "{}", output.text());
        assert_eq!(lines[0]["level"], "WARN");
        assert_eq!(
            lines[0]["message"],
            "Ignoring unknown config key key=\"servers.0.tierr\""
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
    fn color_only_on_a_terminal_without_no_color() {
        use std::ffi::OsStr;
        assert_eq!(ColorMode::detect(true, None), ColorMode::Ansi);
        assert_eq!(ColorMode::detect(false, None), ColorMode::Plain);
        assert_eq!(
            ColorMode::detect(true, Some(OsStr::new("1"))),
            ColorMode::Plain
        );
        assert_eq!(
            ColorMode::detect(false, Some(OsStr::new("1"))),
            ColorMode::Plain
        );
        // no-color.org: an empty NO_COLOR does not disable colour.
        assert_eq!(
            ColorMode::detect(true, Some(OsStr::new(""))),
            ColorMode::Ansi
        );
    }

    use super::test_support::CapturedOutput;

    fn capture(format: LogFormat, color: ColorMode, emit: impl FnOnce()) -> String {
        let output = CapturedOutput::default();
        let subscriber =
            tracing_subscriber::registry().with(output_layer(format, color, output.clone()));
        tracing::subscriber::with_default(subscriber, emit);
        output.text()
    }

    #[test]
    fn json_lines_carry_timestamp_level_target_message_fields_and_span() {
        let out = capture(LogFormat::Json, ColorMode::Ansi, || {
            let span = tracing::info_span!("session", client = "127.0.0.1:40000");
            let _entered = span.enter();
            tracing::warn!(
                event = "backend_unreachable",
                backend = "backend-a",
                count = 3_u64,
                "Backend unreachable"
            );
            drop(_entered);
            tracing::info!("plain");
        });
        let lines: Vec<serde_json::Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(lines.len(), 2, "one JSON object per line: {out}");

        let first = &lines[0];
        assert!(first["timestamp"].is_string());
        assert_eq!(first["level"], "WARN");
        assert_eq!(first["target"], "nntp_proxy::logging::tests");
        assert_eq!(first["message"], "Backend unreachable");
        assert_eq!(first["event"], "backend_unreachable");
        assert_eq!(first["backend"], "backend-a");
        assert_eq!(first["count"], 3);
        assert_eq!(first["span"]["name"], "session");
        assert_eq!(first["span"]["client"], "127.0.0.1:40000");
        assert!(!out.contains('\x1b'), "JSON never carries ANSI escapes");

        assert_eq!(lines[1]["message"], "plain");
        assert!(lines[1].get("span").is_none());
    }

    #[test]
    fn plain_text_lines_have_no_escapes() {
        let out = capture(LogFormat::Text, ColorMode::Plain, || {
            tracing::error!(pool = "backend-a", "Failed");
        });
        assert!(!out.contains('\x1b'), "{out:?}");
        assert!(out.contains("ERROR"));
        assert!(out.contains("pool=\"backend-a\""), "{out:?}");

        let coloured = capture(LogFormat::Text, ColorMode::Ansi, || {
            tracing::error!("Failed");
        });
        assert!(coloured.contains('\x1b'));
    }

    #[test]
    fn stdout_log_writer_is_constructible() {
        let _ = stdout_log_writer();
    }
}
