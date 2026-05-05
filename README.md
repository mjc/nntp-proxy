# nntp-proxy

High-throughput NNTP proxy written in Rust. It lets multiple NNTP clients share
multiple backend servers while keeping connection limits, backend auth, routing,
TLS, cache behavior, and live metrics in one place.

## Features

- Hybrid routing by default: starts efficient and switches to stateful mode when a client needs group context.
- Health-aware backend selection with weighted round-robin or least-loaded selection.
- Per-backend TLS, backend authentication, connection limits, keep-alives, and tiering.
- Plain NNTP listener only for clients; intended for trusted LAN/VPN/segmented-network deployment rather than direct internet exposure.
- Optional article-body caching plus lightweight availability tracking.
- Availability-only cache mode by default, so smart routing works without storing article bodies.
- RAM article-cache hits measured 5.16 GB/s on a Ryzen 9 5950X.
- A hot-cache path with a 256 MB in-memory article cache and disk cache on SSD measured 2.58 GB/s on the same system.
- Allocation-conscious hot paths: borrowed request slices, preallocated buffer pools, and allocation-free cache key lookup in the steady state.
- Optional terminal dashboard with persisted metrics across restarts.
- TOML config, environment-based backend configuration, and CLI overrides.

## Runtime Binary

- `nntp-proxy` is the runtime executable.
- The same binary can run headless or with the terminal dashboard via `--ui headless` or `--ui tui`.
- Headless mode can also publish the dashboard state over websocket with `--tui-listen 127.0.0.1:8120` (`HOST:PORT`).
- A separate terminal can attach read-only with `--ui tui --tui-attach 127.0.0.1:8120` (`HOST:PORT`).
- Article caching is configured via `[cache]`; there is no separate cache-only executable.
- Client-facing connections are plain NNTP only. The proxy does not terminate inbound TLS or offer a TLS listening mode.

## Quick Start

Build:

```bash
cargo build --release
```

Create a minimal `config.toml`:

```toml
[[servers]]
host = "news.example.com"
port = 119
name = "Primary"
username = "backend-user"
password = "backend-pass"
max_connections = 10
```

Run the proxy:

```bash
./target/release/nntp-proxy --config config.toml
```

To launch the dashboard-enabled UI, use the same `nntp-proxy` binary with `--ui tui`.

To run headless and expose a remote dashboard:

```bash
./target/release/nntp-proxy --ui headless --tui-listen 127.0.0.1:8120
```

To attach a terminal client to that dashboard:

```bash
./target/release/nntp-proxy --ui tui --tui-attach 127.0.0.1:8120
```

Connect a client to `localhost:8119` unless you changed `[proxy].port`.

## Routing

`routing.mode` controls how client sessions use backend connections:

- `hybrid`: default. Stateless commands use pooled per-command routing until a stateful command appears; then the session switches to a dedicated backend connection.
- `stateful`: one client session maps to one backend connection for the session lifetime.
- `per-command`: each supported command can use a different backend; group-context commands are rejected.

Per-command mode supports message-ID based requests such as:

- `ARTICLE <message-id@example.com>`
- `BODY <message-id@example.com>`
- `HEAD <message-id@example.com>`
- `STAT <message-id@example.com>`
- `LIST`, `HELP`, `DATE`, `CAPABILITIES`

Per-command mode rejects commands that depend on selected group or current article state:

- `GROUP`
- `NEXT`
- `LAST`
- `LISTGROUP`
- `ARTICLE <number>`
- `HEAD <number>`
- `BODY <number>`
- `STAT <number>`
- `XOVER`
- `OVER`
- `XHDR`
- `HDR`

Use `hybrid` unless every client is known to be message-ID only.

## Configuration

Example configs:

- [config.minimal.toml](config.minimal.toml)
- [config.example.toml](config.example.toml)

Canonical section order is general to specific:

- `[proxy]`: listen address, threads, logging, stats path.
- `[routing]`: routing mode, backend selection, adaptive availability precheck.
- `[memory]`: socket buffers and pooled transport/capture buffers.
- `[cache]`: article cache behavior, cache TTL, availability persistence, disk spillover.
- `[health_check]`: backend health probe interval, timeout, and failure threshold.
- `[client_auth]` and `[[client_auth.users]]`: credentials accepted from clients.
- `[[servers]]`: concrete backend servers.

### Complete Shape

