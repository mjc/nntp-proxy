//! Read-only Prometheus text-format exporter for proxy metrics.
//!
//! Two responsibilities live here, kept separate for testability:
//! - [`render_prometheus`]: a PURE function turning a [`MetricsSnapshot`] into
//!   Prometheus 0.0.4 exposition text. No I/O — this is where the formatting
//!   logic and the bulk of the tests live.
//! - the minimal HTTP layer (`parse_request_target`, `write_response`,
//!   `serve_connection`, added in later tasks): a `GET /metrics` responder over
//!   any async stream, driven by `runtime::spawn_metrics_exporter`.

// micros->seconds conversion casts u64 to f64; values are presentation timings.
#![allow(clippy::cast_precision_loss)]
// Items are intentionally forward-declared; consumers are wired in later tasks.
#![allow(dead_code)]

use std::borrow::Cow;
use std::fmt::Write as _;

use crate::metrics::MetricsSnapshot;
use crate::metrics::types::BackendHealthStatus;

/// Map backend health to a stable numeric gauge value.
const fn health_value(status: BackendHealthStatus) -> u8 {
    match status {
        BackendHealthStatus::Healthy => 2,
        BackendHealthStatus::Degraded => 1,
        BackendHealthStatus::Down => 0,
    }
}

/// Escape a label value per the Prometheus exposition format.
fn escape_label(value: &str) -> Cow<'_, str> {
    if value.bytes().any(|b| matches!(b, b'\\' | b'"' | b'\n')) {
        let mut out = String::with_capacity(value.len() + 8);
        for c in value.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                '\n' => out.push_str("\\n"),
                _ => out.push(c),
            }
        }
        Cow::Owned(out)
    } else {
        Cow::Borrowed(value)
    }
}

/// Emit a single-sample family (`# HELP`, `# TYPE`, one value line).
fn metric(out: &mut String, name: &str, typ: &str, help: &str, value: impl std::fmt::Display) {
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {typ}");
    let _ = writeln!(out, "{name} {value}");
}

/// Resolve a backend's display name (falls back to the numeric index).
fn backend_label(names: &[String], index: usize) -> Cow<'_, str> {
    match names.get(index) {
        Some(name) => escape_label(name),
        None => Cow::Owned(index.to_string()),
    }
}

/// Emit a per-backend counter/gauge family: `# HELP`/`# TYPE` once, then one
/// labelled sample per backend. `value_of` returns the formatted sample value.
fn backend_family<F>(
    out: &mut String,
    snapshot: &MetricsSnapshot,
    names: &[String],
    name: &str,
    typ: &str,
    help: &str,
    mut value_of: F,
) where
    F: FnMut(&crate::metrics::BackendStats) -> String,
{
    let _ = writeln!(out, "# HELP {name} {help}");
    let _ = writeln!(out, "# TYPE {name} {typ}");
    for stats in snapshot.backend_stats.iter() {
        let label = backend_label(names, stats.backend_id.as_index());
        let value = value_of(stats);
        let _ = writeln!(out, "{name}{{backend=\"{label}\"}} {value}");
    }
}

/// Emit a per-backend summary family (`_sum` seconds + `_count`).
fn backend_summary<S, C>(
    out: &mut String,
    snapshot: &MetricsSnapshot,
    names: &[String],
    base: &str,
    help: &str,
    mut sum_seconds: S,
    mut count: C,
) where
    S: FnMut(&crate::metrics::BackendStats) -> f64,
    C: FnMut(&crate::metrics::BackendStats) -> u64,
{
    let _ = writeln!(out, "# HELP {base} {help}");
    let _ = writeln!(out, "# TYPE {base} summary");
    for stats in snapshot.backend_stats.iter() {
        let label = backend_label(names, stats.backend_id.as_index());
        let sum = sum_seconds(stats);
        let cnt = count(stats);
        let _ = writeln!(out, "{base}_sum{{backend=\"{label}\"}} {sum}");
        let _ = writeln!(out, "{base}_count{{backend=\"{label}\"}} {cnt}");
    }
}

