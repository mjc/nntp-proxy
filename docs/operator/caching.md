# Caching

Caching is built into `nntp-proxy`. There is no separate cache-only runtime binary.

`[cache]` is optional. If you omit it entirely, the runtime still uses internal availability tracking, but you do not get configured cache TTL/capacity settings or an availability index persistence path.

## Cache modes

### Availability-only mode (default)

```toml
[cache]
store_article_bodies = false
```

This mode:

- keeps backend availability tracking enabled
- stores routing/retry knowledge in the availability index
- does **not** retain article bodies
- can persist the availability index with `availability_index_path`
- leaves `[cache.disk]` inactive even if it is configured

### Full article-body caching

```toml
[cache]
article_cache_capacity = "256mb"
article_cache_ttl_secs = 3600
store_article_bodies = true
```

This mode:

- keeps availability tracking enabled
- stores retained payloads in memory
- stores `ARTICLE` and `BODY` payloads
- can serve cached `ARTICLE` and `BODY` responses directly
- can synthesize cached `HEAD` and `STAT` responses from retained entries when the needed payload is available
- can add an optional disk tier with `[cache.disk]`

## Important behavior

`store_article_bodies` only controls whether payloads are retained. Availability tracking is still part of the cache behavior either way.

Cached article entries store typed payload sections, not raw backend response
bytes. Cache-hit responses are rendered back to NNTP wire format from the stored
status, article number, message ID, headers, and body. Multiline cache-hit
completion is shared with the session framer so cache code does not duplicate
response-boundary logic.

## What gets retained or tracked

Caching behavior is for message-ID based article retrievals:

| Request | Success response | Availability-only mode | Full body-cache mode |
| --- | --- | --- | --- |
| `ARTICLE <message-id>` | `220` | Track availability | Retain full article |
| `BODY <message-id>` | `222` | Track availability | Retain body payload |
| `HEAD <message-id>` | `221` | Track availability | Track availability |
| `STAT <message-id>` | `223` | Track availability | Track availability / synthetic cache hit metadata |

Group-context and article-number commands are not cacheable across backends in the same way.

`430` responses are retained as authoritative negative availability for the
backend that returned them. They do not contain a multiline body and are used to
avoid retrying a backend already known not to have that message ID until the
availability entry expires.

## Disk cache

Use `[cache.disk]` only when `store_article_bodies = true`.

```toml
[cache.disk]
path = "/var/cache/nntp-proxy"
capacity = "10gb"
compression = "lz4"
shards = 4
```

Practical guidance:

- Use SSD/NVMe storage
- Keep `compression = "lz4"` unless you have a reason to prefer ratio over CPU
- Increase memory cache first before adding a very large disk tier

## Availability index persistence

In availability-only mode, set `availability_index_path` if you want backend availability knowledge to survive restarts:

```toml
[cache]
store_article_bodies = false
availability_index_path = "/var/cache/nntp-proxy/availability.idx"
```

If `[cache]` is configured and `availability_index_path` is unset, the runtime defaults to `availability.idx` next to the config file.

If `[cache]` is omitted entirely, the proxy still uses internal availability tracking but does not resolve an availability persistence path from config.

## CLI overrides

These flags override cache config:

- `--article-cache-capacity`
- `--article-cache-ttl`
- `--store-article-bodies`

Environment equivalents:

- `NNTP_PROXY_ARTICLE_CACHE_CAPACITY`
- `NNTP_PROXY_ARTICLE_CACHE_TTL_SECS`
- `NNTP_PROXY_STORE_ARTICLE_BODIES`
