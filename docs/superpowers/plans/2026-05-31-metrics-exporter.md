# Prometheus `/metrics` Exporter Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a read-only Prometheus text `/metrics` endpoint to nntp-proxy so a local VictoriaMetrics `vmagent` can scrape proxy/per-backend/cache/pipeline counters.

**Architecture:** A pure `render_prometheus(&MetricsSnapshot, &[String]) -> String` formatter plus a minimal hand-rolled tokio HTTP responder (`parse_request_target` / `write_response` / `serve_connection`) in a new `src/metrics/exporter.rs`. `runtime::spawn_metrics_exporter` binds a `TcpListener` (loopback by default) and, per connection, snapshots metrics and serves the rendered text. Opt-in via a new `[metrics]` config section (default off). No per-user metrics.

**Tech Stack:** Rust 2024, tokio, serde/toml. **Zero new dependencies.**

**Conventions:** Match repo commit style (imperative, sentence case, no `feat:` prefix). End every commit message with the trailer:
`Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`
Reference spec: `docs/superpowers/specs/2026-05-31-metrics-exporter-design.md`.

**Pre-flight:** Work on branch `feat/metrics-exporter` (already created). A pre-existing unrelated edit in `src/cache/hybrid.rs` is present in the working tree — **do not stage or commit it**; stage only the files named in each task.

---

### Task 1: Add the `[metrics]` config section

**Files:**
- Modify: `src/config/types.rs` (add `Metrics` struct near `Proxy` ~line 210; add `metrics` field to `Config` ~line 138)
- Test: `src/config/types.rs` (inline `#[cfg(test)]` module)

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `src/config/types.rs`:

```rust
    #[test]
    fn metrics_section_defaults_when_absent() {
        let config: Config = toml::from_str("").unwrap();
        assert!(!config.metrics.enabled);
        assert_eq!(
            config.metrics.listen,
            std::net::SocketAddr::from(([127, 0, 0, 1], 9101))
        );
    }

    #[test]
    fn metrics_section_parses_overrides() {
        let toml = r#"
[metrics]
enabled = true
listen = "0.0.0.0:9999"
"#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(config.metrics.enabled);
        assert_eq!(config.metrics.listen, "0.0.0.0:9999".parse().unwrap());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p nntp-proxy --lib config::types::tests::metrics_section -- --nocapture`
Expected: FAIL to compile — `no field 'metrics' on type 'Config'`.

- [ ] **Step 3: Add the `Metrics` struct**

In `src/config/types.rs`, immediately after the `impl Default for Proxy { ... }` block (just before `/// Routing configuration`), add:

```rust
/// Prometheus `/metrics` exporter settings (`[metrics]` section).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Metrics {
    /// Enable the read-only Prometheus `/metrics` HTTP endpoint (default: false).
    pub enabled: bool,
    /// Address the exporter binds (default: 127.0.0.1:9101, loopback-only).
    pub listen: std::net::SocketAddr,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: std::net::SocketAddr::from(([127, 0, 0, 1], 9101)),
        }
    }
}
```

- [ ] **Step 4: Add the `metrics` field to `Config`**

In the `pub struct Config { ... }` definition, add this field immediately after the `pub proxy: Proxy,` field (around line 119):

```rust
    /// Prometheus `/metrics` exporter settings
    #[serde(default)]
    pub metrics: Metrics,
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p nntp-proxy --lib config::types::tests::metrics_section -- --nocapture`
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
git add src/config/types.rs
git commit
```
Message (with the Co-Authored-By trailer):
```
Add [metrics] config section for the exporter

New opt-in [metrics] table (enabled=false, listen=127.0.0.1:9101) that
gates the upcoming Prometheus /metrics endpoint.
```

---

### Task 2: Create the exporter module and pure renderer

**Files:**
- Create: `src/metrics/exporter.rs`
- Modify: `src/metrics/mod.rs` (add `pub mod exporter;` after `mod user_stats;`, ~line 38)
- Test: `src/metrics/exporter.rs` (inline `#[cfg(test)]`)

- [ ] **Step 1: Declare the module**

In `src/metrics/mod.rs`, in the block of `mod` declarations (after `mod user_stats;`), add:

```rust
pub mod exporter;
```

- [ ] **Step 2: Write the exporter file with the renderer and its failing tests**

Create `src/metrics/exporter.rs` with exactly this content:

