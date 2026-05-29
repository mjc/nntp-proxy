# nntp-proxy

High-throughput NNTP proxy written in Rust.

`nntp-proxy` sits between NNTP clients and one or more backend servers. It gives you a single local endpoint while handling backend selection, pooling, authentication, optional caching, and metrics in one place.

## What it does

- Shares multiple backend servers across multiple clients
- Pools and reuses backend connections instead of having every client open its own
- Supports backend auth, outbound TLS, health checks, and connection limits
- Tracks article availability and can optionally cache article bodies
- Runs as one binary: `nntp-proxy`

Client-facing connections are plain NNTP only. TLS support is for outbound backend connections, not for the local listener.

## Performance snapshot

Historical published Ryzen 9 results saw the `1/1/1` shape (1 client, 1 proxy thread, 1 backend connection) reach **4797.65 MiB/s** in the 10GiB matrix, while separate 100GiB spot checks measured **4712.73 MiB/s mean** over 10 runs. See [CHANGELOG.md](CHANGELOG.md) for the recorded numbers and [docs/development.md](docs/development.md) for the current benchmark workflow.

## Current status

Hybrid is the default routing mode. `per-command` and `stateful` remain available as explicit modes. Details are in [docs/operator/runtime-and-routing.md](docs/operator/runtime-and-routing.md).

## Quick start

```bash
cargo build --release
cp config.minimal.toml config.toml
./target/release/nntp-proxy --config config.toml
```

Then edit `config.toml` so `[[servers]]` points at a real backend and connect your NNTP client to `localhost:8119` unless you changed `[proxy].port`.

## Read next

- [Getting started](docs/operator/getting-started.md)
- [Configuration](docs/operator/configuration.md)
- [Caching](docs/operator/caching.md)
- [Operations](docs/operator/operations.md)
- [Development](docs/development.md)

## Example configs

- [config.minimal.toml](config.minimal.toml)
- [config.example.toml](config.example.toml)
- [config.full.toml](config.full.toml)
- [config.cache.toml](config.cache.toml)

## License

MIT
