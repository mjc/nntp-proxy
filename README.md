# nntp-proxy

High-throughput NNTP proxy and pooled NNTP client written in Rust.

`nntp-proxy` sits between NNTP clients and one or more backend servers. It gives you a single local endpoint while handling backend selection, pooling, authentication, optional caching, and metrics in one place.

The crate also includes a standalone pooled client for applications that need to
fetch and validate article responses without running the proxy listener.

## Requirements

- Rust 1.91 or newer when building the crate directly.
- [devenv](https://devenv.sh/getting-started/) for the repository's pinned
  development tools and reproducible checks.

## What it does

- Shares multiple backend servers across multiple clients
- Pools and reuses backend connections instead of having every client open its own
- Supports backend authentication, outbound TLS, health checks, and connection limits
- Tracks authoritative article-missing responses and can optionally cache article bodies
- Runs as one binary: `nntp-proxy`

Client-facing connections are plain NNTP only. TLS support is for outbound backend connections, not for the local listener.

## Routing and caching

Hybrid routing is the default. It uses per-command routing for commands that do
not need reader state and hands the session to a dedicated backend connection
when stateful protocol context is required. `per-command` keeps every command
on the stateless path, while `stateful` assigns each client a dedicated backend
connection. A per-command session returns an NNTP error for commands that
require state instead of silently routing them with the wrong context.

Routing mode does not change NNTP response semantics. Response shape is derived
from the request that produced it, so a status code is never treated as
multiline in isolation. Packed backend responses remain associated with the
correct request and are consumed in order.

With the default cache settings, the proxy records authoritative `430` article
misses for routing and retry decisions without retaining article bodies. Set
`[cache].store_article_bodies = true` to enable payload caching; adding
`[cache.disk]` provides a memory-to-disk cache tier. The canonical configuration
names are documented in [Configuration](docs/operator/configuration.md).

Details about routing restrictions, availability namespaces, queue backpressure,
and cache behavior are in [Runtime and routing](docs/operator/runtime-and-routing.md)
and [Caching](docs/operator/caching.md).

## Quick start

```bash
devenv shell cargo build --release
cp config.minimal.toml config.toml
./target/release/nntp-proxy --config config.toml
```

Install [devenv](https://devenv.sh/getting-started/). For automatic activation,
install its native shell hook (for zsh, add `eval "$(devenv hook zsh)"` to
`~/.zshrc`) and run `devenv allow` once in the repository root. You can always
use `devenv shell <command>` without the hook. Packaging and cross-platform
release commands continue to use Nix; see [Development](docs/development.md).

Then edit `config.toml` so `[[servers]]` points at a real backend and connect your NNTP client to `localhost:8119` unless you changed `[proxy].port`.

The binary starts in headless mode and writes logs to stdout. Use `--ui tui` (or
`--tui`) for the local dashboard. For a separate dashboard process, run the
server with `--ui headless --tui-listen 127.0.0.1:PORT` and attach with
`--ui tui --tui-attach 127.0.0.1:PORT`; dashboard listeners and attachments are
restricted to loopback addresses.

## Library API

The crate exposes the proxy builder and a standalone pooled client through the
Rust library API. `NntpClient` fetches article-family responses into pooled
storage and keeps response framing separate from semantic article validation:

```rust,no_run
use nntp_proxy::client::NntpClient;
use nntp_proxy::protocol::YencValidation;

# async fn example(client: &NntpClient, message_id: &nntp_proxy::types::MessageId<'_>)
#     -> Result<(), Box<dyn std::error::Error>> {
let framed = client.fetch_article(message_id).await?;
let validated = framed.validate_with_yenc(YencValidation::Disabled)?;
let article = validated.article();
println!("{}", article.message_id);
# Ok(())
# }
```

The transition has three deliberately separate guarantees:

1. `FramedArticle` owns the complete response bytes after the response boundary
   has been established. The status line and content are retained; the NNTP
   multiline terminator is not.
2. `ValidatedArticle` consumes that owner, validates the request-scoped article
   shape, and retains the validated layout and transformation requirements.
3. `ValidatedArticle::article()` returns a reusable borrowed `ArticleView`.
   Repeated access does not reparse or rediscover plain headers and bodies.

Use `fetch_body` or `fetch_head` when the request only needs one article
section, and `stat` when you only need an existence check. Use
`YencValidation::Enabled` only when the application needs yEnc structure
validated as part of the transition; NNTP framing and article validation remain
separate from yEnc decoding.

The lower-level `Article::parse` API remains available when an application
already owns complete framed bytes and does not need the pooled client.

The proxy's ordinary `ARTICLE`, `BODY`, and `HEAD` forwarding path is separate
from this retained client API: it borrows bytes from pooled read storage and
does not allocate a complete article merely to forward it.

## Performance and development

Release benchmark evidence is recorded with its exact commit and workload in
[CHANGELOG.md](CHANGELOG.md) and the archived benchmark notes when a release
benchmark has been run. Those measurements describe the listed revisions, not
a guarantee for every deployment. Follow
[Development](docs/development.md) for reproducible tests, documentation
checks, microbenchmarks, Gungraun measurements, and end-to-end benchmark
commands. The repository development environment is managed by
[devenv](https://devenv.sh/); use `devenv shell <command>` or activate it with
`devenv allow`.

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
