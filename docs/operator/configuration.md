# Configuration

## Config sources and precedence

`nntp-proxy` loads configuration in this order:

1. `--config <FILE>` / `NNTP_PROXY_CONFIG` if the file exists
2. `NNTP_SERVER_*` backend variables override `[[servers]]` from the file
3. `NNTP_PROXY_CREDENTIALS_FILE` can overlay backend and client-auth secrets
4. Selected CLI flags override the loaded config

If no config file exists and no backend environment variables are present, the proxy creates a default `config.toml`.

## Canonical section order

Keep the config ordered from general settings to concrete identities:

1. `[proxy]`
2. `[routing]`
3. `[routing.queue.backpressure]`
4. `[memory]`
5. `[cache]`
6. `[cache.disk]`
7. `[health_check]`
8. `[metrics]`
9. `[client_auth]`
10. `[[client_auth.users]]`
11. `[[servers]]`

See [../../config.full.toml](../../config.full.toml) for a complete example.

## Section reference

### `[proxy]`

| Field | Default | Notes |
| --- | --- | --- |
| `host` | `0.0.0.0` | Listen host |
| `port` | `8119` | Listen port |
| `threads` | `1` | Use `0` for CPU-count worker threads |
| `validate_yenc` | `true` | Validate yEnc structure/checksums |
| `log_file_level` | `"warn"` | Filter for the optional local `debug.log` appender |
| `response_write_metrics_secs` | unset | Optional interval for response write metrics logging |
| `stats_file` | unset | If unset, metrics persistence defaults to `stats.json` next to the config file |

### `[routing]`

| Field | Default | Notes |
| --- | --- | --- |
| `mode` | `hybrid` | `hybrid` is the default; `per-command` and `stateful` remain available explicit modes |
| `backend_selection` | `least-loaded` | Or `weighted-round-robin` |
| `adaptive_precheck` | `false` | Adaptive availability precheck for `STAT`/`HEAD` |
| `reader_message_id_failover` | `false` | In hybrid mode, answer a pinned backend's `430` for a message-ID lookup from another backend. See [Runtime and routing](runtime-and-routing.md#message-id-lookups-in-a-reader-session) |
| `lower_tier_stat_prefetch` | `"first-attempt"` | Background `STAT` probes that `ARTICLE`/`BODY` send to tier >0 backends with `stat_missing` enabled, so a primary-tier `430` can skip lower-tier misses. `"first-attempt"` probes each eligible lower-tier backend once per request, `"after-primary-miss"` probes only when the availability index already records a tier-0 `430` for the message ID, and `"off"` never prefetches. The retry `STAT` sweep after a primary-tier `430` runs in every mode. Consider `"off"` or `"after-primary-miss"` when the primary tier rarely misses and lower-tier pools are small. |

### `[routing.queue.backpressure]`

| Field | Default | Notes |
| --- | --- | --- |
| `enabled` | `true` | Enable queue-pressure-aware backend filtering |
| `soft_waiters_per_connection_percent` | `25` | Penalize backends above this queued-requests-per-connection threshold |
| `hard_waiters_per_connection_percent` | `50` | Filter backends above this threshold while alternatives exist in the tier |
| `all_busy_sleep_ms` | `1` | Sleep before retrying when every eligible backend in the preferred tier is hard-saturated |

### `[routing.article]`

Time limits for `ARTICLE`, `BODY`, `HEAD`, and `STAT` lookups by message-id.

| Field | Default | Notes |
| --- | --- | --- |
| `deadline_ms` | `20000` | Total time one client request may spend across all backend attempts. When it runs out the proxy answers with `unavailable_reply`. `0` disables the deadline. Set the client's article timeout a little above this |
| `checkout_wait_ms` | `3000` | Longest one attempt waits for a free connection on a backend, also cut to what is left of `deadline_ms` |
| `backlog_skip` | `true` | Skip a backend at once, without queueing, when its estimated connection wait is too long |
| `backlog_skip_percent` | `100` | Skip when the estimated wait is longer than this percentage of the attempt's wait. Lower values skip sooner |
| `unavailable_reply` | `"503"` | `"503"` or `"430"`: the reply when a request ends before every eligible backend returned `430`, either because `deadline_ms` ran out or because every remaining backend was busy, skipped, refused the lookup with a non-`430` error, or failed. A request that every eligible backend answered with `430` always gets `430` |
| `rescue_floor_percent` | `0` | Rescue-rate floor (0-100, `0` = off). A backend's rescue rate is the share of its recent lookups, made after another backend had answered `430`, that found the article. A busy backend under the floor is not waited on as a last resort, and a lookup whose only unconfirmed backends are under it answers `430` at once. See below |
| `unfinished_replies_before_430` | `2` | How many `503`s one message-id may get, once at least one backend returned `430` for it, before the next unfinished lookup answers `430`. `0` keeps answering `unavailable_reply`. Ignored when `unavailable_reply` is `"430"` |
| `backlog_last_resort` | `"wait-shortest"` | What a request does when backlog skipping leaves no other backend to try in the tier that should serve it. `"wait-shortest"` queues on the skipped backend with the shortest estimated wait, bounded by `checkout_wait_ms` and what is left of `deadline_ms`; `"give-up"` ends the request with `unavailable_reply` |

