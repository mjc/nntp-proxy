# nntp-proxy

`nntp-proxy` gives NNTP clients one endpoint for multiple backend servers. It
pools backend connections, routes requests, and optionally caches articles in
memory or on disk. A terminal dashboard shows throughput, connections, cache
activity, and logs.

- Reuse authenticated connections across clients, with a limit for each server.
- Distribute requests across providers and set priority tiers for article retries.
- Remember which providers have reported an article missing to avoid repeated misses.
- Share backend connections until a command needs a dedicated reader session.

## Install

With [Rust and Cargo](https://rustup.rs/) installed:

```bash
cargo install nntp-proxy --locked
```

Building version 0.6 requires Rust 1.95 or newer and a native C/C++ build
toolchain. Cargo installs the `nntp-proxy` executable into its bin directory;
make sure that directory is on your `PATH`. See
[Cargo's installation guide](https://doc.rust-lang.org/cargo/commands/cargo-install.html)
if your shell cannot find the command.

## Configure and run

Save this as `config.toml`, replacing the example backend and credentials with
those from your provider:

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

Remove both credential lines if your backend does not require authentication.
Set `max_connections` to fit your provider's connection allowance.

Start the proxy:

```bash
nntp-proxy --config config.toml
```

In your NNTP client, use **host `127.0.0.1`, port `8119`, with TLS disabled**.
No client username or password is needed for this local example. The proxy
uses your configured credentials and TLS when connecting to the backend;
backend certificate verification is enabled by default.

This example listens only on your computer. To serve other machines, configure
the listener address and [client authentication](https://github.com/mjc/nntp-proxy/blob/main/docs/operator/configuration.md).
Use a trusted network or an encrypted tunnel for remote clients: the plain
NNTP listener exposes credentials and traffic on an untrusted network. TLS
support applies to backend connections. See [Operations](https://github.com/mjc/nntp-proxy/blob/main/docs/operator/operations.md)
for deployment guidance.

## Dashboard

Run the proxy with its terminal dashboard:

```bash
nntp-proxy --config config.toml --tui
```

Or keep the server headless and attach a dashboard from another terminal:

```bash
# Server terminal
nntp-proxy --config config.toml --tui-listen 127.0.0.1:8120

# Dashboard terminal
nntp-proxy --tui --tui-attach 127.0.0.1:8120
```

Dashboard connections are restricted to loopback. Without `--tui`, the server
runs headless and writes logs to stdout. Use `nntp-proxy --help` for the full
command-line reference.

## Backends and routing

Add another `[[servers]]` entry for each upstream server. Set `tier = 0` for
primary servers and a higher tier for fallback servers. For article-missing
retries, the proxy tries eligible servers in the current tier before moving
to the next one. Connection limits and TLS settings are configured per server.

Hybrid routing is the default: independent commands share backend connections,
and commands that need reader state switch the session to a dedicated backend.
Choose `stateful` to dedicate a backend connection from the start, or
`per-command` for stateless workloads. Per-command mode returns an NNTP error
for commands that require session state.

See [Runtime and routing](https://github.com/mjc/nntp-proxy/blob/main/docs/operator/runtime-and-routing.md)
for backend selection, priority tiers, and queue backpressure.

## Caching

By default, the proxy remembers `430` responses by exact message ID and
canonical provider hostname without storing article bodies. This index has a
fixed capacity. Entries can expire, rotate out, or be replaced; the proxy
retries a provider once it has forgotten the earlier miss.
To retain payloads in memory, add this as a top-level section in `config.toml`:

```toml
[cache]
store_article_bodies = true
```

An optional `[cache.disk]` section adds a memory-to-disk payload cache. See
[Caching](https://github.com/mjc/nntp-proxy/blob/main/docs/operator/caching.md)
for capacity, expiration, disk storage, and availability tracking settings.

## Documentation

- [Getting started](https://github.com/mjc/nntp-proxy/blob/main/docs/operator/getting-started.md)
  — installation, configuration, and the first connection.
- [Configuration](https://github.com/mjc/nntp-proxy/blob/main/docs/operator/configuration.md)
  — settings, authentication, and environment variables.
- [Operations](https://github.com/mjc/nntp-proxy/blob/main/docs/operator/operations.md)
  — running and monitoring the proxy.
- [Example configuration](https://github.com/mjc/nntp-proxy/blob/main/config.example.toml)
  — primary and fallback backends.
- [Full configuration](https://github.com/mjc/nntp-proxy/blob/main/config.full.toml)
  and [cache configuration](https://github.com/mjc/nntp-proxy/blob/main/config.cache.toml).
- [Changelog](https://github.com/mjc/nntp-proxy/blob/main/CHANGELOG.md)
  — release changes.

## Contributing

Repository development uses [devenv](https://devenv.sh/). See the
[development guide](https://github.com/mjc/nntp-proxy/blob/main/docs/development.md)
for source builds, tests, profiling, and benchmarks. Use the application for
normal operation; the Rust library API is still evolving.

## License

[MIT](https://github.com/mjc/nntp-proxy/blob/main/LICENSE)