```toml
[proxy]
host = "0.0.0.0"
port = 8119
threads = 1
validate_yenc = true
log_file_level = "warn"
# stats_file = "/var/lib/nntp-proxy/stats.json"

[routing]
mode = "hybrid"
backend_selection = "least-loaded"
adaptive_precheck = false

[memory]
socket_recv_buffer_size = 16777216
socket_send_buffer_size = 16777216
buffer_pool_size = 741376
buffer_pool_count = 50
capture_pool_size = 790528
capture_pool_count = 16

[cache]
article_cache_capacity = "256mb"
article_cache_ttl_secs = 3600
store_article_bodies = true
# availability_index_path = "/var/cache/nntp-proxy/availability.idx"  # Only used when store_article_bodies = false

# Optional article-body disk cache.
[cache.disk]
path = "/var/cache/nntp-proxy/articles"
capacity = "10gb"
compression = "lz4"
shards = 4

[health_check]
interval = 30
timeout = 5
unhealthy_threshold = 3

[client_auth]
greeting = "200 NNTP proxy ready"

[[client_auth.users]]
username = "reader"
password = "reader-password"

[[servers]]
host = "news.example.com"
port = 563
name = "Primary"
username = "backend-user"
password = "backend-pass"
max_connections = 20
tier = 0
use_tls = true
tls_verify_cert = true
connection_keepalive = 60
```

### Cache vs Memory

`[cache]` controls article and availability storage:

- `store_article_bodies = false` is the default. The proxy tracks which backend has an article but does not keep full article bodies.
- `store_article_bodies = true` enables hot in-memory article-body caching.
- `[cache.disk]` is optional and stores article bodies evicted from the memory cache.
- `availability_index_path` persists availability-only routing knowledge across restarts.

`[memory]` controls transport memory, not article storage:

- TCP socket send/receive buffer sizes.
- Main pooled streaming buffer size and count.
- Capture buffer size and count for cache ingest and response assembly.

If you are tuning RAM use, check both sections: `[cache]` for stored article bodies and `[memory]` for transport/buffer pools.

### Backend Servers

Common server fields:

| Field | Required | Default | Notes |
| --- | --- | --- | --- |
| `host` | yes | - | Backend hostname or IP. |
| `port` | yes | - | `119` for plain NNTP, `563` for NNTPS. |
| `name` | yes | - | Friendly name used in logs and the TUI. |
| `username` | no | - | Backend auth username. |
| `password` | no | - | Backend auth password. |
| `max_connections` | no | 10 | Pool limit for this backend. |
| `tier` | no | 0 | Lower tiers are preferred; higher tiers get longer cache TTL. |
| `use_tls` | no | false | Enable TLS to this backend. |
| `tls_verify_cert` | no | true | Keep true in production. |
| `tls_cert_path` | no | - | Additional PEM CA certificate. |
| `connection_keepalive` | no | - | Send `DATE` on idle backend connections every N seconds. |
| `backend_pipelining` | no | true | Enable backend request pipelining. |

The proxy currently supports up to 8 backend servers because article availability uses a compact bitset.

### Tiering

Lower tiers are tried first. Higher tiers are fallbacks and get exponentially longer cache retention:

```toml
[[servers]]
host = "primary.example.com"
port = 119
name = "Primary"
tier = 0

[[servers]]
host = "archive.example.com"
port = 119
name = "Archive"
tier = 10
```

With a 1 hour base cache TTL:

| Tier | Effective TTL |
| --- | --- |
| 0 | 1 hour |
| 1 | 2 hours |
| 5 | 32 hours |
| 10 | about 43 days |

This keeps primary-server results fresh while avoiding repeated expensive lookups against backup or archive servers.

### TLS

TLS support is for outbound backend connections only. `nntp-proxy` does not provide a TLS listener for inbound client connections; it is meant to sit behind trusted-network boundaries such as a LAN, VPN, or segmented internal network.

For NNTPS backends:

```toml
[[servers]]
host = "secure.news.example.com"
port = 563
name = "Secure"
use_tls = true
tls_verify_cert = true
```

For private CAs, add `tls_cert_path`. The custom certificate is added to the system trust store, not used as a replacement.

Do not set `tls_verify_cert = false` in production.

### Health Checks

`[health_check]` controls proxy-level backend health probing:

| Field | Default | Notes |
| --- | --- | --- |
| `interval` | 30 | Seconds between health check cycles. |
| `timeout` | 5 | Seconds before a health check times out. |
| `unhealthy_threshold` | 3 | Consecutive failures before marking a backend unhealthy. |

Each server can also tune health-check pool behavior with `health_check_max_per_cycle` and `health_check_pool_timeout`, and can enable idle keep-alives with `connection_keepalive`.

### Client Auth

Client auth is optional. When configured, clients authenticate to the proxy; backend credentials remain per-server.

```toml
[client_auth]
greeting = "200 NNTP proxy ready"

[[client_auth.users]]
username = "reader"
password = "reader-password"
```

## Performance Notes

The current hot paths are designed to avoid avoidable heap work:

- RAM article-cache hits measured 5.16 GB/s on a Ryzen 9 5950X.
- With a 256 MB in-memory article cache and disk cache on SSD, the hot-cache path measured 2.58 GB/s on the same system.
- Backend request forwarding uses parsed request slices instead of rebuilding command strings.
- The main I/O and capture buffers are preallocated and prefaulted at startup.
- Buffer acquisition is allocation-free while the configured pools have capacity; exhaustion intentionally falls back to allocating and logs that fact.
- Article-cache lookup uses borrowed `&str` keys against `Arc<str>` cache keys, avoiding per-lookup key allocation.

This is not an unconditional "zero allocations everywhere" claim. TLS handshakes, connection setup, logging, metrics snapshots, pool exhaustion, and oversized capture buffers can allocate. The steady-state forwarding/cache-hit path is the part intentionally kept allocation-free where the configured pools are sized correctly.

## CLI

Common flags:

| Flag | Environment | Values / default | Meaning |
| --- | --- | --- | --- |
| `--config <FILE>` | `NNTP_PROXY_CONFIG` | Default: `config.toml` | Config file path. |
| `--ui <MODE>` | `NNTP_PROXY_UI` | `headless` or `tui`; default: `headless` | Selects how the single `nntp-proxy` binary runs: plain server logging or the terminal dashboard. |
| `--tui-listen <HOST:PORT>` | `NNTP_PROXY_TUI_LISTEN` | `HOST:PORT` | Bind the dashboard websocket publisher to a socket address in headless mode. |
| `--tui-attach <HOST:PORT>` | `NNTP_PROXY_TUI_ATTACH` | `HOST:PORT` | Connect the read-only TUI client to a dashboard websocket at a socket address. |
| `--host <HOST>` | `NNTP_PROXY_HOST` | Default from config; otherwise `0.0.0.0` | Override `[proxy].host`. |
| `--port <PORT>` | `NNTP_PROXY_PORT` | Default from config; otherwise `8119` | Override `[proxy].port`. |
| `--routing-mode <MODE>` | `NNTP_PROXY_ROUTING_MODE` | `hybrid`, `stateful`, or `per-command`; default from config | Override `[routing].mode`. |
| `--backend-selection <STRATEGY>` | `NNTP_PROXY_BACKEND_SELECTION` | `least-loaded` or `weighted-round-robin`; default from config | Override `[routing].backend_selection`. |
| `--article-cache-capacity <SIZE>` | `NNTP_PROXY_ARTICLE_CACHE_CAPACITY` | Example: `64mb`, `256mb`, `1gb`; default from config | Override `[cache].article_cache_capacity`. |
| `--article-cache-ttl <SECONDS>` | `NNTP_PROXY_ARTICLE_CACHE_TTL_SECS` | Integer seconds; default from config | Override `[cache].article_cache_ttl_secs`. |
| `--store-article-bodies <BOOL>` | `NNTP_PROXY_STORE_ARTICLE_BODIES` | `true` or `false`; default from config | Override `[cache].store_article_bodies`. |
| `--threads <N>` | `NNTP_PROXY_THREADS` | `0` for CPU cores; otherwise integer; default from config | Override `[proxy].threads`. |
| `--backend-pipelining <BOOL>` | `NNTP_PROXY_BACKEND_PIPELINING` | `true` or `false`; default from config | Override `backend_pipelining` for all configured servers. |

Examples:

```bash
nntp-proxy --config /etc/nntp-proxy/config.toml
nntp-proxy --ui tui --config config.toml
nntp-proxy --routing-mode stateful --port 119
nntp-proxy --store-article-bodies true --article-cache-capacity 256mb
```

