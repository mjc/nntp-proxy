# NNTP Proxy

A high-performance NNTP proxy server written in Rust, with intelligent hybrid routing, round-robin load balancing, and TLS support.

## Key Features

- 🧠 **Hybrid routing mode** - Intelligent per-command routing that auto-switches to stateful when needed (default)
- 🔄 **Round-robin load balancing** - Distributes connections across multiple backend servers
- 🔐 **TLS/SSL support** - Secure backend connections using rustls with system certificate store
- ⚡ **High performance** - Lock-free routing, optimized response parsing, efficient I/O
- 🏥 **Health checking** - Automatic backend health monitoring with failure detection
- 📊 **Connection pooling** - Pre-authenticated connections with configurable limits and reservation
- 🛡️ **Type-safe protocol handling** - RFC 3977 compliant parsing with comprehensive validation
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

This NNTP proxy offers three operating modes:

1. **Hybrid mode** (default) - Starts with per-command routing, automatically switches to stateful when needed
2. **Standard mode** (`--routing-mode standard`) - Full NNTP proxy with complete command support
3. **Per-command routing mode** (`--routing-mode per-command`) - Pure stateless routing for maximum efficiency

### Design Goals

- **Load balancing** - Distribute connections across multiple backend servers with health-aware routing
- **Performance** - Lock-free routing, optimized protocol parsing, efficient I/O with connection pooling
- **Security** - TLS/SSL support with certificate verification, pre-authenticated backend connections
- **Reliability** - Health monitoring, automatic failover, graceful connection handling
- **Flexibility** - Choose between full NNTP compatibility or resource-efficient per-command routing

### When to Use This Proxy

✅ **Hybrid mode (default) - Best for:**
- **Universal compatibility** - Works with any NNTP client automatically
- **Optimal performance** - Efficient per-command routing until stateful operations needed
- **Intelligent switching** - Automatically detects when clients need stateful mode (GROUP, XOVER, etc.)
- **Resource efficiency** - Uses per-command routing when possible, stateful only when necessary
- **Most deployments** - Recommended default that adapts to client behavior

✅ **Standard mode - Good for:**
- Traditional newsreaders requiring guaranteed stateful behavior
- Debugging or when you need predictable 1:1 connection mapping
- Legacy deployments where hybrid mode is not desired
- Maximum compatibility and simplicity

✅ **Per-command routing mode - Good for:**
- Message-ID based article retrieval workloads only
- Indexing and search tools that only need ARTICLE/BODY/HEAD by message-ID
- Specialized deployments where stateful operations are never needed
- Maximum resource efficiency when you control all clients

❌ **Not suitable for:**
- Scenarios requiring concurrent request processing (NNTP is inherently serial)
- Custom NNTP extensions not in RFC 3977 (unless in standard mode with compatible backend)

## Limitations

### Per-Command Routing Mode Restrictions

When running in **per-command routing mode** (`--per-command-routing` or `-r`), the proxy rejects stateful commands to maintain consistent routing:

**Rejected commands (require group context):**
- Group navigation: `GROUP`, `NEXT`, `LAST`, `LISTGROUP`
- Article by number: `ARTICLE 123`, `HEAD 123`, `BODY 123`, `STAT 123`
- Overview commands: `XOVER`, `OVER`, `XHDR`, `HDR`

**Always supported:**
- ✅ Article by Message-ID: `ARTICLE <message-id@example.com>`
- ✅ Metadata retrieval: `LIST`, `HELP`, `DATE`, `CAPABILITIES`
- ✅ Posting: `POST` (if backend supports)
- ✅ Authentication: `AUTHINFO USER/PASS` (handled by proxy)

**Rationale:** Commands requiring group context (current article number, group selection) cannot work reliably when each command routes to a different backend. Use standard mode or hybrid mode if you need these features.

### Hybrid Mode Advantage (Default)

**Hybrid mode** automatically handles the per-command routing limitations:
- ✅ **Starts efficiently** - Uses per-command routing for stateless operations (ARTICLE by message-ID, LIST, etc.)
- ✅ **Switches intelligently** - Detects stateful commands (GROUP, XOVER, etc.) and seamlessly switches to dedicated backend
- ✅ **Universal compatibility** - Works with any NNTP client without configuration
- ✅ **Resource efficient** - Uses shared pool when possible, dedicated connections only when needed
- ✅ **Best of both worlds** - Combines per-command efficiency with full protocol support

