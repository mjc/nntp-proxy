# NNTP Proxy

A high-performance stateless NNTP proxy server written in Rust with a clean, modular architecture.

## ⚠️ Important: Stateless Mode

This proxy operates in **stateless mode** and **does not support GROUP-based commands**. It's designed for:
- Message-ID based article retrieval
- Metadata queries (LIST, CAPABILITIES, etc.)
- Future connection multiplexing capabilities

**Not compatible with traditional newsreaders** that use GROUP/NEXT/LAST navigation. See [LIMITATIONS.md](LIMITATIONS.md) for details.

## Features

- 🔄 **Round-robin load balancing** - Distributes connections evenly across backend servers
- 🚀 **Stateless design** - No session state, enables future multiplexing
- ⚡ **Async/await** - Built on Tokio for high concurrency
- 📝 **TOML Configuration** - Simple and readable configuration format
- 🔍 **Structured logging** - Detailed logging with tracing
- 🛠️ **Nix development environment** - Reproducible development setup
- 📊 **Connection tracking** - Logs client connections and backend routing
- 🔐 **Authentication interception** - Handles client auth locally
- 🧩 **Modular architecture** - Clean separation of concerns for maintainability

## Architecture

The codebase is organized into focused modules:

- **auth/** - Authentication handling (client & backend)
- **command/** - Command parsing and classification
- **config** - Configuration management
- **pool/** - Connection and buffer pooling
- **protocol/** - NNTP protocol constants and parsing
- **types** - Core type definitions

See [REFACTORING.md](REFACTORING.md) for detailed architecture documentation.

## Quick Start

### Prerequisites

- Nix with flakes enabled
- direnv (optional but recommended)

### Development Setup

1. Clone the repository:
```bash
git clone <repository-url>
cd nntp-proxy
```

2. Enter the development environment:
```bash
# If using direnv
direnv allow

# Or manually with nix
nix develop
```

3. Build and run:
```bash
cargo build
cargo run
```

## Configuration

The proxy uses a TOML configuration file (`config.toml` by default):

```toml
[[servers]]
host = "news.example.com"
port = 119
name = "Example News Server 1"
# Optional authentication
username = "your_username"
password = "your_password"
# Connection limit (defaults to 10 if not specified)
max_connections = 20

[[servers]]
host = "nntp.example.org"
port = 119
name = "Example News Server 2"
max_connections = 10

[[servers]]
host = "localhost"
port = 1119
name = "Local Test Server"
max_connections = 5
```

### Configuration Fields

- `host` - Backend server hostname
- `port` - Backend server port (default: 119)  
- `name` - Friendly name for the server
- `username` - Optional authentication username
- `password` - Optional authentication password
- `max_connections` - Maximum concurrent connections to this server (default: 10)

## Usage

```bash
# Run with default settings (port 8119, config.toml)
cargo run

# Specify custom port and config
cargo run -- --port 8120 --config my-config.toml

# Show help
cargo run -- --help
```

## Command Line Options

- `-p, --port <PORT>` - Port to listen on (default: 8119)
- `-c, --config <CONFIG>` - Configuration file path (default: config.toml)
- `-h, --help` - Print help information
- `-V, --version` - Print version information

## Testing

To test the proxy, you can use telnet to connect:

```bash
# Connect to the proxy
telnet localhost 8119

# The connection will be forwarded to one of the configured backend servers
# in round-robin fashion
```

## How it Works

1. **Client Connection**: A client connects to the proxy on the configured port
2. **Server Selection**: The proxy selects the next backend server using round-robin
3. **Backend Connection**: The proxy establishes a connection to the selected backend
4. **Bidirectional Proxy**: Data is forwarded in both directions between client and backend
5. **Connection Cleanup**: When either side closes, both connections are cleaned up

## Architecture

The proxy is built with:

- **Tokio**: Async runtime for handling many concurrent connections
- **Tracing**: Structured logging for observability
- **Clap**: Command-line argument parsing
- **Serde + TOML**: Configuration file handling
- **Anyhow**: Error handling

## TODO

- **SSL/TLS Support**: Add support for secure NNTP connections (NNTPS) for both client-facing and backend connections
- **Connection Persistence**: Implement longer-lived connections with proper connection reuse to reduce authentication overhead and improve performance
- **Health Checks**: Add periodic health checks for backend servers
- **Metrics**: Expose Prometheus metrics for monitoring
- **Configuration Hot-Reload**: Support reloading configuration without restart
- **IPv6 Support**: Full IPv6 support for client and backend connections

## Building for Production

```bash
# Build optimized release version
cargo build --release

# The binary will be in target/release/nntp-proxy
```

## License

MIT License - see LICENSE file for details.