/// Render the full Prometheus exposition document for a snapshot.
///
/// `server_names` is indexed by backend id (as supplied to the metrics saver);
/// missing entries fall back to the numeric backend index. Per-user metrics are
/// intentionally excluded.
#[must_use]
pub(crate) fn render_prometheus(snapshot: &MetricsSnapshot, server_names: &[String]) -> String {
    let mut out = String::with_capacity(4096);

    // ---- proxy-level ----
    metric(
        &mut out,
        "nntp_connections_total",
        "counter",
        "Total client connections accepted.",
        snapshot.total_connections,
    );
    metric(
        &mut out,
        "nntp_active_connections",
        "gauge",
        "Currently active client connections.",
        snapshot.active_connections,
    );
    metric(
        &mut out,
        "nntp_stateful_sessions",
        "gauge",
        "Currently active stateful sessions.",
        snapshot.stateful_sessions,
    );
    metric(
        &mut out,
        "nntp_uptime_seconds",
        "gauge",
        "Proxy uptime in seconds.",
        snapshot.uptime.as_secs_f64(),
    );
    metric(
        &mut out,
        "nntp_client_to_backend_bytes_total",
        "counter",
        "Bytes forwarded from clients to backends.",
        snapshot.client_to_backend_bytes.as_u64(),
    );
    metric(
        &mut out,
        "nntp_backend_to_client_bytes_total",
        "counter",
        "Bytes forwarded from backends to clients.",
        snapshot.backend_to_client_bytes.as_u64(),
    );

    // ---- per-backend ----
    backend_family(
        &mut out,
        snapshot,
        server_names,
        "nntp_backend_commands_total",
        "counter",
        "Commands processed per backend.",
        |s| s.total_commands.get().to_string(),
    );
    backend_family(
        &mut out,
        snapshot,
        server_names,
        "nntp_backend_bytes_sent_total",
        "counter",
        "Bytes sent to the backend.",
        |s| s.bytes_sent.as_u64().to_string(),
    );
    backend_family(
        &mut out,
        snapshot,
        server_names,
        "nntp_backend_bytes_received_total",
        "counter",
        "Bytes received from the backend.",
        |s| s.bytes_received.as_u64().to_string(),
    );
    backend_family(
        &mut out,
        snapshot,
        server_names,
        "nntp_backend_errors_total",
        "counter",
        "Errors observed per backend.",
        |s| s.errors.get().to_string(),
    );
    backend_family(
        &mut out,
        snapshot,
        server_names,
        "nntp_backend_errors_4xx_total",
        "counter",
        "4xx responses per backend.",
        |s| s.errors_4xx.get().to_string(),
    );
    backend_family(
        &mut out,
        snapshot,
        server_names,
        "nntp_backend_errors_5xx_total",
        "counter",
        "5xx responses per backend.",
        |s| s.errors_5xx.get().to_string(),
    );
    backend_family(
        &mut out,
        snapshot,
        server_names,
        "nntp_backend_article_bytes_total",
        "counter",
        "Article bytes transferred per backend.",
        |s| s.article_bytes_total.get().to_string(),
    );
    backend_family(
        &mut out,
        snapshot,
        server_names,
        "nntp_backend_articles_total",
        "counter",
        "Articles transferred per backend.",
        |s| s.article_count.get().to_string(),
    );
    backend_family(
        &mut out,
        snapshot,
        server_names,
        "nntp_backend_connection_failures_total",
        "counter",
        "Backend connection failures.",
        |s| s.connection_failures.get().to_string(),
    );
    backend_family(
        &mut out,
        snapshot,
        server_names,
        "nntp_backend_active_connections",
        "gauge",
        "Currently active connections to the backend.",
        |s| s.active_connections.get().to_string(),
    );
    backend_family(
        &mut out,
        snapshot,
        server_names,
        "nntp_backend_health",
        "gauge",
        "Backend health (2=healthy, 1=degraded, 0=down).",
        |s| health_value(s.health_status).to_string(),
    );

    // Timing summaries (micros -> seconds). send/recv share the ttfb measurement count.
    backend_summary(
        &mut out,
        snapshot,
        server_names,
        "nntp_backend_ttfb_seconds",
        "Backend time-to-first-byte (seconds).",
        |s| s.ttfb_micros_total.get() as f64 / 1_000_000.0,
        |s| s.ttfb_count.get(),
    );
    backend_summary(
        &mut out,
        snapshot,
        server_names,
        "nntp_backend_send_seconds",
        "Backend send time (seconds); count shares the ttfb measurement count.",
        |s| s.send_micros_total.get() as f64 / 1_000_000.0,
        |s| s.ttfb_count.get(),
    );
    backend_summary(
        &mut out,
        snapshot,
        server_names,
        "nntp_backend_recv_seconds",
        "Backend receive time (seconds); count shares the ttfb measurement count.",
        |s| s.recv_micros_total.get() as f64 / 1_000_000.0,
        |s| s.ttfb_count.get(),
    );

    // ---- cache ----
    metric(
        &mut out,
        "nntp_cache_entries",
        "gauge",
        "Entries currently in the cache.",
        snapshot.cache_entries,
    );
    metric(
        &mut out,
        "nntp_cache_size_bytes",
        "gauge",
        "Approximate cache size in bytes.",
        snapshot.cache_size_bytes,
    );
    metric(
        &mut out,
        "nntp_cache_hit_rate",
        "gauge",
        "Cache hit rate as a ratio (0..1).",
        snapshot.cache_hit_rate / 100.0,
    );

    if let Some(disk) = snapshot.disk_cache.as_ref() {
        metric(
            &mut out,
            "nntp_cache_disk_hits_total",
            "counter",
            "Cache hits served from the disk tier.",
            disk.disk_hits,
        );
        metric(
            &mut out,
            "nntp_cache_disk_hit_rate",
            "gauge",
            "Disk hits as a ratio of total hits (0..1).",
            disk.disk_hit_rate / 100.0,
        );
        metric(
            &mut out,
            "nntp_cache_disk_capacity_bytes",
            "gauge",
            "Configured disk cache capacity in bytes.",
            disk.disk_capacity,
        );
        metric(
            &mut out,
            "nntp_cache_disk_bytes_written_total",
            "counter",
            "Bytes written to the disk tier.",
            disk.bytes_written,
        );
        metric(
            &mut out,
            "nntp_cache_disk_bytes_read_total",
            "counter",
            "Bytes read from the disk tier.",
            disk.bytes_read,
        );
        metric(
            &mut out,
            "nntp_cache_disk_write_ios_total",
            "counter",
            "Disk write I/O operations.",
            disk.write_ios,
        );
        metric(
            &mut out,
            "nntp_cache_disk_read_ios_total",
            "counter",
            "Disk read I/O operations.",
            disk.read_ios,
        );
    }

    // ---- pipeline ----
    metric(
        &mut out,
        "nntp_pipeline_batches_total",
        "counter",
        "Pipelined batches (more than one command).",
        snapshot.pipeline_batches,
    );
    metric(
        &mut out,
        "nntp_pipeline_commands_total",
        "counter",
        "Commands processed in pipelined batches.",
        snapshot.pipeline_commands,
    );
    metric(
        &mut out,
        "nntp_pipeline_requests_queued_total",
        "counter",
        "Requests enqueued to backend pipeline queues.",
        snapshot.pipeline_requests_queued,
    );
    metric(
        &mut out,
        "nntp_pipeline_requests_completed_total",
        "counter",
        "Requests completed via backend pipelines.",
        snapshot.pipeline_requests_completed,
    );

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::types::BackendHealthStatus;
    use crate::metrics::{
        BackendStats, CommandCount, DiskCacheStats, ErrorCount, MetricsSnapshot, RecvMicros,
        SendMicros, TtfbMicros,
    };
    use crate::types::{
        BackendId, BackendToClientBytes, BytesReceived, BytesSent, ClientToBackendBytes,
        TimingMeasurementCount,
    };

    fn fixture() -> MetricsSnapshot {
        let backend = BackendStats {
            backend_id: BackendId::from_index(0),
            total_commands: CommandCount::new(100),
            errors: ErrorCount::new(5),
            bytes_sent: BytesSent::new(1000),
            bytes_received: BytesReceived::new(2000),
            ttfb_micros_total: TtfbMicros::new(10_000), // 0.01 s
            ttfb_count: TimingMeasurementCount::new(10),
            send_micros_total: SendMicros::new(3000),
            recv_micros_total: RecvMicros::new(5000),
            health_status: BackendHealthStatus::Healthy,
            ..Default::default()
        };
        MetricsSnapshot {
            total_connections: 5,
            client_to_backend_bytes: ClientToBackendBytes::new(1500),
            backend_to_client_bytes: BackendToClientBytes::new(3500),
            backend_stats: vec![backend].into(),
            cache_hit_rate: 42.0,
            ..Default::default()
        }
    }

    #[test]
    fn renders_proxy_and_backend_families() {
        let out = render_prometheus(&fixture(), &["primary".to_string()]);
        assert!(out.contains("# TYPE nntp_connections_total counter"));
        assert!(out.contains("\nnntp_connections_total 5\n"));
        assert!(out.contains("nntp_backend_commands_total{backend=\"primary\"} 100"));
        assert!(out.contains("nntp_backend_errors_total{backend=\"primary\"} 5"));
        assert!(out.contains("nntp_backend_health{backend=\"primary\"} 2"));
    }

    #[test]
    fn ttfb_summary_converts_micros_to_seconds() {
        let out = render_prometheus(&fixture(), &["primary".to_string()]);
        assert!(out.contains("# TYPE nntp_backend_ttfb_seconds summary"));
        assert!(out.contains("nntp_backend_ttfb_seconds_sum{backend=\"primary\"} 0.01"));
        assert!(out.contains("nntp_backend_ttfb_seconds_count{backend=\"primary\"} 10"));
    }

    #[test]
    fn hit_rate_is_emitted_as_ratio() {
        let out = render_prometheus(&fixture(), &["primary".to_string()]);
        assert!(out.contains("nntp_cache_hit_rate 0.42"));
    }

    #[test]
    fn disk_metrics_absent_without_disk_cache() {
        let out = render_prometheus(&fixture(), &["primary".to_string()]);
        assert!(!out.contains("nntp_cache_disk_"));
    }

    #[test]
    fn disk_metrics_present_with_hybrid() {
        let mut snapshot = fixture();
        snapshot.disk_cache = Some(DiskCacheStats {
            disk_hits: 7,
            disk_hit_rate: 50.0,
            disk_capacity: 1024,
            bytes_written: 10,
            bytes_read: 20,
            write_ios: 2,
            read_ios: 4,
        });
        let out = render_prometheus(&snapshot, &["primary".to_string()]);
        assert!(out.contains("nntp_cache_disk_hits_total 7"));
        assert!(out.contains("nntp_cache_disk_hit_rate 0.5"));
    }

    #[test]
    fn backend_name_falls_back_to_index() {
        let out = render_prometheus(&fixture(), &[]);
        assert!(out.contains("nntp_backend_commands_total{backend=\"0\"} 100"));
    }

    #[test]
    fn label_values_are_escaped() {
        let out = render_prometheus(&fixture(), &["a\"b\\c".to_string()]);
        assert!(out.contains("backend=\"a\\\"b\\\\c\""));
    }

    #[test]
    fn empty_snapshot_renders_without_panic() {
        let out = render_prometheus(&MetricsSnapshot::default(), &[]);
        assert!(out.contains("nntp_connections_total 0"));
        assert!(out.contains("nntp_pipeline_batches_total 0"));
    }
}
