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
2. `[logging]`
3. `[routing]`
4. `[routing.queue.backpressure]`
5. `[memory]`
6. `[cache]`
7. `[cache.disk]`
8. `[health_check]`
9. `[metrics]`
10. `[client_auth]`
11. `[[client_auth.users]]`
12. `[[servers]]`

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

### `[logging]`

| Field | Default | Notes |
| --- | --- | --- |
| `format` | `"text"` | Headless stdout format: `"text"` or `"json"`. `--log-format` / `NNTP_PROXY_LOG_FORMAT` override it |
| `backend_summary_interval_secs` | `0` (off) | Seconds between per-backend summary lines at `info`; `300` is a reasonable choice for log-only monitoring. At most `86400` |

Unknown keys in `[logging]` are reported at startup. See [logging.md](logging.md) for field names and the level policy.

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
| `rescue_floor_percent` | `0` | Rescue-rate floor (0-100, `0` = off). A backend's rescue rate is the share of its recent lookups, made after another backend had answered `430`, that found the article. After another backend has answered `430`, a backend under the floor is used only when it has a connection free right away, and a lookup whose only unconfirmed backends are under it answers `430` at once. See below |
| `unfinished_replies_before_430` | `2` | How many `503`s one message-id may get, once at least one backend returned `430` for it, before the next unfinished lookup answers `430`. `0` keeps answering `unavailable_reply`. Ignored when `unavailable_reply` is `"430"` |
| `standing` | `"off"` | `"off"` or `"adaptive"`. With `"adaptive"`, each backend's share of article lookups within its tier is scaled by its recent standing: it falls fast on `430` and climbs back slowly on hits. Tier order and retry rules are unchanged. See below |
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

- a backend under the floor is used only when a checkout would not wait: an
  idle connection, or room to open one, with no checkout already queued. The
  checkout itself decides, without blocking, so concurrent lookups cannot all
  count on the same idle connection. Otherwise the backend is skipped like a
  backlogged backend, whatever its queue estimate says. A connection the
  client session already holds to that backend is still reused. The `STAT`
  probes that follow a `430` and the background lower-tier `STAT` probes
  apply the same rule;
- a skipped backend under the floor is waited on as a last resort only to keep
  the tier rule (no lower tier before every eligible backend in this tier
  answered `430`): when a lower tier could still serve the request, the
  request waits on it rather than moving down. When no lower tier could, it
  is not waited on;
- if every backend that has not answered `430` was skipped as backlogged and
  is under the floor, the lookup answers `430` at once. A backend that failed,
  refused, or ran out of time instead keeps the normal `503` handling.

One in every 16 pass-overs is let through to a normal checkout instead, and a
request counts at most one pass-over per backend. Together with a backend
that is not busy being asked normally, this keeps its rate tracking the
workload, so it recovers when the backend starts finding articles again.
Background lower-tier probes never count as pass-overs.

The floor only ever skips through backlog skipping: with `backlog_skip =
false` nothing is skipped, and `rescue_floor_percent` has no effect.

The trade-off is an article that only that backend has, while its rate is
below the floor. A low floor such as `2` limits that to roughly one article in
fifty such lookups.

Skips and checkout timeouts are summarised at `info` once per backend every
10 seconds instead of logged per request.

#### Article standing

Takedowns remove a whole release from a provider at once, so a backend that
starts answering `430` for a download usually keeps doing so for the rest of
it. Each backend keeps a standing from 0 to 1, starting at 1:

- every `430`, or a lookup refused with another error, moves it down by 1/8
  of its value: about 18 misses in a row take it to a tenth, and about 35 to a
  hundredth;
- every hit moves it up by 1/128 of the gap to 1: about 88 hits in a row take
  a bottomed-out backend halfway back, and about 300 to nine tenths.

Direct `ARTICLE`/`BODY`/`HEAD`/`STAT` attempts and the `STAT` probes that
follow a `430` all count. A backend settles near 1 when it misses one lookup
in a thousand, and near 0.5 when it misses one in twenty.

With `standing = "adaptive"`, article lookups weigh backends within a tier by
capacity times standing, never below a floor of 1/64:

