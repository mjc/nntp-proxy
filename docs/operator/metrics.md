# Metrics

`nntp-proxy` can serve Prometheus metrics over HTTP for Prometheus, VictoriaMetrics, or any scraper that reads the [text exposition format 0.0.4](https://prometheus.io/docs/instrumenting/exposition_formats/).

## Enabling the endpoint

The endpoint is off by default. Turn it on with a `[metrics]` section:

```toml
[metrics]
listen = "127.0.0.1:9469"
```

or with `--metrics-listen 127.0.0.1:9469` or `NNTP_PROXY_METRICS_LISTEN=127.0.0.1:9469`. The flag and the variable override the config file and turn the endpoint on without a `[metrics]` section. If the address cannot be bound, the proxy fails at startup.

The endpoint answers:

| Request | Response |
| --- | --- |
| `GET /metrics` | `200`, `Content-Type: text/plain; version=0.0.4; charset=utf-8` |
| `HEAD /metrics` | `200` headers only. It does not render the metrics, so it shows the listener is up, not that a scrape would succeed; use `GET` for that. |
| any other path | `404` |
| any other method | `405` |

Every response closes the connection. A request head larger than 8 KiB gets `431`. A client that has not sent its request within 5 seconds is disconnected. At most 32 scrapes are served at once, and further connections are closed immediately instead of queueing.

### Security

The endpoint has no authentication or TLS. Backend names, pool sizes, and traffic volumes show which providers you use and how much. Keep `listen` on loopback, or bind it to a private interface that only your scraper can reach. In a container, publish the port only on an internal network.

## Cost

Values come from counters the proxy already keeps for its dashboard and stats file. A scrape reads them with relaxed atomic loads, takes the availability cache's shard locks briefly to sum hits and misses, and reads a few files under `/proc`. Rendering runs on a blocking thread, not on the proxy's async workers. Between scrapes the endpoint does no work.

Request handling adds a single relaxed atomic increment per client command (command class counter, spread over per-thread stripes so worker threads do not contend on one cache line) and per authoritative `430` (missing-article counter). Pinned (stateful) sessions classify each backend reply as it is framed and add the totals to the per-backend counters once per backend read. Each latency histogram observation is a binary search over 13 fixed bounds plus two relaxed atomic increments, with no allocation or lock. Connection churn counters change once per backend connection opened or closed.

## Labels

Labels are bounded so the number of series does not grow with traffic:

| Label | Values |
| --- | --- |
| `backend` | Configured server `name`. If two servers share a name, the later one gets `#<index>` appended so their series stay distinct. |
| `tier` | Configured server `tier`. |
| `command` | `article`, `body`, `head`, `stat`, `group`, `overview`, `list`, `post`, `auth`, `session`, `other` (unrecognised verbs), `rejected` (lines refused as over-long or invalid) |
| `code_class` | `4xx`, `5xx` |
| `direction`, `state`, `outcome`, `reason`, `result` | Fixed values listed with each metric. |

No metric is labelled by message-ID, client address, or username. Per-user statistics stay in the dashboard and stats file and are not exported, because usernames are unbounded.

## Counter resets

Counters only grow while a process runs. A restart resets most of them to zero, which `rate()` and `increase()` handle. `nntp_proxy_client_sessions_total` and the per-backend command, byte, article, error, and `430` counters are restored from the stats file at startup, so after a restart they continue from the saved value instead of zero. They still never decrease within one process.

## Metric reference

### Process and build

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `nntp_proxy_build_info` | gauge | `version`, `git_sha` | Always `1`. `git_sha` is `unknown` unless the build set `NNTP_PROXY_GIT_SHA`. |
| `nntp_proxy_start_time_seconds` | gauge | | Unix time the process started. |
| `nntp_proxy_process_cpu_seconds_total` | counter | | User plus system CPU time. Linux only. |
| `nntp_proxy_process_resident_memory_bytes` | gauge | | Resident set size. Linux only. |
| `nntp_proxy_process_open_fds` | gauge | | Open file descriptors, sockets included. Linux only. |
| `nntp_proxy_process_threads` | gauge | | OS threads. Linux only. |

On platforms without `/proc`, the `nntp_proxy_process_*` metrics are omitted.

### Clients

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `nntp_proxy_client_sessions_active` | gauge | | Client connections open now. |
| `nntp_proxy_client_sessions_total` | counter | | Client connections accepted. |
| `nntp_proxy_stateful_sessions_active` | gauge | | Client sessions pinned to one backend connection. |
| `nntp_proxy_client_commands_total` | counter | `command` | Commands read from clients, by class. Cache hits and locally answered commands are included. |
| `nntp_proxy_client_bytes_total` | counter | `direction` = `received`, `sent` | Bytes exchanged with clients. Added when each session ends, however it ends, so long sessions show up late; use the backend byte counters for live throughput. |
| `nntp_proxy_pipeline_batches_total` | counter | | Pipelined batches of more than one client command. |
| `nntp_proxy_pipeline_commands_total` | counter | | Commands carried in those batches. |

### Cache

| Metric | Type | Labels | Meaning |
| --- | --- | --- | --- |
| `nntp_proxy_cache_entries` | gauge | | Entries in the memory cache or availability index. The availability index may still count entries past their TTL that it has not rotated out yet; a scrape never triggers rotation. Omitted for the hybrid disk cache, which cannot count entries. |
| `nntp_proxy_cache_size_bytes` | gauge | | Memory used by the memory cache. Omitted for the availability index (its table is allocated up front, so use the capacity) and for the hybrid disk cache (which does not report usage). |
| `nntp_proxy_cache_capacity_bytes` | gauge | | Configured memory capacity. |
| `nntp_proxy_cache_lookups_total` | counter | `result` = `hit`, `miss` | Cache lookups. |
| `nntp_proxy_cache_disk_hits_total` | counter | | Hits served from the disk tier. Disk cache only. |
| `nntp_proxy_cache_disk_capacity_bytes` | gauge | | Configured disk capacity. Disk cache only. |
| `nntp_proxy_cache_disk_bytes_total` | counter | `direction` = `read`, `written` | Bytes moved to and from disk. Disk cache only. |
| `nntp_proxy_cache_disk_ios_total` | counter | `direction` = `read`, `written` | Disk I/O operations. Disk cache only. |

### Backends

Per-backend counters cover every routing mode. Pinned (stateful) sessions count commands, successful articles, `430`s, and error replies per reply, like per-command traffic, so `nntp_proxy_backend_article_bytes_total` and `nntp_proxy_backend_articles_total` update as each article completes in every mode.

Every backend metric has the labels `backend` and `tier`, plus any listed here.

| Metric | Type | Extra labels | Meaning |
| --- | --- | --- | --- |
| `nntp_proxy_backend_commands_total` | counter | | Commands sent to the backend. |
| `nntp_proxy_backend_articles_total` | counter | | Successful `ARTICLE`/`BODY`/`HEAD` responses. |
| `nntp_proxy_backend_article_bytes_total` | counter | | Bytes in those responses. |
| `nntp_proxy_backend_article_missing_total` | counter | | Authoritative `430 No such article` replies. |
| `nntp_proxy_backend_error_replies_total` | counter | `code_class` = `4xx`, `5xx` | Error status replies. `4xx` includes the `430`s above; subtract them for refused commands. |
| `nntp_proxy_backend_errors_total` | counter | | All errors attributed to the backend: `4xx` and `5xx` replies plus transport and session failures. |
| `nntp_proxy_backend_bytes_total` | counter | `direction` = `sent`, `received` | Bytes sent to and received from the backend. Per-command traffic is added per response; pinned (stateful) sessions add theirs every 100 loop iterations and when the session ends. |
| `nntp_proxy_backend_pool_connections` | gauge | `state` = `idle`, `in_use` | Pooled connections. |
| `nntp_proxy_backend_pool_max_connections` | gauge | | Current pool limit. Lower than `max_connections` while replacement cooldowns are active. |
| `nntp_proxy_backend_pool_waiters` | gauge | | Checkouts waiting for a connection. |
| `nntp_proxy_backend_pool_cooldowns` | gauge | | Slots withheld after broken connections. |
| `nntp_proxy_backend_pending_commands` | gauge | | Commands routed to the backend and not finished. |
| `nntp_proxy_backend_stateful_sessions` | gauge | | Client sessions pinned to the backend. |
| `nntp_proxy_backend_connect_attempts_total` | counter | `outcome` = `created`, `failed`, `login_refused`, `backoff_denied` | Attempts to open a connection. `backoff_denied` attempts were refused locally during login backoff without contacting the backend. |
| `nntp_proxy_backend_connections_closed_total` | counter | `reason` = `backend_error`, `cancelled`, `client_abort`, `health_check`, `stale`, `idle` | Connections closed, counted when the connection is destroyed, so `created` minus all closes is the live connection count. `cancelled`: the request using it was cancelled mid-exchange (deadline expiry or a dropped request). `health_check`: the periodic health check found it dead. `stale`: it failed the liveness check when taken from the pool. `idle`: the pool dropped it itself (idle-timeout clearing, a pool shrinking during replacement cooldown, or shutdown). |
| `nntp_proxy_backend_login_backoff` | gauge | | `1` while login backoff makes new connections to the backend fail locally: a backoff window is open, or the window has ended and the one probe login it allows is still in flight. `0` once logins are allowed again, even before a later login succeeds. |
| `nntp_proxy_backend_health_checks_total` | counter | | Idle connections checked by the periodic health check. |
| `nntp_proxy_backend_health_check_failures_total` | counter | | Idle connections that failed it. |

### Backend admission and failover

These explain requests that skipped, waited on, or gave up on a backend without it returning `430`. All have the labels `backend` and `tier`. The admission counters start at zero with each process; `nntp_proxy_backend_error_reply_failovers_total` is restored from the stats file.

| Metric | Type | Meaning |
| --- | --- | --- |
| `nntp_proxy_backend_backlog_skips_total` | counter | Checkouts that skipped the backend because its connection queue would not serve them within their wait (`backlog_skip`). |
| `nntp_proxy_backend_busy_timeouts_total` | counter | Bounded checkouts that waited their whole limit (`checkout_wait_ms`) without a connection. |
| `nntp_proxy_backend_deadline_expiries_total` | counter | Client requests whose deadline (`deadline_ms`) ran out with this backend next in line. |
| `nntp_proxy_backend_last_resort_waits_total` | counter | Checkouts that queued here anyway because every other backend left for the request was backlogged (`backlog_last_resort = "wait-shortest"`). |
| `nntp_proxy_backend_last_resort_connected_total` | counter | Last-resort checkouts that got a connection. |
| `nntp_proxy_backend_last_resort_timeouts_total` | counter | Last-resort checkouts that waited their whole limit without one. |
| `nntp_proxy_backend_error_reply_failovers_total` | counter | Non-`430` error replies to article lookups that were retried on another backend instead of sent to the client. Shows a backend that refuses lookups with a non-standard code. |
| `nntp_proxy_backend_service_time_seconds` | gauge | Moving average of how long one command holds a connection, used to estimate queue waits. `0` before the first sample. |
| `nntp_proxy_backend_estimated_wait_seconds` | gauge | Estimated wait for a connection if a checkout joined the queue now. |
| `nntp_proxy_backend_article_standing` | gauge | Recent article-lookup standing from 0 to 1. Starts at 1, drops by 1/8 on each `430` or refused lookup, and climbs by 1/128 of the gap on each hit. With `[routing.article] standing = "adaptive"` it scales the backend's share of article lookups within its tier. Tracked even when that setting is `"off"`. See [Article standing](configuration.md#article-standing). |

### Backend latency histograms

Both histograms have the labels `backend` and `tier` and expose `_bucket` (with `le`), `_sum` (seconds), and `_count` series. Buckets are fixed. They start empty at each process start and are not saved in the stats file.

| Metric | Buckets (seconds) | Meaning |
| --- | --- | --- |
| `nntp_proxy_backend_first_byte_seconds` | 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10 | Time from sending a command until the backend's first response bytes arrive. Only commands the proxy times one at a time are included: retries and failover attempts, adaptive `STAT`/`HEAD` prechecks, and other commands sent on their own. Commands sent in a pipelined window are not timed individually, so `_count` can be well below `nntp_proxy_backend_commands_total`. |
| `nntp_proxy_backend_exchange_duration_seconds` | 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60 | Time a backend connection was busy with one exchange, from its first write to the end of the response. For an article fetch this is the whole transfer. A pipelined window of several commands on one connection counts as one exchange. This is the same measurement that backlog admission averages. |

## Scrape configuration

### Prometheus

```yaml
scrape_configs:
  - job_name: nntp-proxy
    scrape_interval: 15s
    static_configs:
      - targets: ["127.0.0.1:9469"]
```

### VictoriaMetrics

Single-node VictoriaMetrics and `vmagent` read the same format. Pass `-promscrape.config=scrape.yml` with:

```yaml
scrape_configs:
  - job_name: nntp-proxy
    scrape_interval: 15s
    static_configs:
      - targets: ["127.0.0.1:9469"]
```

When the proxy runs in a container next to the scraper, use the container name and set `listen = "0.0.0.0:9469"` inside the container, without publishing the port on the host.

## Example queries

Share of commands each backend answered with `430`:

```promql
sum by (backend) (rate(nntp_proxy_backend_article_missing_total[5m]))
/
sum by (backend) (rate(nntp_proxy_backend_commands_total[5m]))
```

Throughput received from each backend, in bytes per second:

```promql
sum by (backend) (rate(nntp_proxy_backend_bytes_total{direction="received"}[5m]))
```

Refused commands (non-`430` 4xx replies) per second:

```promql
sum by (backend) (rate(nntp_proxy_backend_error_replies_total{code_class="4xx"}[5m]))
-
sum by (backend) (rate(nntp_proxy_backend_article_missing_total[5m]))
```

95th percentile time to first byte per backend:

```promql
histogram_quantile(0.95,
  sum by (backend, le) (rate(nntp_proxy_backend_first_byte_seconds_bucket[5m])))
```

Median article transfer time per backend:

```promql
histogram_quantile(0.5,
  sum by (backend, le) (rate(nntp_proxy_backend_exchange_duration_seconds_bucket[5m])))
```

Pool saturation, from 0 (idle) to 1 (every connection busy):

```promql
nntp_proxy_backend_pool_connections{state="in_use"}
/ on (backend, tier)
nntp_proxy_backend_pool_max_connections
```

Backends demoted by article standing (below a tenth for ten minutes). A takedown
on one provider shows up as that backend dropping while its tier peers stay
near 1:

```promql
max_over_time(nntp_proxy_backend_article_standing[10m]) < 0.1
```

Alert when requests queue for connections on a backend for several minutes:

```promql
min_over_time(nntp_proxy_backend_pool_waiters[5m]) > 0
```

Requests turned away from busy backends, per second:

```promql
sum by (backend) (
    rate(nntp_proxy_backend_backlog_skips_total[5m])
  + rate(nntp_proxy_backend_busy_timeouts_total[5m])
  + rate(nntp_proxy_backend_deadline_expiries_total[5m]))
```

Connection churn per minute, by reason:

```promql
sum by (backend, reason) (rate(nntp_proxy_backend_connections_closed_total[5m])) * 60
```