The estimated wait is `(requests ahead in the pool queue) × (average time one
command holds a connection) / (pool size)`. The average is a running mean of
recent commands on that backend, so the estimate is zero until the backend has
finished a command, and zero while it has an idle connection or room to open
one. Background `STAT` probes use the same skip rule with their fixed 2-second
wait.

A skipped or timed-out backend is unavailable for that request only. It is not
recorded as missing the article, and the next request tries it again. As with
any busy backend, a skip does not move the lookup to a lower-priority tier
while another backend in the current tier has not answered `430`.

Skipping saves time only when another backend can answer instead. When every
backend left in the tier was skipped as backlogged, the default
`backlog_last_resort = "wait-shortest"` queues on the one expected to free a
connection first rather than failing the request. That wait is bounded like any
other checkout, and if it times out the request ends with `unavailable_reply`
as before. A backend that
answered `430` or failed is never a last-resort candidate, and background
`STAT` probes always skip.

The proxy answers `430` only when every backend that may serve the request
returned `430`, either during this request or as a cached availability fact.
Downloaders take `430` as final: a typical downloader stops trying an article
on a server after `430` but retries it later after other errors. A request that
ended because a backend was busy, skipped, refused the lookup with a non-`430`
error, or failed, or because the deadline ran out, has not established that
the article is missing, so it gets `503 Backend error` by default and the
downloader retries.
Set `unavailable_reply = "430"` to fail such requests fast instead.

Answering only `503` makes a truly dead article slow to fail when a small,
busy backend can never confirm the miss in time: the downloader retries it
again and again. So once a message-id has had `unfinished_replies_before_430`
`503`s, the next unfinished lookup answers `430` if at least one backend has
returned `430` for it, during this request or as a cached fact. A backend that
was busy, skipped, failed, or refused with another error (such as `451`) says
nothing either way, so it does not block the `430`. The count lives in a
fixed 64K-slot table (512 KiB); a slot collision only restarts a count.

| How the request ended | Reply |
| --- | --- |
| A backend returned the article (or `223` for `STAT`) | the backend's reply |
| Every eligible backend returned `430` (or has a cached `430` fact) | `430` |
| `deadline_ms` ran out | `unavailable_reply` (`503`) |
| Every backend left was busy, skipped, refused with a non-`430` error (such as `451`), or failed, even if others returned `430` | `unavailable_reply` (`503`) |
| Either of the two above, after `unfinished_replies_before_430` earlier `503`s for this message-id, with at least one `430` | `430` |
| Either of the two above, when every backend that has not answered `430` was skipped as busy and is under `rescue_floor_percent` | `430` |

#### Rescue-rate floor

When articles are gone from every provider, each one must still be confirmed
missing by every backend before the proxy can answer `430`. A small or slow
fallback backend then becomes the bottleneck: every lookup queues on it, and
the client waits out checkout timeouts and retries for each missing article.

The proxy keeps, per backend, an exponential average (weight 1/64, a running
mean for the first 64 samples) of how often a lookup sent after another
backend's `430` found the article. Direct attempts and the `STAT` probes that
follow a `430` both count. The rate is trusted after 100 such lookups.

With `rescue_floor_percent` set, and only once some backend has answered `430`
for the article:

- a backend under the floor that this request skipped as backlogged is not
  waited on as a last resort, unless the request could not be served by any
  lower tier either, so the tier rule (no lower tier before every eligible
  backend in this tier answered `430`) still holds;
- if every backend that has not answered `430` was skipped as backlogged and
  is under the floor, the lookup answers `430` at once. A backend that failed,
  refused, or ran out of time instead keeps the normal `503` handling.

