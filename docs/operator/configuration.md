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
3. `[memory]`
4. `[cache]`
5. `[cache.disk]`
6. `[health_check]`
7. `[client_auth]`
8. `[[client_auth.users]]`
9. `[[servers]]`

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
| `stats_file` | unset | If unset, metrics persistence defaults to `stats.json` next to the config file |

### `[routing]`

| Field | Default | Notes |
| --- | --- | --- |
| `mode` | `hybrid` | `hybrid` is the default; `per-command` and `stateful` remain available explicit modes |
| `backend_selection` | `least-loaded` | Or `weighted-round-robin` |
| `adaptive_precheck` | `false` | Adaptive availability precheck for `STAT`/`HEAD` |

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
| `host` | required | Backend hostname or IP |
| `port` | required | `119` for plain NNTP, `563` for NNTPS |
| `name` | required | Friendly name used in logs and the TUI |
| `username` / `password` | unset | Backend auth |
| `max_connections` | `10` | Per-backend pool size |
| `stat_missing` | `0` | Probe missing articles with `STAT` before `ARTICLE`/`BODY`/`HEAD` on this backend. Enable it on backends that correctly return `430` to speed up retrying missing articles. |
| `tier` | `0` | Lower tiers are preferred first |
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