- `backend_selection = "least-loaded"` compares `(active + 1) / (max_connections
  × standing)`. A demoted backend gets first attempts only once its peers are
  busy enough to look worse, so it serves as overflow rather than first
  choice. The `+ 1` matters because a backend that answers `430` quickly stays
  idle, and an idle backend would otherwise always look least loaded.
- `backend_selection = "weighted-round-robin"` uses `max_connections ×
  standing`, rounded up, as the share, so a demoted backend still gets one or
  two slots per cycle.

Standing only reorders backends that can take the request now. A backend counts
as free when it has an idle pooled connection, or room to open one that login
backoff won't refuse; connections a client holds between commands are not free
to others. When any backend in the tier is free, only free backends are
considered, so a request never queues on a well-standing but full pool next to
a demoted one with idle capacity. When none is free, standing decides as before.

A demoted backend still gets every retry its tier owes it. When its peers
answer `430` the request comes to it before any lower tier, as before. Those
retries, the floor share, and overflow are how it earns its standing back.
Standing is not saved across restarts, so every backend starts at 1.

`GROUP`, overview, and list commands ignore standing. Standing is tracked and
exported as `nntp_proxy_backend_article_standing` even when the setting is
`"off"`, so you can watch it before turning it on.

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
| `connection_cap` | `"fixed"` | `"adaptive"` lets the pool size follow login refusals instead of always regrowing to `max_connections`. See [Adaptive connection cap](#adaptive-connection-cap) |
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
| `backend_idle_timeout` | `600` seconds | Closes a pooled connection once it has gone this long without a completed exchange, even while clients stay connected. Checked every half timeout (1–60 s); `0` disables |
| `prewarm_connections` | `max_connections` | Connections opened at startup; must not exceed `max_connections`. `0` opens none and the pool fills on demand. Lower it on a backend whose account is shared with other clients, so idle pooled connections don't use up the provider's per-account limit. |

Availability identity is derived from the configured `host` value only; the
proxy does not resolve DNS to infer provider identity. Hostnames are compared
case-insensitively after removing one trailing DNS dot. Ports, credentials,
TLS settings, tiers, and display names do not create separate provider slots.

Unknown keys in `[[servers]]`, `[routing]` (including its subtables),
`[metrics]`, and `[logging]` are
ignored, and the proxy logs a warning naming each one at startup, for example
`Ignoring unknown config key 'servers.1.stateful_session'`.

### Adaptive connection cap

`max_connections` should match what the provider account allows, but the real
limit can move: another client shares the account, the provider lowers it, or
a session it still counts is slow to close. With the default
`connection_cap = "fixed"` the pool always regrows to `max_connections`, so it
keeps hitting "too many connections". Login backoff only spaces those
refusals out (5 s, doubling to 60 s).

With `connection_cap = "adaptive"` the pool remembers a size that works:

- **Fast down.** When the provider refuses a login because the account is at
  its connection limit (a `482`, or reply text such as "too many connections"
  or "connection limit"), the cap drops at once to the number of connections
  the provider had accepted, and the pool shrinks to match. Bad credentials
  (`481`), a locked or expired account, a quota, or any other refusal back off
  but leave the cap alone: fewer connections would not fix them. See
  [Login refusals and connection limits](operations.md#login-refusals-and-connection-limits).
- **Slow up.** Below `max_connections`, the cap grows by one connection per
  minute without a refusal.
- **Very slow past `max_connections`.** At or above it, the cap grows by one
  connection every 10 minutes, and only while the pool is full and every
  connection is in use, so it never climbs while idle and then opens a burst
  the provider never allowed. The next refusal brings it straight back down.
- **Paused during login backoff.** The cap does not climb while logins are
  paused.

Each refusal also restarts the climb timer. The cap starts at
`max_connections` on every restart. Replacement cooldowns shrink the pool
from the current cap. `nntp_proxy_backend_connection_cap` and
`nntp_proxy_backend_connection_cap_reductions_total` show it; the
environment variable is `NNTP_SERVER_<N>_CONNECTION_CAP`.

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