One in every 16 pass-overs still waits on the backend, and a backend that is
not busy is asked normally, so its rate keeps tracking the workload and
recovers when it starts finding articles again.

The trade-off is an article that only that backend has, while its rate is
below the floor. A low floor such as `2` limits that to roughly one article in
fifty such lookups.

Skips and checkout timeouts are summarised at `info` once per backend every
10 seconds instead of logged per request.

### `[memory]`

These settings control transport memory, not stored article bodies.

| Field | Default |
| --- | ---: |
| `socket_recv_buffer_size` | `16777216` |
| `socket_send_buffer_size` | `16777216` |
| `buffer_pool_size` | `1048576` |
| `buffer_pool_count` | `100` |
| `capture_pool_size` | `1048576` |
| `capture_pool_count` | `16` |

With `store_article_bodies = true`, each cache miss copies its response into
capture buffers until the cache stores it. A read that fills at least half of
its read buffer moves that buffer into the capture instead. A capture that
finds the capture pool empty allocates a fresh buffer and logs
`Capture buffer pool exhausted`. The default of 16 covers 16 concurrent
captures of articles up to `capture_pool_size`. If cache misses routinely run
on more backend connections than that, raise `capture_pool_count` toward your
total backend `max_connections`. Pooled buffers are allocated on first use and
kept afterward, so this costs up to `capture_pool_count × capture_pool_size`
of memory. The fallback allocations are freed when each capture finishes, so
leaving the default only costs allocator churn, not unbounded memory.

### `[cache]`

`[cache]` is optional. If you omit it entirely, the runtime still uses availability tracking internally.

| Field | Default | Notes |
| --- | --- | --- |
| `article_cache_capacity` | `64mb` | Memory tier capacity |
| `article_cache_ttl_secs` | `3600` | Base TTL |
| `store_article_bodies` | `false` | Availability tracking stays on either way |
| `availability_index_path` | unset | When unset in availability-only mode, defaults to `availability.idx` next to the config file |

The availability-only index has a fixed capacity. It stores exact message IDs,
compact fingerprints, and bits identifying providers that returned `430`.
Rotation and replacement can remove old entries, causing previous misses to be
retried. A fingerprint match alone cannot mark an article as missing.

### `[cache.disk]`

Optional disk tier for article bodies evicted from memory. It is only active when `store_article_bodies = true`.

| Field | Default |
| --- | --- |
| `path` | `/var/cache/nntp-proxy` |
| `capacity` | `10gb` |
| `compression` | `lz4` |
| `shards` | `4` |

### `[health_check]`

| Field | Default |
| --- | --- |
| `interval` | `30` seconds |
| `timeout` | `5` seconds |
| `unhealthy_threshold` | `3` failures |

### `[metrics]`

Serves Prometheus metrics at `http://<listen>/metrics`. Without this section, no metrics port is opened.

| Field | Default |
| --- | --- |
| `listen` | `"127.0.0.1:9469"` |

```toml
[metrics]
listen = "127.0.0.1:9469"
```

`--metrics-listen <IP:PORT>` or `NNTP_PROXY_METRICS_LISTEN=<IP:PORT>` sets the address and turns the endpoint on even when the config file has no `[metrics]` section. The endpoint has no authentication. Keep it on loopback or a private network. See [Metrics](metrics.md) for the metric reference and scrape examples.

### `[client_auth]` and `[[client_auth.users]]`

Client auth is optional and applies to the proxy itself, not the backend servers.

```toml
[client_auth]
greeting = "200 NNTP proxy ready"

[[client_auth.users]]
username = "reader"
password = "reader-password"
```

### `[[servers]]`