```rust
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
    metric(&mut out, "nntp_connections_total", "counter",
        "Total client connections accepted.", snapshot.total_connections);
    metric(&mut out, "nntp_active_connections", "gauge",
        "Currently active client connections.", snapshot.active_connections);
    metric(&mut out, "nntp_stateful_sessions", "gauge",
        "Currently active stateful sessions.", snapshot.stateful_sessions);
    metric(&mut out, "nntp_uptime_seconds", "gauge",
        "Proxy uptime in seconds.", snapshot.uptime.as_secs_f64());
    metric(&mut out, "nntp_client_to_backend_bytes_total", "counter",
        "Bytes forwarded from clients to backends.", snapshot.client_to_backend_bytes.as_u64());
    metric(&mut out, "nntp_backend_to_client_bytes_total", "counter",
        "Bytes forwarded from backends to clients.", snapshot.backend_to_client_bytes.as_u64());

    // ---- per-backend ----
    backend_family(&mut out, snapshot, server_names, "nntp_backend_commands_total", "counter",
        "Commands processed per backend.", |s| s.total_commands.get().to_string());
    backend_family(&mut out, snapshot, server_names, "nntp_backend_bytes_sent_total", "counter",
        "Bytes sent to the backend.", |s| s.bytes_sent.as_u64().to_string());
    backend_family(&mut out, snapshot, server_names, "nntp_backend_bytes_received_total", "counter",
        "Bytes received from the backend.", |s| s.bytes_received.as_u64().to_string());
    backend_family(&mut out, snapshot, server_names, "nntp_backend_errors_total", "counter",
        "Errors observed per backend.", |s| s.errors.get().to_string());
    backend_family(&mut out, snapshot, server_names, "nntp_backend_errors_4xx_total", "counter",
        "4xx responses per backend.", |s| s.errors_4xx.get().to_string());
    backend_family(&mut out, snapshot, server_names, "nntp_backend_errors_5xx_total", "counter",
        "5xx responses per backend.", |s| s.errors_5xx.get().to_string());
    backend_family(&mut out, snapshot, server_names, "nntp_backend_article_bytes_total", "counter",
        "Article bytes transferred per backend.", |s| s.article_bytes_total.get().to_string());
    backend_family(&mut out, snapshot, server_names, "nntp_backend_articles_total", "counter",
        "Articles transferred per backend.", |s| s.article_count.get().to_string());
    backend_family(&mut out, snapshot, server_names, "nntp_backend_connection_failures_total", "counter",
        "Backend connection failures.", |s| s.connection_failures.get().to_string());
    backend_family(&mut out, snapshot, server_names, "nntp_backend_active_connections", "gauge",
        "Currently active connections to the backend.", |s| s.active_connections.get().to_string());
    backend_family(&mut out, snapshot, server_names, "nntp_backend_health", "gauge",
        "Backend health (2=healthy, 1=degraded, 0=down).", |s| health_value(s.health_status).to_string());

    // Timing summaries (micros -> seconds). send/recv share the ttfb measurement count.
    backend_summary(&mut out, snapshot, server_names, "nntp_backend_ttfb_seconds",
        "Backend time-to-first-byte (seconds).",
        |s| s.ttfb_micros_total.get() as f64 / 1_000_000.0, |s| s.ttfb_count.get());
    backend_summary(&mut out, snapshot, server_names, "nntp_backend_send_seconds",
        "Backend send time (seconds); count shares the ttfb measurement count.",
        |s| s.send_micros_total.get() as f64 / 1_000_000.0, |s| s.ttfb_count.get());
    backend_summary(&mut out, snapshot, server_names, "nntp_backend_recv_seconds",
        "Backend receive time (seconds); count shares the ttfb measurement count.",
        |s| s.recv_micros_total.get() as f64 / 1_000_000.0, |s| s.ttfb_count.get());

    // ---- cache ----
    metric(&mut out, "nntp_cache_entries", "gauge",
        "Entries currently in the cache.", snapshot.cache_entries);
    metric(&mut out, "nntp_cache_size_bytes", "gauge",
        "Approximate cache size in bytes.", snapshot.cache_size_bytes);
    metric(&mut out, "nntp_cache_hit_rate", "gauge",
        "Cache hit rate as a ratio (0..1).", snapshot.cache_hit_rate / 100.0);

    if let Some(disk) = snapshot.disk_cache.as_ref() {
        metric(&mut out, "nntp_cache_disk_hits_total", "counter",
            "Cache hits served from the disk tier.", disk.disk_hits);
        metric(&mut out, "nntp_cache_disk_hit_rate", "gauge",
            "Disk hits as a ratio of total hits (0..1).", disk.disk_hit_rate / 100.0);
        metric(&mut out, "nntp_cache_disk_capacity_bytes", "gauge",
            "Configured disk cache capacity in bytes.", disk.disk_capacity);
        metric(&mut out, "nntp_cache_disk_bytes_written_total", "counter",
            "Bytes written to the disk tier.", disk.bytes_written);
        metric(&mut out, "nntp_cache_disk_bytes_read_total", "counter",
            "Bytes read from the disk tier.", disk.bytes_read);
        metric(&mut out, "nntp_cache_disk_write_ios_total", "counter",
            "Disk write I/O operations.", disk.write_ios);
        metric(&mut out, "nntp_cache_disk_read_ios_total", "counter",
            "Disk read I/O operations.", disk.read_ios);
    }

    // ---- pipeline ----
    metric(&mut out, "nntp_pipeline_batches_total", "counter",
        "Pipelined batches (more than one command).", snapshot.pipeline_batches);
    metric(&mut out, "nntp_pipeline_commands_total", "counter",
        "Commands processed in pipelined batches.", snapshot.pipeline_commands);
    metric(&mut out, "nntp_pipeline_requests_queued_total", "counter",
        "Requests enqueued to backend pipeline queues.", snapshot.pipeline_requests_queued);
    metric(&mut out, "nntp_pipeline_requests_completed_total", "counter",
        "Requests completed via backend pipelines.", snapshot.pipeline_requests_completed);

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
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p nntp-proxy --lib metrics::exporter -- --nocapture`
Expected: PASS (8 tests). If a `crate::types::` import path is wrong, fix the import — the field types are confirmed in `src/metrics/snapshot.rs` and `src/metrics/backend_stats.rs`.

