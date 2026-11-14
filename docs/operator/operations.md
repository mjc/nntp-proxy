# Operations

## Docker

Build:

```bash
docker build -t nntp-proxy .
```

Run with environment-provided backends:

```bash
docker run -d \
  --name nntp-proxy \
  -p 8119:8119 \
  -e NNTP_SERVER_0_HOST=news.example.com \
  -e NNTP_SERVER_0_PORT=119 \
  -e NNTP_SERVER_0_NAME=Primary \
  -e NNTP_SERVER_0_STAT_MISSING=1 \
  -e NNTP_SERVER_0_USERNAME="$BACKEND_USER" \
  -e NNTP_SERVER_0_PASSWORD="$BACKEND_PASS" \
  nntp-proxy
```

Set `NNTP_SERVER_*_STAT_MISSING=1` on backends that correctly return `430` for missing articles if you want the proxy to prefetch those misses with `STAT`.

The repository also includes [../../docker-compose.yml](../../docker-compose.yml).

## Logs

Headless mode logs to stdout. Set `[logging] format = "json"` (or
`NNTP_PROXY_LOG_FORMAT=json`) when a log store should index fields, and alert
on the `event` field. Colour is used only on a terminal. See
[logging.md](logging.md).

## Metrics and state files

Common persisted files:

- `stats.json` for metrics persistence when `[proxy].stats_file` is not set
- `availability.idx` for availability-only cache persistence when `[cache]` is configured, `store_article_bodies = false`, and `[cache].availability_index_path` is not set
- `availability.registry` beside a hybrid cache's payload directory, when a
  disk-backed cache is configured; it maps canonical provider hostnames to the
  slots used for availability tracking. It contains no credentials or
  message IDs.

The proxy records missing articles by exact message ID and canonical provider
hostname. The same hostname shares a provider slot across ports, addresses,
accounts, TLS settings, tiers, and display names. The bounded index can forget
old facts through expiry, rotation, or replacement. The proxy retries a provider
once it has forgotten the earlier miss.

If you want explicit paths, configure them directly in `config.toml`.

## Deployment notes

- Client-facing connections are plain NNTP only.
- TLS applies to backend connections, not the client listener.
- Keep dashboard websocket addresses on loopback.
- Use a credentials overlay file when you do not want secrets in the main config.
- Hybrid is the default routing mode.
- `stateful` and `per-command` remain available as explicit modes.

## Troubleshooting

### Port already in use

```bash
lsof -i :8119
nntp-proxy --port 8120
```

### Backend auth failures

- Check `username` and `password` on the matching `[[servers]]` entry.
- Test direct connectivity to the backend.
- Look for `event=backend_login_refused` or `event=backend_connection_limit`
  lines; their `error` field carries the backend's response. See
  [logging.md](logging.md) for every alertable event.

### Login refusals and connection limits

When a backend refuses a login (a bad greeting, a failed `AUTHINFO`, a refused
`MODE READER`, or bad credentials), the proxy stops logging in to it for 5
seconds, doubling up to 60 seconds while the refusals continue. When a pause
ends, a single probe login decides whether to resume. Network errors do not
start a pause. Each kind of refusal has its own rate-limited operator event
(`backend_login_refused`, `backend_connection_limit`, `backend_unavailable`,
`backend_reader_mode_refused`, `backend_login_rejected`) carrying the backend's
reply; see [logging.md](logging.md).

A refusal counts as a connection limit when it is a `482`, or when its text
says so in whole words ("too many [simultaneous] connections", "max conns",
"maximum number of connections", "connection limit", "session limit",
"connections exceeded", "limit of 20 connections", and similar). A `481`, or
text about login attempts, failed logins, a locked or expired account, a quota,
a download limit, a rate limit, or commands out of sequence, is never a
connection limit. Only a connection-limit refusal lowers an adaptive
connection cap (see
[Adaptive connection cap](configuration.md#adaptive-connection-cap)); a
credential failure keeps the pool size and only backs off.

### Routing surprises

- Check `[routing].mode` first when behavior does not match your workload.
- `hybrid` starts per-command and switches to a dedicated backend for stateful commands.

### TLS failures

- Keep `tls_verify_cert = true` in production.
- Check the system CA store.
- Add `tls_cert_path` for private CA deployments.

### Unexpected memory use

- `[cache]` controls retained article data.
- `[memory]` controls socket buffers and buffer pools.
- `[cache.disk]` adds a disk tier for article bodies, not transport memory.

## TCP tuning

See [tcp-tuning.md](tcp-tuning.md) for the current socket options the proxy sets and for optional host-level tuning guidance.