CLI flags override config file values.

UI selection is separate from `--routing-mode`: routing mode controls backend session behavior, while UI options control whether `nntp-proxy` runs headless or with the dashboard.

`--no-tui` remains accepted as a hidden compatibility alias for headless mode, but `--ui headless` / `--ui tui` is the documented interface.

## Environment-Based Backend Configuration

Indexed `NNTP_SERVER_N_*` variables can configure backends without a TOML server list, which is useful in containers.

```bash
NNTP_SERVER_0_HOST=news.example.com \
NNTP_SERVER_0_PORT=119 \
NNTP_SERVER_0_NAME=Primary \
NNTP_SERVER_0_USERNAME=user \
NNTP_SERVER_0_PASSWORD=pass \
nntp-proxy
```

Server variables are contiguous: `NNTP_SERVER_0_HOST`, then `NNTP_SERVER_1_HOST`, and so on. Scanning stops at the first missing host index.

If a config file exists and `NNTP_SERVER_*` variables are set, the server list from the environment overrides the file's `[[servers]]` entries. Other config sections still come from the file plus CLI overrides.

## Docker

Build:

```bash
docker build -t nntp-proxy .
```

Run with environment variables:

```bash
docker run -d \
  --name nntp-proxy \
  -p 8119:8119 \
  -e NNTP_SERVER_0_HOST=news.example.com \
  -e NNTP_SERVER_0_PORT=119 \
  -e NNTP_SERVER_0_NAME=Primary \
  -e NNTP_SERVER_0_USERNAME="$BACKEND_USER" \
  -e NNTP_SERVER_0_PASSWORD="$BACKEND_PASS" \
  nntp-proxy
```

The repository also includes [docker-compose.yml](docker-compose.yml).

Do not hardcode provider credentials in compose files. Use environment substitution or secrets.

## Development

Use the included Nix shell if desired:

```bash
nix develop
```

Build the packaged binary with Nix:

```bash
nix build .#default
```

For NixOS, the flake exports `nixosModules.default`, which adds a
`services.nntp-proxy` module. It can either use an existing `config.toml`, or
render one from `services.nntp-proxy.settings`. A separate
`services.nntp-proxy.credentialsFile` can provide secret backend/client
passwords at runtime:

```nix
{
  inputs.nntp-proxy.url = "github:mjc/nntp-proxy";

  outputs = { nixpkgs, nntp-proxy, ... }: {
    nixosConfigurations.proxy = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        nntp-proxy.nixosModules.default
        {
          services.nntp-proxy = {
            enable = true;
            openFirewall = true;
            credentialsFile = "/var/lib/nntp-proxy/credentials.toml";
            settings = {
              proxy.port = 8119;
              routing.mode = "hybrid";
              servers = [
                {
                  host = "news.example.com";
                  port = 563;
                  name = "Primary";
                  use_tls = true;
                  tls_verify_cert = true;
                  max_connections = 20;
                }
              ];
            };
          };
        }
      ];
    };
  };
}
```

Example credentials overlay:

```toml
[[servers]]
name = "Primary"
username = "backend-user"
password = "backend-pass"

[[client_auth.users]]
username = "reader"
password = "reader-password"
```

Common checks:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features
cargo nextest run
```

Use `cargo test` for doctests, exact test filtering, or debugging with `-- --nocapture`.

Manual smoke test:

```bash
telnet localhost 8119
HELP
QUIT
```

## Troubleshooting

Port already in use:

```bash
lsof -i :8119
nntp-proxy --port 8120
```

Backend auth failures:

- Check `username` and `password` under the matching `[[servers]]` entry.
- Test direct connectivity to the provider.
- Look at logs for backend response codes.

Stateful commands rejected:

- You are probably using `routing.mode = "per-command"`.
- Use `routing.mode = "hybrid"` or `routing.mode = "stateful"` for normal newsreader behavior.

TLS failures:

- Keep `tls_verify_cert = true`.
- Check system CA certificates.
- Add `tls_cert_path` for private CA deployments.

Unexpected memory use:

- `[cache].store_article_bodies` and `[cache].article_cache_capacity` control article-body memory.
- `[memory]` controls socket buffers and buffer pools.
- Disk cache capacity is under `[cache.disk]` and should be backed by SSD/NVMe for best results.

## License

MIT
