# Runtime and Routing

## UI modes

`nntp-proxy` has one runtime binary with two public UI modes:

- `--ui headless`
- `--ui tui`

Shorthands:

- `--headless`
- `--tui`

## Dashboard attach/listen

Headless mode can publish dashboard state over a loopback websocket:

```bash
nntp-proxy --ui headless --tui-listen 127.0.0.1:8120
```

Another terminal can attach a read-only TUI client:

```bash
nntp-proxy --ui tui --tui-attach 127.0.0.1:8120
```

Rules:

- `--tui-listen` requires headless mode
- `--tui-attach` requires TUI mode
- Both addresses must stay on loopback
- The dashboard socket must be different from the main NNTP listener

## Routing modes

The config surface and CLI expose three modes:

| Mode | Intended behavior |
| --- | --- |
| `per-command` | Stateless routing, each supported command may use a different backend |
| `hybrid` | Starts per-command and switches to a dedicated backend for stateful commands |
| `stateful` | One client session maps to one backend connection |

## Current runtime behavior

Hybrid is the default routing mode. `per-command` and `stateful` remain available as explicit modes, and the runtime honors the configured mode.

## What per-command mode supports

Per-command mode is meant for stateless operations, especially message-ID
lookups such as:

- `ARTICLE <message-id>`
- `BODY <message-id>`
- `HEAD <message-id>`
- `STAT <message-id>`
- `LIST`
- `HELP`
- `DATE`
- `CAPABILITIES`

Commands that depend on selected group or current article state are not a good fit for `per-command` mode.

## Response Handling

Routing code works with typed request metadata, not raw command strings. The
request kind decides whether a successful backend status has a response body:
`211` is single-line for `GROUP` but multiline for `LISTGROUP`, while article
payload commands use their own `220`/`221`/`222` body status codes.

Backend response boundaries are owned by `src/session/multiline_framing.rs`.
Normal ARTICLE/BODY/HEAD forwarding writes borrowed slices from pooled backend
read buffers. The proxy only owns full response payloads for explicit retention
paths such as article-body caching, cache-hit rendering, list-style responses
that need ownership, tests/prechecks, and oversize or pool-exhaustion fallbacks.

When a backend read includes bytes for the next response, those bytes are kept as
opaque pending input on the backend connection. Connections with pending bytes
are not returned to the general pool after a completed per-command response.

## CLI overrides

Most day-to-day runtime overrides map directly to config:

| Flag | Environment | Config field |
| --- | --- | --- |
| `--config <FILE>` | `NNTP_PROXY_CONFIG` | config source |
| `--ui <MODE>` | `NNTP_PROXY_UI` | runtime UI |
| `--tui-listen <IP:PORT>` | `NNTP_PROXY_TUI_LISTEN` | dashboard publisher |
| `--tui-attach <IP:PORT>` | `NNTP_PROXY_TUI_ATTACH` | dashboard client |
| `--host <HOST>` | `NNTP_PROXY_HOST` | `[proxy].host` |
| `--port <PORT>` | `NNTP_PROXY_PORT` | `[proxy].port` |
| `--routing-mode <MODE>` | `NNTP_PROXY_ROUTING_MODE` | `[routing].mode` |
| `--backend-selection <STRATEGY>` | `NNTP_PROXY_BACKEND_SELECTION` | `[routing].backend_selection` |
| `--article-cache-capacity <SIZE>` | `NNTP_PROXY_ARTICLE_CACHE_CAPACITY` | `[cache].article_cache_capacity` |
| `--article-cache-ttl <SECONDS>` | `NNTP_PROXY_ARTICLE_CACHE_TTL_SECS` | `[cache].article_cache_ttl_secs` |
| `--store-article-bodies <BOOL>` | `NNTP_PROXY_STORE_ARTICLE_BODIES` | `[cache].store_article_bodies` |
| `--threads <N>` | `NNTP_PROXY_THREADS` | `[proxy].threads` |

CLI flags override the loaded config.

## Metrics persistence

Metrics are persisted to:

- `[proxy].stats_file` when set
- otherwise `stats.json` next to the config file

The dashboard uses that persisted state to survive restarts.