- [ ] **Step 4: Lint and format the new file**

Run: `cargo clippy -p nntp-proxy --lib 2>&1 | tail -20`
Expected: no warnings for `metrics::exporter`.
Run: `cargo fmt`

- [ ] **Step 5: Commit**

```bash
git add src/metrics/exporter.rs src/metrics/mod.rs
git commit
```
Message:
```
Add pure Prometheus renderer for metrics snapshots

render_prometheus() turns a MetricsSnapshot into Prometheus 0.0.4 text:
proxy/per-backend/cache/pipeline families, timing summaries (micros->seconds),
hit-rate as a 0..1 ratio, disk metrics gated on hybrid cache. No per-user series.
```

---

### Task 3: HTTP request-line parsing

**Files:**
- Modify: `src/metrics/exporter.rs` (add `RequestTarget` + `parse_request_target` above the `#[cfg(test)]` module; add tests inside it)

- [ ] **Step 1: Write the failing tests**

Add these tests inside the existing `mod tests` block in `src/metrics/exporter.rs`:

```rust
    #[test]
    fn parses_get_metrics() {
        assert_eq!(parse_request_target(b"GET /metrics HTTP/1.1"), RequestTarget::Metrics);
        assert_eq!(parse_request_target(b"GET /metrics?x=1 HTTP/1.1"), RequestTarget::Metrics);
    }

    #[test]
    fn unknown_path_or_method_is_not_found() {
        assert_eq!(parse_request_target(b"GET /healthz HTTP/1.1"), RequestTarget::NotFound);
        assert_eq!(parse_request_target(b"POST /metrics HTTP/1.1"), RequestTarget::NotFound);
    }

    #[test]
    fn malformed_request_line_is_bad_request() {
        assert_eq!(parse_request_target(b"GET"), RequestTarget::BadRequest);
        assert_eq!(parse_request_target(b""), RequestTarget::BadRequest);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p nntp-proxy --lib metrics::exporter::tests::parses_get_metrics`
Expected: FAIL to compile — `cannot find type 'RequestTarget'` / `parse_request_target` not found.

- [ ] **Step 3: Implement the parser**

In `src/metrics/exporter.rs`, immediately above the `#[cfg(test)]` module, add:

```rust
/// Routing outcome for an HTTP request line.
#[derive(Debug, PartialEq, Eq)]
enum RequestTarget {
    Metrics,
    NotFound,
    BadRequest,
}

/// Parse the first line of an HTTP request (`METHOD SP TARGET SP VERSION`).
fn parse_request_target(request_line: &[u8]) -> RequestTarget {
    let Ok(text) = std::str::from_utf8(request_line) else {
        return RequestTarget::BadRequest;
    };
    let line = text.trim_end_matches(['\r', '\n']);
    let mut parts = line.split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
        return RequestTarget::BadRequest;
    };
    if method.is_empty() || target.is_empty() {
        return RequestTarget::BadRequest;
    }
    let path = target.split('?').next().unwrap_or(target);
    if method == "GET" && path == "/metrics" {
        RequestTarget::Metrics
    } else {
        RequestTarget::NotFound
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p nntp-proxy --lib metrics::exporter::tests::`
Expected: PASS (11 tests total now).

