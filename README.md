# nntp-proxy

NNTP proxy written in Rust.

What it gives you:

- lets many NNTP clients use many backends without you managing connection limits yourself
- `hybrid` mode by default, so stateless commands can stay efficient while stateful commands still work
- per-backend TLS and backend authentication
- optional article caching plus availability tracking
- a hot-cache path that can hit 2.58 GB/s with a 256 MB in-memory article cache and the disk tier on SSD
- metrics persistence for restarts
- a TUI binary for live inspection

## Binaries

- `nntp-proxy` runs the proxy server
- `nntp-proxy-tui` runs the proxy with a TUI dashboard

## Routing modes

- `hybrid` is the default
- `per-command` routes each command independently and is limited to stateless commands
- `stateful` keeps one backend connection per client session

In `per-command` mode, commands that require group context are rejected. That includes:

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

Commands using message IDs are supported in `per-command` mode.

## Configuration

The repo includes two config examples:

- [config.minimal.toml](config.minimal.toml)
- [config.example.toml](config.example.toml)

Main config sections:

- `[proxy]` for listen address, thread count, logging, and stats file path
- `[routing]` for routing mode, backend selection, and adaptive precheck
- `[memory]` for socket buffers and pooled buffer sizes/counts
- `[cache]` for article-cache capacity, TTL, storage mode, and availability persistence
- `[client_auth]` and `[[client_auth.users]]` for client authentication
- `[[servers]]` for backend servers

Common server fields:

- `host`
- `port`
- `name`
- `username`
- `password`
- `max_connections`
- `tier`
- `use_tls`
- `tls_verify_cert`
- `tls_cert_path`
- `connection_keepalive`
- `enable_pipelining`

Common proxy/cache settings:

- `host`
- `port`
- `threads`
- `validate_yenc`
- `log_file_level`
- `stats_file`
- `routing.mode`
- `routing.backend_selection`
- `routing.adaptive_precheck`
- `memory.socket_recv_buffer_size`
- `memory.socket_send_buffer_size`
- `memory.buffer_pool_size`
- `memory.buffer_pool_count`
- `memory.capture_pool_size`
- `memory.capture_pool_count`
- `cache.article_cache_capacity`
- `cache.article_cache_ttl_secs`
- `cache.store_article_bodies`
- `cache.availability_index_path`
- `cache.disk`

Command-line flags mirror the same ideas:

- `--routing-mode`
- `--backend-selection`
- `--article-cache-capacity`
- `--article-cache-ttl`
- `--store-article-bodies`
- `--backend-pipelining`

Command-line flags override config file values.

## Quick start

Build with Cargo:

```bash
cargo build --release
```

Run with a config file:

```bash
./target/release/nntp-proxy --config config.toml
```

Run the TUI binary:

```bash
cargo run --bin nntp-proxy-tui -- --config config.toml
```

## Nix

The repo includes a Nix flake.

```bash
nix develop
```

That enters the development shell with the Rust toolchain and supporting tools.

## Docker

The repository also includes:

- [Dockerfile](Dockerfile)
- [docker-compose.yml](docker-compose.yml)

## Testing

```bash
cargo test
```

Useful checks:

```bash
cargo fmt --check
cargo clippy --all-targets --all-features
```

## License

MIT
