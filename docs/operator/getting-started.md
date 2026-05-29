# Getting Started

## What you run

The repository ships a single runtime binary, `nntp-proxy`. The same binary can run headless, with the built-in terminal dashboard, or as an attached read-only TUI client.

## Build

```bash
cargo build --release
```

The release binary will be at `./target/release/nntp-proxy`.

## Minimal config

Start from the smallest useful config:

```bash
cp config.minimal.toml config.toml
```

Then point the backend at a real NNTP server:

```toml
[[servers]]
host = "news.example.com"
port = 119
name = "Primary"
```

For a fuller example, see [../../config.full.toml](../../config.full.toml).

## Run the proxy

Headless:

```bash
./target/release/nntp-proxy --config config.toml
```

With the built-in terminal dashboard:

```bash
./target/release/nntp-proxy --tui --config config.toml
```

Connect your NNTP client to `localhost:8119` unless you changed `[proxy].port`.

## Attach the TUI from another terminal

If you want the proxy to stay headless but still expose the dashboard to another terminal:

1. Run the proxy headless and publish dashboard state on a loopback websocket:

```bash
./target/release/nntp-proxy --config config.toml --tui-listen 127.0.0.1:8120
```

1. Attach from another terminal:

```bash
./target/release/nntp-proxy --tui --tui-attach 127.0.0.1:8120
```

Keep the dashboard address on loopback and use a different port from the main NNTP listener.

## First-use checklist

1. Add at least one `[[servers]]` entry.
2. Add backend `username` and `password` if your provider requires auth.
3. Set `use_tls = true` and `port = 563` for NNTPS backends.
4. Leave `tls_verify_cert = true` unless you are debugging a private CA setup.

## Current routing note

Hybrid is the default routing mode. `stateful` and `per-command` remain available as explicit modes when you need them.

See [runtime-and-routing.md](runtime-and-routing.md) for details.
