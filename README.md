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

```bash
cargo install nntp-proxy --locked
```

Build this checkout with devenv:

```bash
devenv shell cargo build
```

## Configure and connect

Save this as `config.toml`, replacing the backend host and credentials with your provider's details:

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

The sample is NNTPS. Plain NNTP is port `119` with `use_tls = false`. Backend
certificate verification stays on unless you turn it off.

Remove both credential lines if your provider does not require authentication. Set `max_connections` within your provider's connection allowance.

Start the proxy:

```bash
nntp-proxy --config config.toml
```

If `config.toml` does not exist and `NNTP_SERVER_*` is unset, the proxy writes a placeholder for `news.example.com` port `119` with TLS off and listens on `0.0.0.0`. Edit that file before you connect a client.

Configure your NNTP client with:

- Host: `127.0.0.1`
- Port: `8119`
- TLS: disabled
- Username and password: none for this example

The proxy uses the backend credentials from `config.toml`.

This example accepts connections only from your computer. Keep `[proxy].host` explicit when adapting another config: its default is `0.0.0.0`, which listens on all IPv4 interfaces. [config.example.toml](config.example.toml) omits this field. Add `host = "127.0.0.1"` under `[proxy]` for local use.

To serve other machines, configure the listener address and [client authentication](docs/operator/configuration.md). The client listener supports plain NNTP only. TLS applies to backend connections. Use a trusted network or an encrypted tunnel for remote clients because client credentials and traffic are unencrypted. See [Operations](docs/operator/operations.md).

## Providers and routing

Add a `[[servers]]` entry for each backend, with its own credentials, TLS settings, and connection limit. Set `tier = 0` for primary servers and a higher tier for fallbacks.

For an article lookup, the proxy tries the next tier after every eligible server in the current tier has returned `430`. A remaining server in the current tier is tried first. A busy server, or a server that fails for that attempt, does not move the lookup to the next tier.

Hybrid routing is the default. Independent commands share backend connections, and a command that needs reader state, such as `GROUP`, switches that client to a dedicated backend. `stateful` dedicates a backend connection from the start. `per-command` is for stateless commands such as `ARTICLE <message-id>`. `per-command` rejects a command that needs reader state. Select either explicit mode with `--routing-mode <MODE>`. See [Runtime and routing](docs/operator/runtime-and-routing.md). Selection and queue settings are in [Configuration](docs/operator/configuration.md).

## Caching

By default, the proxy remembers article-missing (`430`) responses without storing article bodies. Misses are recorded by exact message ID and configured backend hostname. The same hostname shares misses across accounts, ports, TLS settings, tiers, and display names. Hostname matching ignores ASCII case and one trailing DNS dot. Different hostnames stay separate.

The miss index has a fixed capacity. A full index can replace an old entry. Once an entry is forgotten, the proxy can retry that provider.

To retain article payloads in memory, add this top-level section to `config.toml`:

```toml
[cache]
store_article_bodies = true
```

An optional `[cache.disk]` section writes article payloads to disk when they are inserted. It stays inactive unless `store_article_bodies = true`. See [Caching](docs/operator/caching.md) for capacity, expiration, and persistence.

## Dashboard and documentation

For the terminal dashboard, run:

```bash
nntp-proxy --config config.toml --tui
```

Without `--tui`, the server runs headless. The [runtime guide](docs/operator/runtime-and-routing.md) explains how to attach a read-only dashboard from another terminal over loopback.

- [Getting started](docs/operator/getting-started.md)
- [Configuration](docs/operator/configuration.md): settings, credentials, and environment variables
- [Operations](docs/operator/operations.md): Docker, persisted files, and troubleshooting
- [Caching](docs/operator/caching.md)
- [Example configuration](config.example.toml), [full configuration](config.full.toml), and [cache configuration](config.cache.toml)
- [Changelog](CHANGELOG.md)
- [Development guide](docs/development.md)

## License

[MIT](LICENSE)
