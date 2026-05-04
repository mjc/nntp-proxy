# nntp-proxy

NNTP proxy written in Rust.

It supports:

- multiple backend servers
- three routing modes: `stateful`, `per-command`, and `hybrid`
- per-backend TLS
- client authentication
- optional article caching
- metrics persistence
- a terminal dashboard binary

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

- `[proxy]` for listen address, routing mode, backend selection, thread count, logging, and stats file path
- `[[servers]]` for backend servers
- `[cache]` for article caching and availability tracking
- `[client_auth]` or `[[client_auth.users]]` for client authentication

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
- `routing_mode`
- `backend_selection`
- `validate_yenc`
- `buffer_pool_count`
- `capture_pool_count`
- `log_file_level`
- `stats_file`
- `max_capacity`
- `ttl`
- `cache_articles`
- `adaptive_precheck`
- `availability_file`
- `disk`

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

## Repository layout

- [src/bin/nntp-proxy.rs](src/bin/nntp-proxy.rs)
- [src/bin/nntp-proxy-tui.rs](src/bin/nntp-proxy-tui.rs)
- [src/config/mod.rs](src/config/mod.rs)
- [src/session/mod.rs](src/session/mod.rs)
- [src/router/mod.rs](src/router/mod.rs)
- [src/cache/mod.rs](src/cache/mod.rs)

## License

MIT