### Standard Mode

In **standard mode** (`--routing-mode standard`):
- ✅ **All RFC 3977 commands supported** - full bidirectional forwarding
- ✅ Compatible with all NNTP clients
- ✅ Stateful operations work normally (GROUP, NEXT, LAST, XOVER, etc.)
- ✅ Each client connection maps to one backend connection (1:1)
- ✅ Simple, predictable behavior

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

The proxy supports TLS/SSL encrypted connections to backend servers using **rustls** - a modern, memory-safe TLS implementation written in pure Rust.

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
- Use rustls with your operating system's trusted certificate store
- Verify the server's certificate against system CAs
- Establish a secure TLS 1.3 connection (with TLS 1.2 fallback)
- Support session resumption for improved performance

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
✅ **Use TLS 1.3 when possible** (automatically negotiated by rustls)  
✅ **Use standard NNTPS port 563** for encrypted connections  
✅ **Monitor TLS handshake failures** in logs  

⚠️ **Never set `tls_verify_cert = false` in production** - this disables all certificate verification and is extremely insecure!

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

The proxy handles authentication transparently:

1. **Backend authentication** (when credentials are configured)
   - Configure `username` and `password` in server configuration
   - Proxy authenticates to backends during connection pool initialization
   - Connections remain pre-authenticated, eliminating per-command auth overhead
   - Credentials are used with RFC 4643 AUTHINFO USER/PASS commands

2. **Client authentication handling**
   - Client `AUTHINFO USER/PASS` commands are intercepted by the proxy
   - Proxy responds with success (281/381) without forwarding to backend
   - No actual credential validation performed (network-level access control recommended)
   - To restrict access, use firewall rules, VPN, or network segmentation

## Usage

### Command Line Options

```bash
nntp-proxy [OPTIONS]
```

| Option | Short | Environment Variable | Description | Default |
|--------|-------|---------------------|-------------|---------|
| `--port <PORT>` | `-p` | `NNTP_PROXY_PORT` | Listen port | 8119 |
| `--routing-mode <MODE>` | `-r` | `NNTP_PROXY_ROUTING_MODE` | Routing mode: hybrid, standard, per-command | hybrid |
| `--config <FILE>` | `-c` | `NNTP_PROXY_CONFIG` | Config file path | config.toml |
| `--threads <NUM>` | `-t` | `NNTP_PROXY_THREADS` | Tokio worker threads | CPU cores |
| `--help` | `-h` | - | Show help | - |
| `--version` | `-V` | - | Show version | - |

**Note**: Environment variables take precedence over default values but are overridden by command-line arguments.

### Examples

