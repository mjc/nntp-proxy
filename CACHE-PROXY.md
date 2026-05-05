# NNTP Proxy Caching Guide

Caching is built into the main `nntp-proxy` binary. There is no separate `nntp-cache-proxy` or `nntp-proxy-tui` runtime executable.

## Cache Modes

### Availability-only mode (default)

- `store_article_bodies = false`
- Tracks which backends have or do not have a message-ID
- Persists routing knowledge with `availability_index_path` if you want it to survive restarts
- Does **not** store article payloads

### Full article-body cache

- `store_article_bodies = true`
- Uses `article_cache_capacity` and `article_cache_ttl_secs` for the in-memory cache
- Can serve cached `ARTICLE`, `BODY`, `HEAD`, and `STAT` responses for message-ID lookups
- Can optionally spill evicted article bodies to `[cache.disk]`

## Quick Start

1. Copy the cache-focused example config:

```bash
cp cache-config.example.toml cache-config.toml
```

2. Point `[[servers]]` at your real backend servers.

3. Enable full article-body caching if you want cached payloads:

```toml
[cache]
article_cache_capacity = "256mb"
article_cache_ttl_secs = 3600
store_article_bodies = true

[cache.disk]
path = "/var/cache/nntp-proxy"
capacity = "10gb"
compression = "lz4"
shards = 4
```

4. Start the proxy:

```bash
./target/release/nntp-proxy --config cache-config.toml
```

To launch the built-in dashboard, use the same `nntp-proxy` binary with `--ui tui`.

## CLI Overrides

These flags work with `nntp-proxy` in either UI mode:

- `--article-cache-capacity <SIZE>`
- `--article-cache-ttl <SECONDS>`
- `--store-article-bodies <true|false>`

The environment variable equivalents are:

- `NNTP_PROXY_ARTICLE_CACHE_CAPACITY`
- `NNTP_PROXY_ARTICLE_CACHE_TTL_SECS`
- `NNTP_PROXY_STORE_ARTICLE_BODIES`

## What Gets Cached

The cache is for message-ID based article retrievals:

- `ARTICLE <message-id>`
- `BODY <message-id>`
- `HEAD <message-id>`
- `STAT <message-id>`

Other commands are forwarded to the backend normally.

## Disk Cache Notes

`[cache.disk]` is only useful when `store_article_bodies = true`. In availability-only mode, the relevant persistence knob is `availability_index_path`.

Recommended settings:

- Put `path` on SSD/NVMe storage
- Keep `compression = "lz4"` unless you specifically want better compression ratios
- Increase `article_cache_capacity` before reaching for a very large disk tier

## Monitoring

- `nntp-proxy` can show cache metrics live when launched with the dashboard enabled
- The proxy also logs periodic cache statistics
- `stats.json` persists metrics; `availability.idx` persists availability-only knowledge when enabled

## Choosing a Mode

Use availability-only mode when you mainly want smarter backend routing and lower RAM use.

Use full article-body caching when repeated message-ID reads are common and you want cache hits to avoid backend round-trips entirely.

## Limitations

- Caching is keyed by message-ID; article-number commands are not cacheable across backends
- Availability-only mode stores backend knowledge, not article bodies
- Disk cache stores article bodies, so it is not active in availability-only mode

## License

Same as the main nntp-proxy project (MIT).
