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

## Metrics and state files

Common persisted files:

- `stats.json` for metrics persistence when `[proxy].stats_file` is not set
- `availability.idx` for availability-only cache persistence when `[cache]` is configured, `store_article_bodies = false`, and `[cache].availability_index_path` is not set

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
- Check logs for backend response codes.

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