```bash
# Hybrid mode with defaults (recommended)
nntp-proxy

# Custom port and config (still uses hybrid mode)
nntp-proxy --port 8120 --config production.toml

# Standard mode (full stateful behavior)
nntp-proxy --routing-mode standard

# Per-command routing mode (pure stateless)
nntp-proxy --routing-mode per-command

# Short form for routing modes
nntp-proxy -r standard
nntp-proxy -r per-command

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

#### Hybrid Mode (default) - `--routing-mode hybrid`

- **Intelligent switching** - Starts each client in efficient per-command routing mode
- **Auto-detection** - Switches to stateful mode when client uses GROUP, XOVER, NEXT, LAST, etc.
- **Resource efficiency** - Uses shared connection pool until stateful behavior is needed
- **Seamless transition** - Switching happens transparently without client awareness
- **Pool reservation** - Reserves stateful connections (max_connections - 1) while keeping 1 for per-command routing
- **Universal compatibility** - Works with any NNTP client, optimizing automatically based on usage patterns

#### Standard Mode - `--routing-mode standard`

- One backend connection per client
- Simple 1:1 connection forwarding
- All NNTP commands supported
- Lower overhead, easier debugging
- Predictable behavior for legacy deployments

#### Per-Command Routing Mode - `--routing-mode per-command`

- Each command routed to next backend (round-robin)
- Commands processed serially (one at a time)
- Multiple clients share backend pool
- Health-aware routing
- Better resource distribution
- Stateful commands rejected (GROUP, XOVER, etc.)

## Architecture

### Module Organization

The codebase is organized into focused modules with clear responsibilities:

| Module | Purpose |
|--------|---------|
| `auth/` | Client and backend authentication (RFC 4643 AUTHINFO) |
| `cache/` | Article caching with TTL-based expiration (cache proxy binary) |
| `command/` | NNTP command parsing and classification |
| `config/` | Configuration loading and validation (TOML + environment variables) |
| `constants/` | Buffer sizes, timeouts, and performance tuning constants |
| `health/` | Backend health monitoring with DATE command probes |
| `network/` | Socket optimization for high-throughput transfers |
| `pool/` | Connection and buffer pooling with deadpool |
| `protocol/` | RFC 3977 protocol parsing, response categorization, message-ID handling |
| `router/` | Backend selection with lock-free round-robin and health awareness |
| `session/` | Client session lifecycle and command/response streaming |
| `stream/` | Connection abstraction supporting TCP and TLS |
| `tls/` | TLS configuration and handshake management using rustls |
| `types/` | Core type definitions (ClientId, BackendId) |

### Protocol Module

The `protocol` module centralizes all NNTP protocol knowledge:

- **`commands.rs`**: Command construction helpers (QUIT, DATE, AUTHINFO, ARTICLE, etc.)
- **`responses.rs`**: Response constants and builders (AUTH_REQUIRED, BACKEND_UNAVAILABLE, etc.)
- **`response.rs`**: Response parsing with `ResponseCode` enum for type-safe categorization
  - Multiline detection per RFC 3977 (1xx, 215, 220-225, 230-231, 282)
  - Message-ID extraction and validation (RFC 5536)
  - Terminator detection for streaming responses

### How It Works

#### Hybrid Mode Flow (Default)

```
Client Connection
    ↓
Send Greeting (200 NNTP Proxy Ready)
    ↓
Read Command
    ↓
Classify Command (is_stateful check)
    ↓
┌─ Stateless Command ─────┐    ┌─ Stateful Command ────┐
│  Route to Backend       │    │  Switch to Stateful   │
│  (per-command routing)  │    │  Reserve Backend       │
│  Execute & Stream       │    │  Bidirectional Forward │
│  Return to Pool         │    │  (until disconnect)    │
└─────────────────────────┘    └────────────────────────┘
    ↓                              ↓
Return to Command Reading      Connection Cleanup
```

#### Standard Mode Flow

```
Client Connection
    ↓
Select Backend (round-robin, health-aware)
    ↓
Get Pooled Connection (pre-authenticated)
    ↓
Bidirectional Data Forwarding
    ↓
Connection Cleanup & Return to Pool
```

#### Per-Command Routing Mode Flow

```
Client Connection
    ↓
Send Greeting (200 NNTP Proxy Ready)
    ↓
Read Command
    ↓
Classify Command (protocol/command.rs)
    ↓
Route to Healthy Backend (round-robin)
    ↓
Get Pooled Connection
    ↓
Execute Command (waits for complete response)
    ↓
Stream Response to Client
    ↓
Return Connection to Pool
    ↓
