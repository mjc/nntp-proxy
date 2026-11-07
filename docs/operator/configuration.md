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
8. `[client_auth]`
9. `[[client_auth.users]]`
10. `[[servers]]`

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

### `[routing.queue.backpressure]`

| Field | Default | Notes |
| --- | --- | --- |
| `enabled` | `true` | Enable queue-pressure-aware backend filtering |
| `soft_waiters_per_connection_percent` | `25` | Penalize backends above this queued-requests-per-connection threshold |
| `hard_waiters_per_connection_percent` | `50` | Filter backends above this threshold while alternatives exist in the tier |
| `all_busy_sleep_ms` | `1` | Sleep before retrying when every eligible backend in the preferred tier is hard-saturated |

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