- [ ] **Step 5: Commit**

```bash
git add src/metrics/exporter.rs
git commit
```
Message:
```
Add HTTP request-line parsing for the metrics endpoint

parse_request_target() classifies the request line into Metrics / NotFound /
BadRequest; only GET /metrics (optionally with a query string) routes to Metrics.
```

---

### Task 4: Response writing and `serve_connection` (with socket tests)

**Files:**
- Modify: `src/metrics/exporter.rs` (add tokio I/O imports, `write_response`, `serve_connection`; add async tests)

- [ ] **Step 1: Write the failing async tests**

Add these tests inside the `mod tests` block in `src/metrics/exporter.rs`:

```rust
    #[tokio::test]
    async fn serve_connection_returns_metrics_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut client, mut server) = tokio::io::duplex(1024);
        let task = tokio::spawn(async move {
            serve_connection(&mut server, || "nntp_demo 1\n".to_string())
                .await
                .unwrap();
        });
        client
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        let resp = String::from_utf8(resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "got: {resp}");
        assert!(resp.contains("Content-Type: text/plain; version=0.0.4"));
        assert!(resp.contains("\r\n\r\nnntp_demo 1\n"));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn serve_connection_404_does_not_render() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut client, mut server) = tokio::io::duplex(1024);
        let task = tokio::spawn(async move {
            serve_connection(&mut server, || unreachable!("render must not run for 404"))
                .await
                .unwrap();
        });
        client.write_all(b"GET /nope HTTP/1.1\r\n\r\n").await.unwrap();
        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        let resp = String::from_utf8(resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 404 Not Found"), "got: {resp}");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn serve_connection_over_real_tcp() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            serve_connection(&mut sock, || "nntp_tcp 1\n".to_string())
                .await
                .unwrap();
        });
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut resp = Vec::new();
        client.read_to_end(&mut resp).await.unwrap();
        let resp = String::from_utf8(resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"), "got: {resp}");
        assert!(resp.contains("nntp_tcp 1"));
        server.await.unwrap();
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p nntp-proxy --lib metrics::exporter::tests::serve_connection`
Expected: FAIL to compile — `serve_connection` not found.

- [ ] **Step 3: Add I/O imports and implement the responder**

In `src/metrics/exporter.rs`, add to the top-level imports (after `use std::fmt::Write as _;`):

```rust
use tokio::io::{AsyncReadExt, AsyncWriteExt};
```

Add these constants next to the existing module items (e.g. just below the `use` block):

```rust
/// Prometheus 0.0.4 text exposition content type.
const METRICS_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";
/// Upper bound on bytes read while scanning for the request line (loopback-only endpoint).
const MAX_REQUEST_BYTES: usize = 4096;
```

Add these functions above the `#[cfg(test)]` module (below `parse_request_target`):

```rust
/// Write a minimal HTTP/1.1 response and close the connection.
async fn write_response<W>(
    writer: &mut W,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let header = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    writer.write_all(header.as_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

/// Serve one `GET /metrics` request over `stream`.
///
/// Reads up to the first newline (bounded by [`MAX_REQUEST_BYTES`]) to obtain the
/// request line, then renders and writes the response. `render_body` is only
/// invoked for a matched `GET /metrics` so non-metrics requests never snapshot.
pub(crate) async fn serve_connection<S, F>(stream: &mut S, render_body: F) -> std::io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    F: FnOnce() -> String,
{
    let mut buf = [0u8; MAX_REQUEST_BYTES];
    let mut filled = 0usize;
    loop {
        let n = stream.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
        if buf[..filled].iter().any(|&b| b == b'\n') || filled == buf.len() {
            break;
        }
    }
    let first_line = buf[..filled].split(|&b| b == b'\n').next().unwrap_or(&[]);

    match parse_request_target(first_line) {
        RequestTarget::Metrics => {
            let body = render_body();
            write_response(stream, "200 OK", METRICS_CONTENT_TYPE, body.as_bytes()).await
        }
        RequestTarget::NotFound => {
            write_response(stream, "404 Not Found", "text/plain; charset=utf-8", b"not found\n").await
        }
        RequestTarget::BadRequest => {
            write_response(stream, "400 Bad Request", "text/plain; charset=utf-8", b"bad request\n").await
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p nntp-proxy --lib metrics::exporter`
Expected: PASS (14 tests total).

