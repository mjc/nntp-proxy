# NNTP Caching Proxy

A high-performance caching layer for NNTP proxy that sits between the regular proxy and real backend servers. This allows you to tune and test the main proxy without bandwidth limits from real-world backends.

## Architecture

```
Client → Main Proxy (tunable/testable) → Caching Proxy → Real Backend
```

The caching proxy:
- Acts as a backend to the main NNTP proxy
- Caches article content retrieved by message-ID
- Reduces bandwidth usage on real backends
- Enables realistic testing and tuning of the main proxy

## Features

- 🚀 **LRU Cache with TTL** - Uses Moka for high-performance async caching
- 📦 **Article Caching** - Caches ARTICLE, BODY, HEAD, STAT responses by message-ID
- ⏱️ **Configurable TTL** - Set cache expiration time per your needs
- 📊 **Cache Statistics** - Periodic logging of cache hit rates and size
- 🔄 **Full Proxy Features** - Inherits all standard proxy capabilities

## Quick Start

### 1. Build the Caching Proxy

```bash
cargo build --release --bin nntp-cache-proxy
```

### 2. Configure the Caching Proxy

Copy the example configuration:

```bash
cp cache-config.example.toml cache-config.toml
```

Edit `cache-config.toml` to point to your real backend servers:

```toml
[[servers]]
host = "your-real-news-server.com"
port = 119
name = "Real News Server"
username = "your_username"
password = "your_password"
max_connections = 10

[cache]
max_capacity = 10000  # Number of articles to cache
ttl_secs = 3600       # 1 hour cache lifetime
```

### 3. Start the Caching Proxy

```bash
./target/release/nntp-cache-proxy --port 8120 --config cache-config.toml
```

The caching proxy will listen on port 8120 by default.

### 4. Configure Main Proxy to Use Caching Proxy

Edit your main proxy's `config.toml`:

```toml
[[servers]]
host = "localhost"
port = 8120
name = "Caching Proxy Backend"
max_connections = 50  # Can be much higher since caching proxy is local
```

### 5. Start the Main Proxy

```bash
./target/release/nntp-proxy --port 8119 --config config.toml
```

Now clients connect to port 8119, which proxies through the caching layer on 8120.

## Configuration Options

### Command-Line Arguments

- `--port <PORT>` - Port to listen on (default: 8120)
- `--config <PATH>` - Configuration file path (default: cache-config.toml)
- `--threads <N>` - Number of worker threads (default: number of CPUs)
- `--cache-capacity <N>` - Cache capacity in articles (default: 10000)
- `--cache-ttl <SECONDS>` - Cache TTL in seconds (default: 3600)

### Configuration File

The caching proxy uses the same configuration format as the main proxy, with an additional `[cache]` section:

```toml
[cache]
max_capacity = 10000  # Maximum number of articles to cache
ttl_secs = 3600       # Time-to-live in seconds
```

## How It Works

### Caching Strategy

The caching proxy intercepts commands and caches responses for:

- `ARTICLE <message-id>` - Full article with headers and body
- `BODY <message-id>` - Article body only
- `HEAD <message-id>` - Article headers only
- `STAT <message-id>` - Article existence check

**Cache Key**: Message-ID (e.g., `<article123@example.com>`)

**Cache Hit**: Response served directly from cache, no backend query
**Cache Miss**: Query forwarded to backend, response cached for future requests

### Non-Cached Commands

All other NNTP commands (LIST, CAPABILITIES, etc.) are forwarded directly to the backend without caching.

### Cache Eviction

The cache uses LRU (Least Recently Used) eviction:
- When cache reaches `max_capacity`, oldest unused entries are evicted
- Entries automatically expire after `ttl_secs` seconds
- Both limits work together to manage memory usage

## Monitoring

The caching proxy logs cache statistics every 60 seconds:

```
Cache stats: entries=8523, size=156789
```

- `entries`: Number of cached articles
- `size`: Total cache memory usage (weighted)

## Use Cases

### 1. Testing and Development

Use the caching proxy to:
- Test main proxy configurations without hitting real backends
- Simulate high-volume scenarios with cached data
- Tune connection pools and buffer sizes

### 2. Bandwidth Optimization

For users with:
- Limited backend bandwidth
- Expensive metered connections
- High client request volumes for same articles

### 3. Performance Testing

- Establish a baseline with cached responses
- Measure proxy overhead without backend latency
- Test at scale without backend capacity limits

## Performance Considerations

### Memory Usage

Each cached article consumes memory. Estimate requirements:

- Average article size: ~50 KB
- Cache capacity: 10,000 articles
- Estimated memory: ~500 MB

Adjust `max_capacity` based on available RAM.

### CPU Usage

The caching proxy adds minimal CPU overhead:
- Cache lookups are O(1) with Moka
- No serialization/deserialization
- Direct byte buffer forwarding

### Network

Place the caching proxy on the same machine as the main proxy for best performance:
- Eliminates network latency for cache hits
- Reduces bandwidth between proxy layers
- Simplifies deployment

## Limitations

- **Stateless Only**: Only caches message-ID based retrievals (stateless commands)
- **Memory Bound**: Cache size limited by available RAM
- **No Persistence**: Cache is in-memory only, lost on restart
- **Single Instance**: No distributed caching support

## Example Deployment

### Development Setup

```bash
# Terminal 1: Start caching proxy
./target/release/nntp-cache-proxy --port 8120

# Terminal 2: Start main proxy pointing to caching proxy
./target/release/nntp-proxy --port 8119

# Terminal 3: Test with NNTP client
telnet localhost 8119
```

### Production Setup

Use systemd or similar to manage both processes:

```ini
# /etc/systemd/system/nntp-cache-proxy.service
[Unit]
Description=NNTP Caching Proxy
After=network.target

[Service]
Type=simple
ExecStart=/usr/local/bin/nntp-cache-proxy --port 8120 --config /etc/nntp/cache-config.toml
Restart=always

[Install]
WantedBy=multi-user.target
```

## Troubleshooting

### Cache Not Working

Check logs for "Cache HIT" and "Cache MISS" messages:

```
Cache MISS for message-ID: <article123@example.com>
Caching response for message-ID: <article123@example.com>
Cache HIT for message-ID: <article123@example.com>
```

### High Memory Usage

Reduce `max_capacity` in configuration or via `--cache-capacity` flag.

### Backend Connection Issues

The caching proxy uses standard proxy connection pooling. Check:
- Backend server is reachable
- Credentials are correct
- `max_connections` is appropriate

## Advanced Configuration

### Multiple Backend Servers

The caching proxy supports multiple backends with round-robin load balancing:

```toml
[[servers]]
host = "news1.example.com"
port = 119
name = "Backend 1"

[[servers]]
host = "news2.example.com"
port = 119
name = "Backend 2"
```

### Custom Cache TTL

Adjust cache lifetime based on your content:

- News articles (rarely change): `ttl_secs = 86400` (24 hours)
- Binary downloads (static): `ttl_secs = 604800` (7 days)
- Frequently updated: `ttl_secs = 1800` (30 minutes)

## License

Same as the main nntp-proxy project (MIT).