| Field | Default | Notes |
| --- | --- | --- |
| `host` | required | Backend hostname or IP; canonical hostname identity used for availability sharing |
| `port` | required | `119` for plain NNTP, `563` for NNTPS |
| `name` | required | Friendly name used in logs and the TUI |
| `username` / `password` | unset | Backend auth |
| `max_connections` | `10` | Per-backend pool size |
| `stat_missing` | `0` | Probe missing articles with `STAT` before `ARTICLE`/`BODY`/`HEAD` on this backend. Enable it on backends that correctly return `430` to speed up retrying missing articles. |
| `tier` | `0` | Lower tiers are preferred first |
| `stateful_sessions` | `"allowed"` | `"excluded"` keeps this backend out of stateful sessions (`GROUP`, `XOVER`, article-number commands) and other replies that carry article numbers, while it still serves message-ID requests. See [Article numbering across providers](#article-numbering-across-providers). |
| `use_tls` | `false` | TLS to the backend |
| `tls_verify_cert` | `true` | Keep enabled in production |
| `tls_cert_path` | unset | Additional PEM CA certificate |
| `connection_keepalive` | unset | Sends `DATE` on idle backend connections |
| `replacement_cooldown` | `30` seconds | Set `0` to disable |
| `health_check_max_per_cycle` | `5` | Per-backend health-check acquisition cap |
| `health_check_pool_timeout` | `100ms` | Wait time when health-checking the pool |
| `compress` | auto-detect | RFC 8054 backend `COMPRESS DEFLATE` |
| `compress_level` | unset | Level `0`-`9` when compression is enabled |
| `backend_idle_timeout` | `600` seconds | Clears idle backend connections after proxy-wide inactivity |
| `prewarm_connections` | `max_connections` | Connections opened at startup; must not exceed `max_connections`. `0` opens none and the pool fills on demand. Lower it on a backend whose account is shared with other clients, so idle pooled connections don't use up the provider's per-account limit. |

Availability identity is derived from the configured `host` value only; the
proxy does not resolve DNS to infer provider identity. Hostnames are compared
case-insensitively after removing one trailing DNS dot. Ports, credentials,
TLS settings, tiers, and display names do not create separate provider slots.

Unknown keys in `[[servers]]` and `[routing]` (including its subtables) are
ignored, and the proxy logs a warning naming each one at startup, for example
`Ignoring unknown config key 'servers.1.stateful_session'`.

### Article numbering across providers

Each provider numbers the articles in a group independently. In `hybrid` and
`stateful` modes the proxy pins a client to one backend for `GROUP`, `XOVER`, and
other article-number commands. If two of a client's sessions land on providers
with different numbering, a client that stores article numbers (an indexer
tracking its position in each group) gets numbers that don't match.

Set `stateful_sessions = "excluded"` on every backend whose numbering differs
from the one your client tracks. Excluded backends keep serving message-ID
requests (`ARTICLE <id>`, `BODY <id>`, `HEAD <id>`, `STAT <id>`) in their tier,
so they still add download capacity. Every other shared request, such as
`LIST ACTIVE` and `LIST COUNTS` (whose replies carry high and low article
numbers), only goes to allowed backends:

```toml
[[servers]]
name = "indexer-numbering"
host = "news.example.com"
port = 563
tier = 0

[[servers]]
name = "download-only"
host = "news.other-provider.example"
port = 563
tier = 0
stateful_sessions = "excluded"
```

The proxy logs `Backend download-only excluded from stateful sessions` at
startup for each excluded backend. If every backend in a tier is excluded,
stateful sessions and numbered replies use the next tier. When a hybrid session
can't get a backend for its first stateful command, the client receives
`400 Backend server unavailable`, the same reply stateful mode sends. Startup
fails when `hybrid` or `stateful` mode has no allowed backend at all. In
`per-command` mode there are no stateful sessions and the setting has no effect.

## Environment-based backend configuration

Indexed `NNTP_SERVER_<N>_*` variables can define backends without a TOML server list.

Currently supported backend environment fields:

- `HOST`
- `PORT`
- `NAME`
- `USERNAME`
- `PASSWORD`
- `MAX_CONNECTIONS`
- `STAT_MISSING`
- `USE_TLS`
- `TLS_VERIFY_CERT`
- `TLS_CERT_PATH`
- `CONNECTION_KEEPALIVE`
- `HEALTH_CHECK_MAX_PER_CYCLE`
- `HEALTH_CHECK_POOL_TIMEOUT`
- `PREWARM_CONNECTIONS` (non-negative integer; any other value stops startup)
- `TIER`
- `STATEFUL_SESSIONS` (`allowed` or `excluded`; any other value stops startup)

Example:

```bash
NNTP_SERVER_0_HOST=news.example.com \
NNTP_SERVER_0_PORT=563 \
NNTP_SERVER_0_NAME=Primary \
NNTP_SERVER_0_USE_TLS=true \
nntp-proxy
```

Indices must be contiguous. Loading stops at the first missing `NNTP_SERVER_<N>_HOST`.

## Credentials overlay

Set `NNTP_PROXY_CREDENTIALS_FILE` to a TOML overlay file when you want to keep secrets out of the main config.

Example:

```toml
[[servers]]
name = "Primary"
username = "backend-user"
password = "backend-pass"

[client_auth]

[[client_auth.users]]
username = "reader"
password = "reader-password"
```

Server entries in the overlay are matched by `name`.
