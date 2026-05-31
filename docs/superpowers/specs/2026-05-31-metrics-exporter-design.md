# nntp-proxy Prometheus `/metrics` Exporter — Design

> **Status:** Approved (design phase) — 2026-05-31
> **Scope:** Add a read-only Prometheus text exporter to nntp-proxy so an external
> VictoriaMetrics (Prometheus-compatible) scraper can pull proxy metrics.
> **Out of scope (follow-on):** the VictoriaMetrics/VictoriaLogs deployment bundle
> (see "Follow-on" at the end).

---

## 1. Motivation

nntp-proxy currently exposes metrics only via (a) `stats.json` (periodic + on-shutdown
JSON snapshot), (b) a loopback WebSocket TUI dashboard (binary postcard), and (c)
`tracing` logs to stdout. None of these is scrapeable by a standard time-series
database. The goal is to make the proxy's rich per-backend / cache / pipeline counters
queryable by an LLM (Claude Code) and by Grafana-style tools **without SSH**, by adding
a Prometheus-format `/metrics` endpoint that VictoriaMetrics' `vmagent` can scrape.

The logs side already works (journald captures stdout), so this spec covers **metrics
only**.

## 2. Decisions (locked during brainstorming)

| Decision | Choice | Rationale |
|---|---|---|
| Backend store | VictoriaMetrics + VictoriaLogs | Lightest option; official read-only MCP servers w/ embedded query-language docs; VM is Prometheus-API-compatible. |
| Metrics bridge | Native `/metrics` exporter inside nntp-proxy | Real-time; exposes live gauges and the full per-backend/cache/pipeline surface via the existing `collector.snapshot()`. |
| Per-user metrics | **Excluded** from `/metrics` | Avoids putting usernames into the TSDB and avoids per-user cardinality/churn. Per-user data remains in the TUI and `stats.json`. |
| Topology | VM/VL + `vmagent` run on the **same host** as the proxy | `/metrics` binds `127.0.0.1` and is never exposed off-box. No auth needed. |
| HTTP layer | **Hand-rolled tokio responder** (zero new deps) | Matches the codebase's minimal-dep, tokio-native, hand-written-protocol style. The route surface is one read-only GET. |

## 3. Architecture

One new module: `src/metrics/exporter.rs`, split into a **pure renderer** and a **thin
tokio HTTP shell** so the formatting logic is fully unit-testable without I/O.

```
runtime.rs
  └─ spawn_metrics_exporter(collector, router, addr)   // only when [metrics].enabled
        │  (tokio task, sibling of spawn_metrics_saver)
        ▼
  TcpListener.accept() loop
        │  per connection:
        ▼
  read request line ──> match "GET /metrics"
        │  yes                         │ no
        ▼                              ▼
  snapshot = collector.snapshot()    write "404 Not Found"
            .with_pool_status(&router)
        │
        ▼
  body = render_prometheus(&snapshot, &backend_names)   // PURE, tested
        │
        ▼
  write "200 OK" + headers + body, then close
```

### 3.1 Units and responsibilities

- **`render_prometheus(snapshot: &MetricsSnapshot, names: &[ServerName]) -> String`**
  - Pure function. No I/O, no clock, no allocation beyond the output `String`.
  - Input: an immutable snapshot + a slice mapping `BackendId` index → backend display
    name (for the `backend="…"` label). Out-of-range / missing names fall back to the
    numeric id (`backend="0"`).
  - Output: a complete Prometheus exposition-format document (`# HELP`, `# TYPE`,
    samples), `text/plain; version=0.0.4`.
  - This is where ~all the logic and tests live.

- **`spawn_metrics_exporter(...)`** — thin shell.
  - Binds a `tokio::net::TcpListener` on the configured address.
  - Accept loop; each accepted socket is handled on its own spawned task. Reads the HTTP
    request line (and drains headers up to `\r\n\r\n`), matches
    method+path, calls `snapshot()` + `render_prometheus`, writes the response with
    `Connection: close`, flushes, drops the socket.
  - Only `GET /metrics` returns 200; anything else returns 404. Malformed/oversized
    request lines are answered `400` and closed (bounded read to avoid unbounded
    buffering).
  - The proxy data path is **untouched** — scrape work happens only on this task when a
    scrape arrives.

### 3.2 Where backend names come from

`BackendStats` carries `backend_id` but not the server name. The exporter resolves names
the same way the TUI/`with_pool_status` does — via the runtime's `BackendSelector`
(`router.backend_provider(id)` / the configured server list). The spawn function captures
an owned `Vec<ServerName>` (or `Arc<[ServerName]>`) indexed by backend id so the renderer
stays pure and free of router types.

## 4. Metric set