- [ ] **Step 5: Lint and format**

Run: `cargo clippy -p nntp-proxy --lib 2>&1 | tail -20` (expect clean) then `cargo fmt`.

- [ ] **Step 6: Commit**

```bash
git add src/metrics/exporter.rs
git commit
```
Message:
```
Add minimal GET /metrics responder over async streams

serve_connection() does a bounded request-line read, routes via
parse_request_target, and writes a Connection: close response; render_body runs
only for a matched GET /metrics. Covered by duplex and real-TCP tests.
```

---

### Task 5: Spawn the exporter in the runtime

**Files:**
- Modify: `src/runtime.rs` (add `spawn_metrics_exporter` after `spawn_metrics_saver`, ~line 991)

- [ ] **Step 1: Implement `spawn_metrics_exporter`**

In `src/runtime.rs`, immediately after the closing `}` of `spawn_metrics_saver` (around line 991), add:

```rust
/// Spawn the read-only Prometheus `/metrics` exporter.
///
/// Binds `listen` and answers `GET /metrics` with the current metrics snapshot
/// in Prometheus text format. Intended for a local scraper (e.g. vmagent) on a
/// loopback address; gated by the `[metrics]` config section (the caller only
/// invokes this when enabled). Bind failures are logged and the task exits — the
/// proxy keeps running without the exporter.
pub fn spawn_metrics_exporter(
    proxy: &std::sync::Arc<crate::NntpProxy>,
    listen: std::net::SocketAddr,
    server_names: Vec<String>,
) {
    use std::sync::Arc;

    let proxy = Arc::clone(proxy);
    tokio::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(listen).await {
            Ok(listener) => listener,
            Err(e) => {
                tracing::warn!("Metrics exporter failed to bind {listen}: {e}");
                return;
            }
        };
        tracing::info!("Metrics exporter listening on http://{listen}/metrics");
        loop {
            let socket = match listener.accept().await {
                Ok((socket, _peer)) => socket,
                Err(e) => {
                    tracing::warn!("Metrics exporter accept error: {e}");
                    continue;
                }
            };
            let proxy = Arc::clone(&proxy);
            let names = server_names.clone();
            tokio::spawn(async move {
                let mut socket = socket;
                let result = crate::metrics::exporter::serve_connection(&mut socket, || {
                    let snapshot = proxy
                        .metrics()
                        .snapshot(Some(&**proxy.cache()))
                        .with_pool_status(&**proxy.router());
                    crate::metrics::exporter::render_prometheus(&snapshot, &names)
                })
                .await;
                if let Err(e) = result {
                    tracing::debug!("Metrics exporter connection error: {e}");
                }
            });
        }
    });
}
```

- [ ] **Step 2: Verify it compiles**

Run: `cargo build -p nntp-proxy 2>&1 | tail -20`
Expected: builds. If `with_pool_status`/`snapshot` argument coercions error, the fixes are `&**proxy.router()` and `Some(&**proxy.cache())` (already used above) — `proxy.router()` returns `&Arc<BackendSelector>` and `proxy.cache()` returns `&Arc<UnifiedCache>`.

- [ ] **Step 3: Lint**

Run: `cargo clippy -p nntp-proxy 2>&1 | tail -20`
Expected: clean.

- [ ] **Step 4: Commit**

```bash
git add src/runtime.rs
git commit
```
Message:
```
Spawn the Prometheus /metrics exporter task

spawn_metrics_exporter() binds the configured address and serves each scrape
from collector.snapshot(cache).with_pool_status(router). Bind failures are
logged and non-fatal.
```

---

### Task 6: Wire config through the binary

**Files:**
- Modify: `src/bin/nntp-proxy.rs` (`ProxyLaunch` ~line 223; `prepare_proxy_launch` ~line 245/254; `main` ~line 99; the `prepare_proxy_launch` test ~line 415)

- [ ] **Step 1: Extend `ProxyLaunch`**

In `src/bin/nntp-proxy.rs`, add two fields to `struct ProxyLaunch` after `metrics_store: Option<MetricsStore>,`:

```rust
    metrics_enabled: bool,
    metrics_listen: std::net::SocketAddr,
```

