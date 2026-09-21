# Getting Started

## What you run

`nntp-proxy` is a single application. It can run headless, with the built-in
terminal dashboard, or as an attached read-only dashboard client.

The local NNTP listener is plain TCP. TLS is used for outbound backend
connections; configure it per backend with `use_tls = true` rather than
expecting the local listener to terminate TLS.

## Install

With [Rust and Cargo](https://rustup.rs/) installed:

```bash
cargo install nntp-proxy --locked
```

Building version 0.6 requires Rust 1.91 or newer and a native C/C++ build
toolchain. Make sure Cargo's bin directory is on your `PATH`, then run
`nntp-proxy --help` to see the available options.

For builds from a repository checkout, follow the
[development guide](../development.md). The installed application does not
require devenv.

## Minimal config

Create `config.toml` with the following contents and replace the example host
and credentials with your provider's details:

```toml
[proxy]
host = "127.0.0.1"
port = 8119

[[servers]]
name = "Primary"
host = "news.example.com"
port = 563
use_tls = true
username = "your_username"
password = "your_password"
max_connections = 10
```

Remove both credential lines if the backend does not require authentication.
Set `max_connections` within your provider's allowance. For a fuller example,
see [config.full.toml](../../config.full.toml).

## Run the proxy

Headless:

```bash
nntp-proxy --config config.toml
```

With the built-in terminal dashboard:

```bash
nntp-proxy --tui --config config.toml
```

Connect your NNTP client to `127.0.0.1:8119` with TLS disabled. This example
requires no client credentials and listens only on your computer. To serve
other machines, configure `[proxy].host` and
[client authentication](configuration.md). The local listener uses plain NNTP.

## Attach the TUI from another terminal

If you want the proxy to stay headless but still expose the dashboard to another terminal:

1. Run the proxy headless and publish dashboard state on a loopback websocket:

```bash
nntp-proxy --config config.toml --tui-listen 127.0.0.1:8120
```

2. Attach from another terminal:

```bash
nntp-proxy --tui --tui-attach 127.0.0.1:8120
```

Keep the dashboard address on loopback and use a different port from the main NNTP listener.

## First-use checklist

1. Add at least one `[[servers]]` entry.
2. Add backend `username` and `password` if your provider requires auth.
3. Set `use_tls = true` and `port = 563` for NNTPS backends.
4. Leave `tls_verify_cert = true` unless you are debugging a private CA setup.

## Routing note

Hybrid is the default routing mode. `stateful` and `per-command` remain available as explicit modes when you need them.

The default cache configuration tracks authoritative article-missing (`430`)
responses for routing and retry decisions without retaining article bodies. Set
`[cache].store_article_bodies = true` only when the proxy should retain payloads;
add `[cache.disk]` for a memory-to-disk payload tier.

See [runtime-and-routing.md](runtime-and-routing.md) for details.