Prefix `nntp_`. **No per-user series.** Counters end in `_total`. Timings are exposed as
Prometheus **summary-style** `_sum`/`_count` pairs (no quantiles) so
`rate(_sum[5m]) / rate(_count[5m])` yields a live average.

### 4.1 Proxy-level

| Metric | Type | Source field |
|---|---|---|
| `nntp_connections_total` | counter | `total_connections` |
| `nntp_active_connections` | gauge | `active_connections` |
| `nntp_stateful_sessions` | gauge | `stateful_sessions` |
| `nntp_uptime_seconds` | gauge | `uptime` |
| `nntp_client_to_backend_bytes_total` | counter | `client_to_backend_bytes` |
| `nntp_backend_to_client_bytes_total` | counter | `backend_to_client_bytes` |

### 4.2 Per-backend — label `{backend="<name>"}`

| Metric | Type | Source field |
|---|---|---|
| `nntp_backend_commands_total` | counter | `total_commands` |
| `nntp_backend_bytes_sent_total` | counter | `bytes_sent` |
| `nntp_backend_bytes_received_total` | counter | `bytes_received` |
| `nntp_backend_errors_total` | counter | `errors` |
| `nntp_backend_errors_4xx_total` | counter | `errors_4xx` |
| `nntp_backend_errors_5xx_total` | counter | `errors_5xx` |
| `nntp_backend_article_bytes_total` | counter | `article_bytes_total` |
| `nntp_backend_articles_total` | counter | `article_count` |
| `nntp_backend_connection_failures_total` | counter | `connection_failures` |
| `nntp_backend_active_connections` | gauge | `active_connections` (via `with_pool_status`) |
| `nntp_backend_health` | gauge | `health_status` → `2`=Healthy, `1`=Degraded, `0`=Down |
| `nntp_backend_ttfb_seconds_sum` / `_count` | summary | `ttfb_micros_total`/1e6, `ttfb_count` |
| `nntp_backend_send_seconds_sum` / `_count` | summary | `send_micros_total`/1e6, `ttfb_count`¹ |
| `nntp_backend_recv_seconds_sum` / `_count` | summary | `recv_micros_total`/1e6, `ttfb_count`¹ |

¹ The codebase computes send/recv averages against `ttfb_count` (there is no separate
send/recv measurement count). The exporter mirrors this: the three `_count` series share
the same `ttfb_count` value. This is documented in each metric's `# HELP`.

### 4.3 Cache

| Metric | Type | Source field |
|---|---|---|
| `nntp_cache_entries` | gauge | `cache_entries` |
| `nntp_cache_size_bytes` | gauge | `cache_size_bytes` |
| `nntp_cache_hit_rate` | gauge | `cache_hit_rate` (emitted as ratio `0..1` — see §7) |

Disk tier — **emitted only when `snapshot.disk_cache.is_some()`** (hybrid cache mode):

| Metric | Type | Source field |
|---|---|---|
| `nntp_cache_disk_hits_total` | counter | `disk_hits` |
| `nntp_cache_disk_hit_rate` | gauge | `disk_hit_rate` (ratio `0..1` — see §7) |
| `nntp_cache_disk_capacity_bytes` | gauge | `disk_capacity` |
| `nntp_cache_disk_bytes_written_total` | counter | `bytes_written` |
| `nntp_cache_disk_bytes_read_total` | counter | `bytes_read` |
| `nntp_cache_disk_write_ios_total` | counter | `write_ios` |
| `nntp_cache_disk_read_ios_total` | counter | `read_ios` |

### 4.4 Pipeline

| Metric | Type | Source field |
|---|---|---|
| `nntp_pipeline_batches_total` | counter | `pipeline_batches` |
| `nntp_pipeline_commands_total` | counter | `pipeline_commands` |
| `nntp_pipeline_requests_queued_total` | counter | `pipeline_requests_queued` |
| `nntp_pipeline_requests_completed_total` | counter | `pipeline_requests_completed` |

## 5. Prometheus text format details

- Each metric family emits one `# HELP <name> <text>` and one `# TYPE <name>
  <counter|gauge|summary>` line, then its sample line(s).
- Label values are escaped per the exposition format: `\` → `\\`, `"` → `\"`, newline →
  `\n`. Backend names are validated newtypes (unlikely to contain these), but the renderer
  escapes unconditionally for safety.
- Integers print without a decimal point; rates/seconds print as floats. `NaN`/inf are
  never emitted — rate fields default to `0` when their denominator is zero (already the
  case in the snapshot helpers).
- Response headers: `200 OK`, `Content-Type: text/plain; version=0.0.4; charset=utf-8`,
  `Content-Length`, `Connection: close`.

## 6. Configuration