Repeat (serial command processing)
```

### Performance Optimizations

The proxy implements several performance optimizations:

| Optimization | Impact | Description |
|--------------|--------|-------------|
| **ResponseCode enum** | Eliminates redundant parsing | Parse response once, reuse for multiline detection and success checks |
| **Lock-free routing** | ~10-15% CPU reduction | Atomic operations for backend selection instead of RwLock |
| **Pre-authenticated pools** | Eliminates auth overhead | Connections authenticate once during pool initialization |
| **Buffer pooling** | ~200+ allocs/sec saved | Reuse pre-allocated buffers in hot paths |
| **Optimized I/O** | Fewer syscalls | 256KB buffers for article transfers, TCP socket tuning |
| **TLS 1.3 with 0-RTT** | Faster reconnections | Session resumption and early data support in rustls |
| **Direct byte parsing** | Avoids allocations | Message-ID extraction and protocol parsing work on byte slices |

### RFC Compliance

The proxy adheres to NNTP standards:

- **RFC 3977**: Network News Transfer Protocol (NNTP)
  - Correct multiline response detection (status code second digit 1/2/3)
  - Proper terminator handling (`\r\n.\r\n`)
  - Serial command processing
- **RFC 4643**: AUTHINFO USER/PASS authentication extension
- **RFC 5536**: Message-ID format validation

## Performance

### Performance Characteristics

- **CPU Usage**: Low overhead with lock-free routing and optimized protocol parsing
  - Per-command routing mode: ~15% of one core for 80 connections at 105MB/s (AMD Ryzen 9 5950X)
  - Standard mode: Similar or lower due to simpler forwarding logic
- **Memory**: Constant usage with pooled buffers; no response buffering (streaming only)
- **Latency**: Minimal overhead (~1-2ms) for command routing and parsing
- **Throughput**: Typically limited by backend servers or network, not the proxy
- **Scalability**: Efficiently handles hundreds of concurrent connections per backend

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
- **200+ unit tests** covering all modules
- **Integration tests** for end-to-end scenarios including:
  - Multiline response handling
  - Per-command routing mode
  - Connection pooling and health checks
  - TLS/SSL connections
- **Protocol compliance tests** for RFC 3977, RFC 4643, RFC 5536
- **Zero clippy warnings** with strict linting enabled

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
| `rustls` | Modern, memory-safe TLS implementation |
| `tokio-rustls` | Tokio integration for rustls |
| `webpki-roots` | Mozilla's CA certificate bundle |
| `rustls-native-certs` | System certificate store integration |
| `tracing` / `tracing-subscriber` | Structured logging framework |
| `anyhow` | Ergonomic error handling |
| `clap` | Command-line argument parsing with derive macros |
| `serde` / `toml` | Configuration parsing and serialization |
| `deadpool` | Generic connection pooling |
| `moka` | High-performance cache (for cache proxy) |
| `memchr` | Fast byte searching (message-ID extraction) |

### Development Dependencies

- `tempfile` - Temporary files for config testing
- Test helpers in `tests/test_helpers.rs`

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

- [ ] Prometheus metrics endpoint for monitoring
- [ ] Configuration hot-reload without restart
- [ ] Admin HTTP API for runtime stats and control
- [ ] Response caching layer for frequently requested articles
- [ ] IPv6 support
- [ ] Connection affinity mode (sticky sessions)

### Recently Completed (v0.2.0)

- [x] **Protocol module refactoring** - Centralized NNTP protocol handling
  - ResponseCode enum for type-safe response categorization
  - Message-ID extraction and validation helpers
  - Eliminated redundant response parsing (70% traffic optimization)
- [x] **TLS/SSL support** - Secure backend connections with rustls
  - System certificate store integration
  - TLS 1.3 with session resumption
  - Per-server TLS configuration
- [x] **Multiline response fix** - Correct RFC 3977 multiline detection
  - Fixed connection pool exhaustion bug
  - Proper status code parsing
- [x] Lock-free routing with atomic operations
- [x] Health checking system with DATE command probes
- [x] Per-command routing mode
- [x] Pre-authenticated connection pools
- [x] TOML configuration with environment variable overrides

## Contributing

Contributions welcome! Please:

1. Fork the repository
2. Create a feature branch
3. Install git hooks: `./scripts/install-git-hooks.sh`
4. Make your changes with tests
5. Ensure all checks pass:
   - `cargo test` - Run all tests
   - `cargo clippy --all-targets --all-features` - Run linter
   - `cargo fmt` - Format code
6. Submit a pull request

### Development Setup

After cloning the repository, install git hooks to automatically run code quality checks:

```bash
./scripts/install-git-hooks.sh
```

The pre-commit hook will automatically run:
- `cargo fmt --check` - Verify code formatting
- `cargo clippy --all-targets --all-features` - Check for lint warnings

To bypass the hook temporarily (not recommended): `git commit --no-verify`

## License

MIT License - see LICENSE file for details.

## Acknowledgments

Built with Rust and the excellent Tokio async ecosystem.