- [ ] **Step 2: Populate them in `prepare_proxy_launch`**

In `prepare_proxy_launch`, after the `let metrics_store = ...;` line, add:

```rust
    let metrics_enabled = config.metrics.enabled;
    let metrics_listen = config.metrics.listen;
```

Then add both to the returned `ProxyLaunch { ... }` literal (after `metrics_store,`):

```rust
        metrics_enabled,
        metrics_listen,
```

- [ ] **Step 3: Call the spawner in `main`**

In `src/bin/nntp-proxy.rs`, immediately after the `runtime::spawn_metrics_saver(...);` call (the block ending at ~line 99) and before `runtime::spawn_availability_saver(...)`, add:

```rust
    if launch.metrics_enabled {
        runtime::spawn_metrics_exporter(
            &proxy,
            launch.metrics_listen,
            launch.server_names.clone(),
        );
    }
```

- [ ] **Step 4: Extend the existing `prepare_proxy_launch` test**

Find the test around line 410 that asserts `launch.server_names` and add, after that assertion:

```rust
        assert!(!launch.metrics_enabled);
        assert_eq!(
            launch.metrics_listen,
            "127.0.0.1:9101".parse().unwrap()
        );
```

- [ ] **Step 5: Run the test and build**

Run: `cargo test -p nntp-proxy --bin nntp-proxy prepare_proxy_launch -- --nocapture`
Expected: PASS.
Run: `cargo build -p nntp-proxy 2>&1 | tail -5`
Expected: builds clean.

- [ ] **Step 6: Commit**

```bash
git add src/bin/nntp-proxy.rs
git commit
```
Message:
```
Wire [metrics] config into the binary launch

prepare_proxy_launch reads config.metrics; main spawns the exporter only when
[metrics].enabled. Defaults remain off (loopback 127.0.0.1:9101).
```

---

### Task 7: Document the `[metrics]` section in sample config

**Files:**
- Modify: `config.example.toml`, `config.full.toml`

- [ ] **Step 1: Append the section to both files**

Append this block to the end of `config.example.toml` AND `config.full.toml`:

```toml

# Prometheus /metrics exporter (read-only). Disabled by default.
# When enabled, a local scraper (e.g. VictoriaMetrics vmagent) can pull
# proxy/per-backend/cache/pipeline metrics. Bind to loopback unless the
# scraper runs on another host.
[metrics]
enabled = false
listen = "127.0.0.1:9101"
```

- [ ] **Step 2: Verify the samples still parse**

Run: `cargo run -q -p nntp-proxy --bin nntp-proxy -- --config config.full.toml --check 2>&1 | tail -5` if a `--check`/validate flag exists; otherwise confirm via a quick test:
Run: `cargo test -p nntp-proxy --lib config:: 2>&1 | tail -5`
Expected: config tests pass (the sample files are valid TOML with the new optional section).

- [ ] **Step 3: Commit**

```bash
git add config.example.toml config.full.toml
git commit
```
Message:
```
Document the [metrics] exporter section in sample config
```

---

### Task 8: Full verification

- [ ] **Step 1: Run the whole test suite**

Run: `cargo nextest run -p nntp-proxy 2>&1 | tail -25`
Expected: all pass (note: foyer disk-cache tests are `#[ignore]` per CLAUDE.md).

- [ ] **Step 2: Clippy and fmt gates**

Run: `cargo clippy --all-targets -p nntp-proxy 2>&1 | tail -20` (expect clean)
Run: `cargo fmt --check` (expect no diff)

- [ ] **Step 3: Manual smoke test**

Run the proxy with a config that sets `[metrics] enabled = true`, then:
Run: `curl -s http://127.0.0.1:9101/metrics | head -40`
Expected: Prometheus text starting with `# HELP nntp_connections_total ...`. Also:
Run: `curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:9101/healthz`
Expected: `404`.

- [ ] **Step 4: Confirm the unrelated `hybrid.rs` change was never committed**

Run: `git log --oneline main..HEAD` (review the task commits) and `git status --short`
Expected: `src/cache/hybrid.rs` still shows as modified/unstaged (it was pre-existing and is not part of this work).

---

## Out of scope (tracked separately)

The VictoriaMetrics/VictoriaLogs **deployment bundle** (vmagent scrape config for `127.0.0.1:9101`, `systemd-journal-upload` drop-in, Vector config for nzbdav logs, the two VictoriaMetrics MCP servers + `claude mcp add` commands, runbook) is the follow-on deliverable described in the spec §10. It is not part of this plan.
