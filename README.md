# NNTP Proxy

A high-performance NNTP proxy server written in Rust, with round-robin load balancing and optional per-command routing.

## Key Features

- 🔄 **Round-robin load balancing** - Distributes connections across multiple backend servers
- ⚡ **High performance** - Lock-free routing, zero-allocation command parsing, optimized I/O
- 🏥 **Health checking** - Automatic backend health monitoring with failure detection
- 🔐 **Authentication** - Proxy-level authentication, backends pre-authenticated
- � **TLS/SSL support** - Secure backend connections with system certificate store
- �🔀 **Per-command routing mode** - Optional stateless routing for resource efficiency
- 📊 **Connection pooling** - Efficient connection reuse with configurable limits
- ⚙️ **TOML configuration** - Simple, readable configuration with sensible defaults
- 🔍 **Structured logging** - Detailed tracing for debugging and monitoring
- 🧩 **Modular architecture** - Clean separation of concerns, well-tested codebase

## Table of Contents

- [Overview](#overview)
- [Quick Start](#quick-start)
- [Configuration](#configuration)
- [Usage](#usage)
- [Architecture](#architecture)
- [Performance](#performance)
- [Limitations](#limitations)
- [Building](#building)
- [Testing](#testing)
- [License](#license)

## Overview

This NNTP proxy offers two operating modes:

1. **Standard mode** (default) - Full NNTP proxy with complete command support
2. **Per-command routing mode** (`--per-command-routing` or `-r`) - Stateless routing for resource efficiency

### Design Goals

- **Load balancing** - Distribute connections across multiple backend servers
- **Health monitoring** - Automatic detection and routing around unhealthy backends
- **High performance** - Lock-free routing, zero-allocation parsing, optimized I/O
- **Flexible deployment** - Choose between full compatibility or resource efficiency

### When to Use This Proxy

✅ **Standard mode - Good for:**
- Traditional newsreaders (tin, slrn, Thunderbird)
- Any NNTP client requiring stateful operations
- Load balancing with full protocol support
- Drop-in replacement for direct backend connections

✅ **Per-command routing mode - Good for:**
- Message-ID based article retrieval
- Indexing and search tools
- Metadata-heavy workloads
- Distributing load across multiple backends

❌ **Not suitable for:**
- Applications requiring custom NNTP extensions (unless in standard mode)
- Scenarios requiring true concurrent request processing (NNTP doesn't support this)

## Limitations

### Per-Command Routing Mode Restrictions

When running in **per-command routing mode** (`--per-command-routing` or `-r`), the proxy rejects stateful commands:

**Rejected in per-command routing mode:**
- Group navigation: `GROUP`, `NEXT`, `LAST`, `LISTGROUP`
- Article retrieval by number: `ARTICLE 123`, `HEAD 123`, `BODY 123`
- Overview commands: `XOVER`, `OVER`, `XHDR`, `HDR`

**Always supported:**
- ✅ Article by Message-ID: `ARTICLE <message-id@example.com>`
- ✅ Metadata: `LIST`, `HELP`, `DATE`, `CAPABILITIES`, `POST`
- ✅ Authentication: `AUTHINFO USER/PASS` (intercepted by proxy)

### Standard Mode (Default)

In **standard mode** (without `--per-command-routing`):
- ✅ **All NNTP commands are supported** - full bidirectional forwarding
- ✅ Compatible with traditional newsreaders (tin, slrn, Thunderbird)
- ✅ Stateful operations work normally (GROUP, NEXT, LAST, etc.)
- Each client gets a dedicated backend connection (1:1 mapping)

## Quick Start

### Prerequisites

- Rust 1.85+ (or use the included Nix flake)
- Optional: Nix with flakes for reproducible development environment

### Installation

```bash
# Clone the repository
git clone https://github.com/mjc/nntp-proxy.git
cd nntp-proxy

# Build release version
cargo build --release

# Binary will be in target/release/nntp-proxy
```

### Using Nix (Optional)

```bash
# Enter development environment
nix develop

# Or use direnv
direnv allow

# Build and run
cargo build
cargo run
```

### First Run

1. Create a configuration file (see [Configuration](#configuration) section)
2. Run the proxy:

```bash
./target/release/nntp-proxy --port 8119 --config config.toml
```

3. Connect with a client:

```bash
telnet localhost 8119
```

## Configuration

The proxy uses a TOML configuration file. Create `config.toml`:

```toml
# Backend servers (at least one required)
[[servers]]
host = "news.example.com"
port = 119
name = "Primary News Server"
username = "your_username"      # Optional
password = "your_password"      # Optional
max_connections = 20            # Optional, default: 10

[[servers]]
host = "news2.example.com"
port = 119
name = "Secondary News Server"
max_connections = 10

# Health check configuration (optional)
[health_check]
interval_secs = 30         # Seconds between checks (default: 30)
timeout_secs = 5           # Timeout per check (default: 5)
unhealthy_threshold = 3    # Failures before marking unhealthy (default: 3)
```

### Configuration Reference

#### Server Configuration

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `host` | string | Yes | - | Backend server hostname or IP |
| `port` | integer | Yes | - | Backend server port |
| `name` | string | Yes | - | Friendly name for logging |
| `username` | string | No | - | Authentication username |
| `password` | string | No | - | Authentication password |
| `max_connections` | integer | No | 10 | Max concurrent connections to this backend |
| `use_tls` | boolean | No | false | Enable TLS/SSL encryption |
| `tls_verify_cert` | boolean | No | true | Verify server certificates (uses system CA store) |
| `tls_cert_path` | string | No | - | Path to additional CA certificate (PEM format) |

### TLS/SSL Support

The proxy supports TLS/SSL encrypted connections to backend servers using your operating system's trusted certificate store by default.

#### Basic TLS Configuration

For servers with valid SSL certificates from recognized CAs:

```toml
[[servers]]
host = "secure.newsserver.com"
port = 563  # Standard NNTPS port
name = "Secure News Server"
use_tls = true
tls_verify_cert = true  # Uses system certificate store (default)
max_connections = 20
```

**That's it!** No additional certificate configuration needed. The proxy will:
- Use your operating system's trusted certificate store automatically
- Verify the server's certificate
- Establish a secure TLS connection

#### Private/Self-Signed CA

For servers using certificates from a private CA:

```toml
[[servers]]
host = "internal.newsserver.local"
port = 563
name = "Internal News Server"
use_tls = true
tls_verify_cert = true
tls_cert_path = "/etc/nntp-proxy/internal-ca.pem"  # PEM format
max_connections = 10
```

**Note**: The custom certificate is **added to** the system certificates, not replacing them.

#### System Certificate Stores

| Operating System | Certificate Store |
|-----------------|-------------------|
| Linux (Debian/Ubuntu) | `/etc/ssl/certs/ca-certificates.crt` |
| Linux (RHEL/CentOS) | `/etc/pki/tls/certs/ca-bundle.crt` |
| macOS | Security.framework (Keychain) |
| Windows | SChannel (Windows Certificate Store) |

#### Port Reference

| Port | Protocol | Description |
|------|----------|-------------|
| 119  | NNTP | Unencrypted, standard NNTP |
| 563  | NNTPS | NNTP over TLS/SSL (encrypted) |
| 8119 | Custom | Common alternative port |

#### Security Best Practices

✅ **Always verify certificates in production** (`tls_verify_cert = true`)  
✅ **Keep system certificates updated** via OS package manager  
✅ **Use standard NNTPS port 563** for encrypted connections  
✅ **Monitor TLS handshake failures** in logs  

⚠️ **Never set `tls_verify_cert = false` in production** - this disables all certificate verification!

#### Environment Variable Overrides for Servers

Backend servers can be configured entirely via environment variables, useful for Docker/container deployments. If any `NNTP_SERVER_N_HOST` variable is found, environment variables take precedence over the config file.

**Per-server variables (N = 0, 1, 2, ...):**

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `NNTP_SERVER_N_HOST` | Yes | - | Backend hostname/IP (presence triggers env mode) |
| `NNTP_SERVER_N_PORT` | No | 119 | Backend port |
| `NNTP_SERVER_N_NAME` | No | "Server N" | Friendly name for logging |
| `NNTP_SERVER_N_USERNAME` | No | - | Backend authentication username |
| `NNTP_SERVER_N_PASSWORD` | No | - | Backend authentication password |
| `NNTP_SERVER_N_MAX_CONNECTIONS` | No | 10 | Max concurrent connections |

**Example Docker deployment:**
```bash
docker run -e NNTP_SERVER_0_HOST=news.example.com \
           -e NNTP_SERVER_0_PORT=119 \
           -e NNTP_SERVER_0_NAME="Primary" \
           -e NNTP_SERVER_0_USERNAME=user \
           -e NNTP_SERVER_0_PASSWORD=pass \
           -e NNTP_SERVER_1_HOST=news2.example.com \
           -e NNTP_SERVER_1_PORT=119 \
           -e NNTP_PROXY_PORT=8119 \
           nntp-proxy
```

#### Health Check Configuration

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `interval_secs` | integer | 30 | Seconds between health checks |
| `timeout_secs` | integer | 5 | Health check timeout in seconds |
| `unhealthy_threshold` | integer | 3 | Consecutive failures before marking unhealthy |

### Authentication

The proxy handles authentication in two ways:

1. **Backend authentication** (when credentials are configured)
   - Configure `username` and `password` in server config
   - Proxy authenticates to backends during connection pool initialization
   - Connections are pre-authenticated, eliminating per-command overhead

2. **Client authentication**
   - Client `AUTHINFO USER/PASS` commands are always intercepted by the proxy
   - Returns success without forwarding to backend
   - No actual client credential validation (proxy trusts connected clients)
   - To restrict access, use firewall rules or network isolation

## Usage

### Command Line Options

```bash
nntp-proxy [OPTIONS]
```

| Option | Short | Environment Variable | Description | Default |
|--------|-------|---------------------|-------------|---------|
| `--port <PORT>` | `-p` | `NNTP_PROXY_PORT` | Listen port | 8119 |
| `--per-command-routing` | `-r` | `NNTP_PROXY_PER_COMMAND_ROUTING` | Enable per-command routing mode | false |
| `--config <FILE>` | `-c` | `NNTP_PROXY_CONFIG` | Config file path | config.toml |
| `--threads <NUM>` | `-t` | `NNTP_PROXY_THREADS` | Tokio worker threads | CPU cores |
| `--help` | `-h` | - | Show help | - |
| `--version` | `-V` | - | Show version | - |

**Note**: Environment variables take precedence over default values but are overridden by command-line arguments.

### Examples

```bash
# Standard mode with defaults
nntp-proxy

# Custom port and config
nntp-proxy --port 8120 --config production.toml

# Per-command routing mode (long form)
nntp-proxy --per-command-routing

# Per-command routing mode (short form)
nntp-proxy -r

# Single-threaded for debugging
nntp-proxy --threads 1

# Production setup
nntp-proxy --port 119 --config /etc/nntp-proxy/config.toml

# Using environment variables for configuration
NNTP_PROXY_PORT=8119 \
NNTP_PROXY_THREADS=4 \
NNTP_SERVER_0_HOST=news.example.com \
NNTP_SERVER_0_PORT=119 \
NNTP_SERVER_0_NAME="Primary" \
nntp-proxy

# Docker deployment with environment variables
docker run -d \
  -e NNTP_PROXY_PORT=119 \
  -e NNTP_SERVER_0_HOST=news.provider.com \
  -e NNTP_SERVER_0_USERNAME=myuser \
  -e NNTP_SERVER_0_PASSWORD=mypass \
  -e NNTP_SERVER_1_HOST=news2.provider.com \
  -p 119:119 \
  nntp-proxy
```

### Operating Modes

#### Standard Mode (default)

- One backend connection per client
- Simple 1:1 connection forwarding
- All NNTP commands supported
- Lower overhead, easier debugging

#### Per-Command Routing Mode (`-r` / `--per-command-routing`)

- Each command routed to next backend (round-robin)
- Commands processed serially (one at a time)
- Multiple clients share backend pool
- Health-aware routing
- Better resource distribution
- Stateful commands rejected

## Architecture

### Module Organization

The codebase is organized into focused modules with clear responsibilities:

| Module | Purpose |
|--------|---------|
| `auth/` | Client and backend authentication handling |
| `command/` | NNTP command parsing and classification |
| `config/` | Configuration loading and validation |
| `constants/` | Centralized configuration constants |
| `health/` | Backend health monitoring system |
| `pool/` | Connection and buffer pooling |
| `protocol/` | NNTP protocol constants and parsing |
| `router/` | Backend selection and load balancing |
| `session/` | Client session lifecycle management |
| `types/` | Core type definitions (IDs, etc.) |

### How It Works

#### Standard Mode Flow

```
Client Connection
    ↓
Select Backend (round-robin)
    ↓
Get Pooled Connection
    ↓
Pre-authenticated Connection
    ↓
Bidirectional Data Forwarding
    ↓
Connection Cleanup
```

#### Per-Command Routing Mode Flow

```
Client Connection
    ↓
Read Command
    ↓
Classify Command
    ↓
Route to Healthy Backend (round-robin)
    ↓
Execute on Backend Connection (BLOCKS)
    ↓
Stream Response to Client
    ↓
Repeat (commands processed serially)
```

### Key Design Decisions

1. **Serial Processing**
   - NNTP processes one command at a time
   - Each command blocks until response received
   - No concurrent request handling possible
   - Round-robin distributes load across backends

2. **Connection Pooling**
   - Pre-authenticated connections
   - Reduces setup overhead
   - Configurable pool sizes per backend

3. **Health Checking**
   - Periodic DATE command probes
   - Automatic failure detection
   - Router skips unhealthy backends

4. **Lock-Free Routing**
   - Atomic operations for pending counts
   - Eliminates RwLock contention
   - Significant CPU reduction with many clients

## Performance

### Optimizations

This proxy implements several performance optimizations:

| Optimization | Impact | Description |
|--------------|--------|-------------|
| Zero-allocation parsing | -0.92% CPU | Direct byte comparison, no `to_ascii_uppercase()` |
| Lock-free routing | -10-15% CPU | Atomic operations instead of RwLock |
| Pre-authenticated connections | High | No per-command auth overhead |
| Buffer reuse | ~200+ allocs/sec saved | Pre-allocated buffers in hot paths |
| Frequency-ordered matching | Better branch prediction | Common commands (ARTICLE, BODY) checked first |
| 64KB read buffers | Fewer syscalls | Optimized for large article transfers |

### Performance Characteristics

- **CPU Usage**: Low overhead with lock-free routing and zero-allocation parsing
  - Per-command routing mode: ~15% of one core for 80 connections at 105MB/s (AMD Ryzen 9 5950X, single-threaded configuration)
- **Memory**: Constant usage regardless of article size (no response buffering)
- **Throughput**: Typically limited by backend servers, not the proxy
- **Scalability**: Efficiently handles hundreds of concurrent connections

### Profiling

To generate a performance flamegraph for analysis:

```bash
# Install cargo-flamegraph (if using Nix, it's already available)
cargo install flamegraph

# Run with flamegraph profiling (per-command routing mode)
cargo flamegraph --bin nntp-proxy -- --config config.toml -r --threads 1

# Open flamegraph.svg in a browser to analyze CPU hotspots
```

## Building

### Development Build

```bash
cargo build
./target/debug/nntp-proxy
```

### Release Build

```bash
cargo build --release
./target/release/nntp-proxy
```

### Production Deployment

```bash
# Build optimized binary
cargo build --release

# Copy binary to deployment location
sudo cp target/release/nntp-proxy /usr/local/bin/

# Create config directory
sudo mkdir -p /etc/nntp-proxy

# Copy config
sudo cp config.toml /etc/nntp-proxy/

# Run as service (example systemd unit included)
sudo systemctl start nntp-proxy
```

### Static Binary (Optional)

For maximum portability, build a fully static binary:

```bash
# Install musl target
rustup target add x86_64-unknown-linux-musl

# Build static binary
cargo build --release --target x86_64-unknown-linux-musl

# Result is a static binary with no dependencies
./target/x86_64-unknown-linux-musl/release/nntp-proxy
```

## Testing

### Running Tests

```bash
# All tests
cargo test

# Unit tests only
cargo test --lib

# Integration tests only
cargo test --test integration_tests

# With output
cargo test -- --nocapture

# Quiet mode
cargo test --quiet
```

### Test Coverage

The codebase includes:
- **165 unit tests** covering all modules
- **Integration tests** for end-to-end scenarios
- **100% pass rate**

### Manual Testing

Test with telnet or netcat:

```bash
# Connect to proxy
telnet localhost 8119

# Should see greeting like:
# 200 news.example.com ready

# Try commands:
HELP
LIST ACTIVE
ARTICLE <message-id@example.com>
QUIT
```

### Load Testing

For performance testing, create custom scripts that:
- Open multiple concurrent NNTP connections
- Issue realistic command sequences
- Measure throughput and latency
- Monitor CPU and memory usage

## Dependencies

### Core Dependencies

| Crate | Purpose |
|-------|---------|
| `tokio` | Async runtime and networking |
| `tokio-native-tls` | TLS/SSL support for async streams |
| `native-tls` | Native TLS backend (uses system certificate store) |
| `tracing` | Structured logging framework |
| `anyhow` | Error handling |
| `clap` | Command-line argument parsing |
| `serde` | Serialization framework |
| `toml` | TOML configuration parsing |
| `deadpool` | Connection pooling |

### Development Dependencies

- `tempfile` - Temporary files for testing
- Test helpers included in `tests/test_helpers.rs`

## Troubleshooting

### Common Issues

**"Connection refused" when starting**
- Check if port is already in use: `lsof -i :8119`
- Try a different port: `--port 8120`

**"Backend authentication failed"**
- Verify credentials in config.toml
- Test direct connection to backend
- Check backend server logs

**"Command not supported" errors**
- In per-command routing mode, stateful commands are rejected (GROUP, NEXT, etc.)
- Use message-ID based retrieval instead
- For stateful operations, use standard mode or connect directly to backend

**High CPU usage**
- Try per-command routing mode: `-r` or `--per-command-routing`
- Reduce worker threads: `--threads 1`
- Check health check interval (increase if too frequent)

**Backends marked unhealthy**
- Check backend server status
- Verify network connectivity
- Review health check configuration
- Check logs for specific errors

### Logging

Control log verbosity with `RUST_LOG`:

```bash
# Info level (default)
RUST_LOG=info nntp-proxy

# Debug level
RUST_LOG=debug nntp-proxy

# Specific module
RUST_LOG=nntp_proxy::router=debug nntp-proxy

# Multiple modules
RUST_LOG=nntp_proxy::router=debug,nntp_proxy::health=debug nntp-proxy
```

## Roadmap

### Planned Features

- [ ] Prometheus metrics endpoint
- [ ] Configuration hot-reload
- [ ] IPv6 support
- [ ] Connection affinity mode
- [ ] Admin/stats HTTP endpoint

### Completed

- [x] SSL/TLS support (NNTPS) with system certificate store
- [x] Lock-free routing
- [x] Zero-allocation command parsing
- [x] Health checking system
- [x] Per-command routing mode
- [x] Pre-authenticated connections
- [x] TOML configuration
- [x] Connection pooling
- [x] Renamed terminology from "multiplexing" to "per-command routing"

## Contributing

Contributions welcome! Please:

1. Fork the repository
2. Create a feature branch
3. Make your changes with tests
4. Ensure all tests pass: `cargo test`
5. Submit a pull request

## License

MIT License - see LICENSE file for details.

## Acknowledgments

Built with Rust and the excellent Tokio async ecosystem.