New `[metrics]` section. **Opt-in (default disabled)** so existing deployments are
unchanged and nothing new binds a port unless asked.

```toml
[metrics]
enabled = false               # default; set true on the server host
listen  = "127.0.0.1:9101"    # default; loopback-only for the same-host topology
```

- When `enabled = false` (default), `spawn_metrics_exporter` is never called.
- `listen` is parsed to a `SocketAddr`; default `127.0.0.1:9101`.
- Wired in `runtime.rs` next to `spawn_metrics_saver`; the task is added to the same
  shutdown/lifecycle handling as the other background tasks.

## 7. Implementation notes to verify during build

- **Cache hit-rate unit:** `cache_hit_rate` and `disk_hit_rate` are `f64`. Confirm against
  `collector.rs` whether they are a fraction (`0..1`) or a percentage (`0..100`). The
  renderer must emit a **ratio `0..1`**; if the snapshot stores a percentage, divide by
  100 in the renderer. Pin this with a unit test once confirmed.
- **Cache fields populated?** `cache_entries` / `cache_size_bytes` / `cache_hit_rate` are
  `#[serde(skip)]` and filled at snapshot time. Confirm `snapshot()` populates them in the
  proxy's run path; if they are only filled on a specific code path, gate their emission
  the same way `disk_cache` is gated (`Option`/presence check) rather than emitting `0`.
- **Exact accessors:** use the existing typed getters (`.get()`, `.as_u64()`) — do not add
  parallel accessors.

## 8. Testing strategy (TDD, per CLAUDE.md)

Write tests first.

**Unit (pure renderer — the bulk):**
- Exact-output assertions against a fixture `MetricsSnapshot` (reuse the
  `create_test_snapshot()` shape from `snapshot.rs` tests): metric names, `# TYPE` lines,
  `backend="…"` labels, micros→seconds conversion, summary `_sum`/`_count` pairing.
- Label-value escaping (`"`, `\`, newline in a synthetic backend name).
- Disk-cache **absent** → no `nntp_cache_disk_*` lines; **present** → all seven lines.
- Empty snapshot (`MetricsSnapshot::default()`) → valid document, zero values, no panics.
- Backend-name fallback to numeric id when `names` is shorter than `backend_stats`.

**Integration (thin shell):**
- Bind the exporter on an ephemeral port (`127.0.0.1:0`), then over a raw TCP/HTTP GET:
  - `GET /metrics` → `200`, `Content-Type` correct, body contains a known metric line.
  - `GET /healthz` (or any other path) → `404`.
  - Malformed request line → `400`, connection closed, server still accepts the next.

**Non-goals for tests:** no scraping of a real VictoriaMetrics instance (that's the
deployment bundle); no per-user assertions (per-user is excluded by design).

## 9. Performance / safety

- Zero impact on the NNTP data path: the exporter is an independent task that only does
  work when a scrape connection arrives (every ~10–15s from a local `vmagent`).
- Bounded reads on the request line/headers to avoid unbounded buffering from a
  misbehaving client; loopback-only bind limits exposure.
- No `unsafe`. The exporter does **not** use `BufferPool`: it is not a hot path and
  renders into a plain `String`, so the pooled-buffer rules in CLAUDE.md (§I/O Buffer
  Management) do not apply. Multiline-framing rules (§Pattern 1) are also irrelevant —
  the exporter does no NNTP multiline parsing.

## 10. Follow-on (separate deliverable, not this spec)

After the exporter lands, a **deployment bundle** (config + runbook, no app code) for the
single `server` host:
- VictoriaMetrics (`:8428`) and VictoriaLogs (`:9428`), e.g. as systemd services or
  containers.
- `vmagent` scrape config targeting `127.0.0.1:9101` (the new endpoint).
- `systemd-journal-upload` drop-in pointing the nntp-proxy unit's journal at
  `http://127.0.0.1:9428/insert/journald`.
- A Vector (or Fluent Bit) config shipping the nzbdav container logs to VL.
- The `VictoriaMetrics/mcp-victoriametrics` + `VictoriaMetrics/mcp-victorialogs` MCP
  servers and the `claude mcp add …` commands so Claude Code can query metrics + logs
  without SSH.

## 11. References

- Repo surface: `src/metrics/snapshot.rs` (`MetricsSnapshot`, `with_pool_status`),
  `src/metrics/backend_stats.rs` (`BackendStats`, typed accessors),
  `src/metrics/collector.rs` (`snapshot()`), `src/runtime.rs` (`spawn_metrics_saver`).
- Prometheus exposition format: `text/plain; version=0.0.4`.
- VictoriaMetrics is Prometheus-API-compatible; `vmagent` uses standard Prometheus scrape
  config.
