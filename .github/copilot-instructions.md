# NNTP Proxy - GitHub Copilot Instructions

This document provides comprehensive guidance for understanding and working with the NNTP Proxy codebase. It is optimized for GitHub Copilot to provide better context-aware suggestions.

**Last Updated:** November 10, 2025  
**Version:** 0.2.2  
**Rust Edition:** 2024  
**MSRV:** 1.85+

---

## Table of Contents

1. [Project Overview](#project-overview)
2. [Architecture Overview](#architecture-overview)
3. [Module Structure](#module-structure)
4. [Core Concepts](#core-concepts)
5. [Coding Standards](#coding-standards)
6. [Performance Patterns](#performance-patterns)
7. [Testing Guidelines](#testing-guidelines)
8. [Common Tasks](#common-tasks)

---

## Project Overview

### What is NNTP Proxy?

A high-performance NNTP (Network News Transfer Protocol) proxy server written in Rust with:
- **Hybrid routing mode** - Intelligent per-command routing that auto-switches to stateful when needed (default)
- **Round-robin load balancing** - Distributes connections across multiple backend servers
- **TLS/SSL support** - Secure backend connections using rustls
- **Connection pooling** - Pre-authenticated connections with deadpool
- **Article caching** - Optional LRU cache for frequently accessed articles
- **Client authentication** - Config-based authentication with credential validation

### Design Philosophy

1. **Performance-first** - Lock-free routing, zero-allocation hot paths, SIMD-optimized parsing
2. **Type safety** - Newtype pattern for all domain values (Port, ServerName, MessageId, etc.)
3. **RFC compliance** - Strict adherence to RFC 3977 (NNTP), RFC 4643 (Auth), RFC 5536 (Message-ID)
4. **Testability** - 74%+ code coverage, integration tests, mock servers, property-based testing
5. **Observability** - Structured logging with tracing, detailed metrics, error classification

### Binary Targets

1. **`nntp-proxy`** - Main proxy server with three routing modes
2. **`nntp-cache-proxy`** - Caching variant with article cache layer

---

## Architecture Overview

### High-Level Flow

```
Client → Proxy (Routing Logic) → Backend Pool → Backend Server(s)
          ↓                          ↓
      Auth Handler              Connection Pool
          ↓                          ↓
    Session Manager            Health Checker
```

### Three Routing Modes

#### 1. **Hybrid Mode** (Default - Recommended)
- Starts in per-command routing for efficiency
- Auto-detects stateful commands (GROUP, XOVER, etc.)
- Seamlessly switches to dedicated backend connection
- **Best for:** Universal compatibility with optimal performance

#### 2. **Standard Mode** (`--routing-mode standard`)
- Traditional 1:1 client-to-backend mapping
- Each client gets dedicated backend connection
- Full NNTP protocol support
- **Best for:** Maximum compatibility, debugging, legacy deployments

#### 3. **Per-Command Mode** (`--routing-mode per-command`)
- Each command routes independently (round-robin)
- Rejects stateful commands (GROUP, NEXT, LAST, XOVER, etc.)
- Only supports message-ID based operations
- **Best for:** Indexing tools, specialized workloads

### Request Flow (Per-Command Mode)

```
1. Client connects → TcpStream
2. Proxy sends greeting (200 NNTP Proxy Ready)
3. Client sends command → Command Parser
4. Router selects backend (round-robin)
5. Get connection from pool
6. Forward command → Backend
7. Stream response → Client (zero-copy)
8. Return connection to pool
9. Repeat from step 3
```

### Request Flow (Hybrid Mode)

```
1. Client connects → TcpStream
2. Proxy sends greeting
3. Start in per-command mode
4. For each command:
   a. Classify command (stateful vs stateless)
   b. If stateless → route per-command
   c. If stateful → switch to dedicated backend
5. Once switched, remain in stateful mode
```

---

## Module Structure

### Source Organization (`src/`)

```
src/
├── lib.rs                    # Public API exports, module declarations
├── main binaries
│   ├── bin/
│   │   ├── nntp-proxy.rs            # Main proxy server
│   │   └── nntp-cache-proxy.rs      # Caching variant
│
├── core types
│   ├── types.rs                     # ClientId, BackendId, core types
│   ├── types/
│   │   ├── config.rs               # BufferSize, Port, validated types
│   │   ├── metrics.rs              # BytesTransferred, TransferMetrics
│   │   ├── protocol.rs             # MessageId
│   │   └── validated.rs            # HostName, ServerName validation
│
├── configuration
│   ├── config/
│   │   ├── mod.rs                  # Public exports
│   │   ├── types.rs                # Config, ServerConfig, RoutingMode
│   │   ├── defaults.rs             # Default values for config
│   │   ├── loading.rs              # TOML parsing and validation
│   │   └── validation.rs           # Config validation logic
│
├── protocol handling
│   ├── protocol/
│   │   ├── mod.rs                  # Public exports
│   │   ├── commands.rs             # Command construction helpers
│   │   ├── response.rs             # Response parsing (NntpResponse, ResponseCode)
│   │   └── responses.rs            # Response constants (AUTH_REQUIRED, etc.)
│
├── command processing
│   ├── command/
│   │   ├── mod.rs                  # Public exports
│   │   ├── classifier.rs           # ULTRA-FAST command classification (4-6ns hot path)
│   │   └── handler.rs              # CommandHandler, CommandAction, AuthAction
│
├── session management
│   ├── session/
│   │   ├── mod.rs                  # ClientSession, SessionMode
│   │   ├── backend.rs              # Backend command execution
│   │   ├── connection.rs           # Connection state management
│   │   ├── error_classification.rs # Error classification utilities
│   │   ├── handlers/
│   │   │   ├── standard.rs         # 1:1 mode handler
│   │   │   ├── per_command.rs      # Per-command routing handler
│   │   │   └── hybrid.rs           # Hybrid mode switching logic
│   │   └── streaming/
│   │       ├── mod.rs              # Streaming utilities
│   │       ├── client.rs           # Client streaming helpers
│   │       └── tail_buffer.rs      # Terminator detection across chunks
│
├── routing and load balancing
│   ├── router/
│   │   ├── mod.rs                  # BackendSelector (round-robin)
│   │   └── tests/                  # Router tests
│
├── connection pooling
│   ├── pool/
│   │   ├── mod.rs                  # Public exports
│   │   ├── buffer.rs               # BufferPool, PooledBuffer (crossbeam-based)
│   │   ├── connection_pool.rs      # Generic connection pool
│   │   ├── connection_trait.rs     # ConnectionProvider trait
│   │   ├── deadpool_connection.rs  # Deadpool-based provider
│   │   ├── connection_guard.rs     # RAII guard for error handling
│   │   ├── health_check.rs         # Health checking (TCP + DATE command)
│   │   └── prewarming.rs           # Connection prewarming
│
├── authentication
│   ├── auth/
│   │   ├── mod.rs                  # Public exports
│   │   ├── handler.rs              # AuthHandler (client auth)
│   │   └── backend.rs              # BackendAuthenticator
│
├── caching
│   ├── cache/
│   │   ├── mod.rs                  # Public exports
│   │   ├── article.rs              # ArticleCache (moka-based LRU)
│   │   └── session.rs              # CachingSession
│
├── networking
│   ├── network.rs                  # SocketOptimizer
│   ├── network/
│   │   └── optimizers.rs           # NetworkOptimizer trait, implementations
│   ├── stream.rs                   # ConnectionStream (Plain/TLS enum)
│   └── tls.rs                      # TLS configuration and helpers
│
├── utilities
│   ├── constants.rs                # All magic numbers (buffer sizes, timeouts)
│   ├── formatting.rs               # Display helpers (bytes, IDs)
│   ├── proxy.rs                    # NntpProxy, NntpProxyBuilder
│   └── connection_error.rs         # ConnectionError type
│
└── health monitoring
    └── health/
        ├── mod.rs                  # Public exports
        └── types.rs                # Health check types
```

### Test Organization (`tests/`)

```
tests/
├── test_helpers.rs              # Reusable test utilities, mock servers
├── config_helpers.rs            # Config creation helpers
├── integration_tests.rs         # Main integration test suite
├── test_authentication.rs       # Auth basic tests
├── test_auth_integration.rs     # Auth integration tests
├── test_auth_security.rs        # Auth security tests
├── test_auth_bypass_prevention.rs # Auth bypass prevention
├── test_duplicate_greeting.rs   # Greeting deduplication tests
└── test_review_claims.rs        # Performance claims validation
```

---

## Core Concepts

### 1. Type-Safe Domain Values

**Pattern:** All domain values use the newtype pattern with validation.

```rust
// ❌ WRONG - Primitive obsession
fn connect(host: String, port: u16) -> Result<()> { ... }

// ✅ CORRECT - Type-safe with validation
fn connect(host: HostName, port: Port) -> Result<()> { ... }
```

**Key Types:**
- `HostName` - Validated hostname (non-empty)
- `ServerName` - Validated server name (non-empty)
- `Port` - Validated port (1-65535)
- `MessageId<'a>` - RFC 5536 compliant message-ID with `<` and `@`
- `ClientId` - UUIDv4 for request tracking
- `BackendId` - Zero-cost wrapper around backend index
- `MaxConnections` - Non-zero connection limit
- `BufferSize` - Non-zero buffer size
- `CacheCapacity` - Non-zero cache capacity

**Benefits:**
- Compile-time validation
- Self-documenting code
- Impossible to mix up parameters
- Zero runtime cost (newtype pattern)

### 2. Zero-Allocation Hot Paths

**Critical Performance Path:** Command classification must be 4-6ns.

```rust
// classifier.rs - 70%+ of all NNTP traffic
impl NntpCommand {
    pub fn classify(command: &str) -> Self {
        // Direct byte comparisons, no allocations
        let bytes = command.as_bytes();
        
        // SIMD-friendly case matching
        if matches_any(cmd, ARTICLE_CASES) {
            return self.classify_article(bytes, cmd_end);
        }
        
        // ... ultra-fast classification
    }
}
```

**Rules:**
- No `String::new()` or `Vec::new()` in hot paths
- Use `&str` and `&[u8]` slices
- Prefer stack over heap
- Use `memchr` for fast byte searching (SIMD-accelerated)

### 3. Streaming Architecture

**Pattern:** Never buffer entire responses. Stream directly client ↔ backend.

```rust
// ❌ WRONG - Buffers entire response (memory explosion)
async fn forward_response(client: &mut TcpStream, backend: &mut TcpStream) {
    let mut response = Vec::new();
    backend.read_to_end(&mut response).await?;
    client.write_all(&response).await?;
}

// ✅ CORRECT - Streams with double-buffering
async fn stream_response(client: &mut WriteHalf, backend: &mut ReadHalf, buffer: &mut PooledBuffer) {
    loop {
        let n = backend.read(buffer.as_mut()).await?;
        if n == 0 { break; }
        client.write_all(&buffer[..n]).await?;
    }
}
```

**Benefits:**
- Constant memory usage regardless of article size
- 100x+ throughput improvement
- Handles GB-sized articles efficiently

### 4. Error Classification

**Pattern:** Distinguish error types for appropriate handling.

```rust
use crate::session::error_classification::ErrorClassifier;

match result {
    Err(e) if ErrorClassifier::is_client_disconnect(&e) => {
        debug!("Client disconnected (normal)");
    }
    Err(e) if ErrorClassifier::is_authentication_error(&e) => {
        error!("Auth failed: {}", e);
    }
    Err(e) => {
        warn!("Unexpected error: {}", e);
    }
}
```

**Error Categories:**
- **Client disconnect** - Broken pipe, connection reset (DEBUG level)
- **Authentication error** - Backend auth failure (ERROR level)
- **Network error** - Timeouts, DNS failures (WARN level)
- **Backend error** - Protocol violations, unexpected responses (ERROR level)

### 5. Connection Pool Management

**Pattern:** Use `deadpool` with custom health checking.

```rust
// Get connection from pool
let mut conn = provider.get_pooled_connection().await?;

// Use connection (automatically returned to pool on drop)
execute_command(&mut *conn, command).await?;

// Connection auto-returned unless error detected
```

**Pool Configuration:**
- `max_connections` - Per-backend connection limit (default: 10)
- Health checks - TCP peek + DATE command validation
- Prewarming - Create connections at startup
- Graceful shutdown - Close idle connections cleanly

---

## Coding Standards

### Rust Code Style

1. **Follow Rust 2024 Edition idioms**
   - Use `const fn` where possible
   - Prefer `#[must_use]` on getter methods
   - Use `#[inline]` on hot path functions
   - Mark intentionally unused code with `#[allow(dead_code)]`

2. **Error Handling**
   ```rust
   // ✅ Use anyhow::Result for application code
   pub async fn handle_client(stream: TcpStream) -> Result<()> { ... }
   
   // ✅ Use thiserror for library errors
   #[derive(Debug, thiserror::Error)]
   pub enum ValidationError {
       #[error("Invalid hostname: {0}")]
       InvalidHostname(String),
   }
   
   // ✅ Use context for better error messages
   connection.read(&mut buf).await
       .context("Failed to read from backend")?;
   ```

3. **Async Patterns**
   ```rust
   // ✅ Use tokio::spawn for concurrent tasks
   tokio::spawn(async move {
       proxy.handle_client(stream, addr).await
   });
   
   // ✅ Use select! for concurrent operations with cancellation
   tokio::select! {
       result = client.read(&mut buf) => { ... }
       _ = shutdown_signal() => { ... }
   }
   
   // ❌ Don't block async runtime
   // std::thread::sleep(Duration::from_secs(1)); // WRONG!
   tokio::time::sleep(Duration::from_secs(1)).await; // CORRECT
   ```

4. **Naming Conventions**
   - **Modules:** `snake_case` (e.g., `connection_pool`)
   - **Types:** `PascalCase` (e.g., `ClientSession`)
   - **Functions:** `snake_case` (e.g., `route_command_sync`)
   - **Constants:** `SCREAMING_SNAKE_CASE` (e.g., `MAX_CONNECTIONS`)
   - **Type aliases:** `PascalCase` (e.g., `Result`)

5. **Documentation**
   ```rust
   /// Parse NNTP response status code
   ///
   /// Per [RFC 3977 §3.2](https://datatracker.ietf.org/doc/html/rfc3977#section-3.2),
   /// responses begin with a 3-digit status code.
   ///
   /// # Arguments
   /// * `data` - Raw response bytes
   ///
   /// # Returns
   /// Status code if valid, None otherwise
   ///
   /// # Examples
   /// ```
   /// assert_eq!(parse_status_code(b"200 OK"), Some(200));
   /// ```
   #[inline]
   pub fn parse_status_code(data: &[u8]) -> Option<u16> { ... }
   ```

   **Always include:**
   - RFC references for protocol-related code
   - Examples for public APIs
   - Performance characteristics for critical paths
   - Invariants and assumptions

### Performance Guidelines

#### 1. Hot Path Optimization

**Critical paths (<10ns target):**
- Command classification (`classifier.rs`)
- Response code parsing (`response.rs`)
- Backend selection (`router/mod.rs`)

**Rules:**
```rust
// ✅ Direct byte comparison (4-6ns)
if bytes[0..7] == *b"ARTICLE" { ... }

// ❌ String conversion (40-60ns)
if command.to_uppercase().starts_with("ARTICLE") { ... }

// ✅ memchr for byte searching (SIMD)
if let Some(pos) = memchr::memchr(b'<', bytes) { ... }

// ❌ Iterator-based search
if let Some(pos) = bytes.iter().position(|&b| b == b'<') { ... }
```

#### 2. Buffer Management

**Use `PooledBuffer` for I/O operations:**
```rust
// ✅ Get buffer from pool
let mut buffer = buffer_pool.get();

// Use buffer
let n = stream.read(buffer.as_mut()).await?;

// Buffer automatically returned to pool on drop
```

**Constants from `src/constants.rs`:**
```rust
use crate::constants::buffer::*;

const POOL: usize = 256 * 1024;        // 256KB pooled buffers
const COMMAND: usize = 512;            // Command buffer
const RESPONSE_MAX: usize = 1024 * 1024; // 1MB response limit
const STREAM_CHUNK: usize = 65536;     // 64KB streaming chunks
```

#### 3. Lock-Free Algorithms

**Router uses atomic operations:**
```rust
// ✅ Lock-free round-robin
let index = self.current_backend
    .fetch_add(1, Ordering::Relaxed) % self.backends.len();

// ✅ Atomic pending count tracking
backend.pending_count.fetch_add(1, Ordering::Relaxed);
```

**Never:**
- Use `Mutex` in hot paths
- Create contention on shared state
- Block on locks in async code

#### 4. Memory Allocation Patterns

```rust
// ✅ Pre-allocate with capacity
let mut backends = Vec::with_capacity(server_count);

// ✅ Reuse buffers across iterations
let mut line = String::with_capacity(COMMAND);
loop {
    line.clear();
    reader.read_line(&mut line).await?;
    // ... process line
}

// ❌ Allocate in loop
loop {
    let mut line = String::new(); // BAD: allocates every iteration
    reader.read_line(&mut line).await?;
}
```

#### 5. TCP Optimization

**Apply socket optimizations:**
```rust
use crate::network::{ConnectionOptimizer, NetworkOptimizer};

// Set large buffers for throughput
let optimizer = ConnectionOptimizer::new(&stream);
optimizer.optimize()?; // Sets 16MB buffers, TCP_NODELAY, etc.
```

**Constants from `src/constants.rs`:**
```rust
const HIGH_THROUGHPUT_RECV_BUFFER: usize = 16 * 1024 * 1024; // 16MB
const HIGH_THROUGHPUT_SEND_BUFFER: usize = 16 * 1024 * 1024; // 16MB
```

### Logging Standards

**Use structured logging with `tracing`:**

```rust
use tracing::{debug, info, warn, error};

// ✅ Structured fields
info!(
    client_addr = %client_addr,
    backend_id = ?backend_id,
    "Routing client to backend"
);

// ✅ Appropriate log levels
debug!("Got pooled connection");      // Internal state
info!("Client connected");            // Important events
warn!("Backend unhealthy");           // Degraded state
error!("Authentication failed");      // Errors requiring attention

// ❌ Don't log in hot paths
// info!("Processing command: {}", cmd); // BAD: Called millions of times

// ✅ Use debug for hot paths
debug!("Processing {}", cmd); // OK: Filtered out in production
```

**Log Level Guidelines:**
- `error!` - Auth failures, backend errors, unrecoverable errors
- `warn!` - Degraded performance, recoverable errors, unusual conditions
- `info!` - Connection lifecycle, routing decisions, important state changes
- `debug!` - Internal state, hot path activities (disabled in production)
- `trace!` - Extremely verbose debugging (disabled by default)

---

## Performance Patterns

### 1. Command Classification (4-6ns Hot Path)

**Location:** `src/command/classifier.rs`

**Pattern:** Ultra-fast byte-based classification with SIMD-friendly matching.

```rust
/// Classify NNTP command - PERFORMANCE CRITICAL (70%+ of traffic)
///
/// Target: 4-6ns on modern CPUs
/// - Zero allocations
/// - SIMD-friendly byte comparisons
/// - Branch prediction optimized (UPPERCASE first)
pub fn classify(command: &str) -> Self {
    let bytes = command.as_bytes();
    
    // Fast path: Check for message-ID (70%+ hit rate)
    if let Some(cmd_end) = find_command_end(bytes) {
        let cmd = &bytes[..cmd_end];
        
        // UPPERCASE checked first (95% of real traffic)
        if matches_any(cmd, ARTICLE_CASES) {
            return self.classify_article(bytes, cmd_end);
        }
        
        // ... more classifications
    }
    
    Self::Stateless // Unknown commands forwarded
}
```

**Key Techniques:**
- Pre-computed case arrays (UPPERCASE, lowercase, Titlecase)
- Direct byte comparison with `matches_any` macro
- Early returns for common cases
- Zero string allocations

### 2. Response Streaming (100x Throughput)

**Location:** `src/session/backend.rs`

**Pattern:** Double-buffered streaming with terminator detection.

```rust
/// Stream multiline response from backend to client
///
/// PERFORMANCE CRITICAL - DO NOT BUFFER ENTIRE RESPONSE
/// Uses double-buffering for 100x+ throughput improvement
async fn stream_multiline_response(
    client_write: &mut WriteHalf<'_>,
    backend_read: &mut BufReader<ReadHalf<'_>>,
    buffer: &mut PooledBuffer,
) -> Result<u64> {
    let mut bytes_written = 0u64;
    let mut tail = TailBuffer::default();
    
    loop {
        // Read chunk from backend
        let n = backend_read.read(buffer.as_mut()).await?;
        if n == 0 { break; }
        
        // Check for terminator
        match tail.detect_terminator(&buffer[..n]) {
            TerminatorStatus::FoundAt(pos) => {
                client_write.write_all(&buffer[..pos]).await?;
                bytes_written += pos as u64;
                break;
            }
            _ => {
                client_write.write_all(&buffer[..n]).await?;
                bytes_written += n as u64;
                tail.update(&buffer[..n]);
            }
        }
    }
    
    Ok(bytes_written)
}
```

**Critical Points:**
- Never call `read_to_end()` or `read_to_string()`
- Stream chunks immediately to client
- Use `TailBuffer` for cross-chunk terminator detection
- Reuse `PooledBuffer` across iterations

### 3. Connection Pool Prewarming

**Location:** `src/pool/prewarming.rs`

**Pattern:** Concurrent connection creation with batching.

```rust
/// Prewarm all connection pools concurrently
///
/// Creates connections in batches to avoid overwhelming backends
pub async fn prewarm_pools(
    providers: &[DeadpoolConnectionProvider],
    servers: &[ServerConfig],
) -> Result<()> {
    let tasks: Vec<_> = providers
        .iter()
        .zip(servers.iter())
        .map(|(provider, server)| {
            tokio::spawn(prewarm_single_pool(
                provider.clone(),
                server.clone(),
            ))
        })
        .collect();
    
    // Wait for all prewarming to complete
    for task in tasks {
        task.await??;
    }
    
    Ok(())
}
```

**Benefits:**
- Reduced first-request latency
- Predictable startup time
- Validates backend connectivity early

### 4. Health Check Strategy

**Location:** `src/pool/health_check.rs`

**Pattern:** Two-level health checking (TCP + Application).

```rust
/// Fast TCP-level check (non-blocking)
pub fn check_tcp_alive(conn: &mut ConnectionStream) -> RecycleResult<anyhow::Error> {
    match conn.try_read(&mut [0u8; 1]) {
        Ok(0) => Err(anyhow::anyhow!("Connection closed")),
        Ok(_) => Err(anyhow::anyhow!("Unexpected data")),
        Err(ref e) if e.kind() == ErrorKind::WouldBlock => Ok(()), // Healthy!
        Err(e) => Err(e.into()),
    }
}

/// Application-level check (sends DATE command)
pub async fn check_date_response(conn: &mut ConnectionStream) -> Result<(), HealthCheckError> {
    conn.write_all(DATE_COMMAND).await?;
    
    let mut buf = [0u8; HEALTH_CHECK_BUFFER_SIZE];
    let n = timeout(HEALTH_CHECK_TIMEOUT, conn.read(&mut buf)).await??;
    
    let response = std::str::from_utf8(&buf[..n])?;
    if !response.starts_with(EXPECTED_DATE_RESPONSE_PREFIX) {
        return Err(HealthCheckError::UnexpectedResponse(response.to_string()));
    }
    
    Ok(())
}
```

**Strategy:**
1. TCP check - Fast, catches closed connections
2. DATE command - Slower, validates NNTP protocol
3. Configurable intervals and thresholds

---

## Testing Guidelines

### Test Structure

The codebase uses a mix of unit tests (in module files) and integration tests (in `tests/` directory).

**Coverage target:** 74%+ overall, 90%+ for critical paths

#### Unit Tests (in `src/**/*.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_basic_functionality() {
        let result = function_under_test();
        assert_eq!(result, expected_value);
    }
    
    #[tokio::test]
    async fn test_async_functionality() {
        let result = async_function().await;
        assert!(result.is_ok());
    }
}
```

#### Integration Tests (in `tests/*.rs`)

```rust
// tests/integration_tests.rs
use nntp_proxy::*;
use tokio::net::TcpListener;

#[tokio::test]
async fn test_proxy_routing() -> Result<()> {
    // 1. Setup mock backend
    let mock_port = find_available_port().await;
    tokio::spawn(create_mock_server(mock_port));
    
    // 2. Create proxy
    let proxy = NntpProxy::new(test_config(), RoutingMode::Hybrid)?;
    
    // 3. Test interaction
    let mut client = TcpStream::connect("127.0.0.1:8119").await?;
    // ... assertions
    
    Ok(())
}
```

### Test Helpers

**Location:** `tests/test_helpers.rs`

```rust
/// Create a mock NNTP server for testing
pub fn spawn_mock_server(port: u16, server_name: &str) -> JoinHandle<()> {
    tokio::spawn(async move {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).await?;
        // ... mock server implementation
    })
}

/// Find an available port for testing
pub async fn find_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// Create test configuration
pub fn create_test_config() -> Config {
    use crate::config::ServerConfig;
    
    Config {
        servers: vec![
            ServerConfig::builder("localhost", 119)
                .name("Test Server")
                .build()
                .unwrap()
        ],
        ..Default::default()
    }
}
```

### Testing Patterns

#### 1. Testing Command Classification

```rust
#[test]
fn test_article_by_message_id() {
    let cmd = NntpCommand::classify("ARTICLE <test@example.com>");
    assert_eq!(cmd, NntpCommand::ArticleByMessageId);
    assert!(!cmd.is_stateful());
}

#[test]
fn test_stateful_commands() {
    let commands = vec!["GROUP alt.test", "NEXT", "LAST", "XOVER 1-100"];
    for cmd in commands {
        let classified = NntpCommand::classify(cmd);
        assert!(classified.is_stateful(), "Command '{}' should be stateful", cmd);
    }
}
```

#### 2. Testing Response Parsing

```rust
#[test]
fn test_multiline_detection() {
    assert!(NntpResponse::is_multiline_response(220)); // ARTICLE
    assert!(NntpResponse::is_multiline_response(215)); // LIST
    assert!(!NntpResponse::is_multiline_response(200)); // Greeting
}

#[test]
fn test_terminator_detection() {
    let data = b"Article content\r\n.\r\n";
    assert!(NntpResponse::has_terminator_at_end(data));
    
    let pos = NntpResponse::find_terminator_end(data);
    assert_eq!(pos, Some(data.len()));
}
```

#### 3. Testing Authentication

```rust
#[tokio::test]
async fn test_auth_required() {
    let auth_handler = AuthHandler::new(
        Some("testuser".to_string()),
        Some("testpass".to_string()),
    );
    
    assert!(auth_handler.is_enabled());
    assert!(auth_handler.validate_credentials("testuser", "testpass"));
    assert!(!auth_handler.validate_credentials("wrong", "wrong"));
}
```

#### 4. Testing Connection Pool

```rust
#[tokio::test]
async fn test_pool_prewarming() {
    let provider = create_test_provider();
    prewarm_single_pool(&provider, &test_config()).await?;
    
    let status = provider.status();
    assert_eq!(status.available, status.max_size);
}
```

#### 5. Testing Routing

```rust
#[test]
fn test_round_robin_selection() {
    let mut selector = BackendSelector::new();
    selector.add_backend(BackendId::from_index(0), "server1", provider1);
    selector.add_backend(BackendId::from_index(1), "server2", provider2);
    
    // Should alternate
    let backend1 = selector.route_command_sync(ClientId::new(), "LIST")?;
    let backend2 = selector.route_command_sync(ClientId::new(), "LIST")?;
    let backend3 = selector.route_command_sync(ClientId::new(), "LIST")?;
    
    assert_eq!(backend1.as_index(), 0);
    assert_eq!(backend2.as_index(), 1);
    assert_eq!(backend3.as_index(), 0); // Wraps around
}
```

### Mock Servers

**Smart Mock Server** (for integration tests):

```rust
pub async fn create_smart_mock_server(port: u16, name: &str) -> JoinHandle<()> {
    tokio::spawn(async move {
        let listener = TcpListener::bind(format!("127.0.0.1:{}", port)).await?;
        
        loop {
            let (mut stream, _) = listener.accept().await?;
            
            // Send greeting
            stream.write_all(b"200 Ready\r\n").await?;
            
            let mut buf = [0; 1024];
            loop {
                let n = stream.read(&mut buf).await?;
                let command = std::str::from_utf8(&buf[..n])?;
                
                // Handle specific commands
                if command.starts_with("GROUP") {
                    stream.write_all(b"211 100 1 100 alt.test\r\n").await?;
                } else if command.starts_with("ARTICLE") {
                    stream.write_all(b"220 0 <msg@test>\r\n").await?;
                    stream.write_all(b"Subject: Test\r\n\r\nBody\r\n.\r\n").await?;
                } else if command.starts_with("QUIT") {
                    stream.write_all(b"205 Goodbye\r\n").await?;
                    break;
                } else {
                    stream.write_all(b"200 OK\r\n").await?;
                }
            }
        }
    })
}
```

### Running Tests

```bash
# Run all tests
cargo test

# Run with output
cargo test -- --nocapture

# Run specific test
cargo test test_hybrid_mode

# Run integration tests only
cargo test --test integration_tests

# Run with coverage (requires cargo-llvm-cov)
cargo llvm-cov --html
```

### Benchmarking

**Location:** `benches/`

```rust
// benches/command_parsing.rs
use divan::Bencher;

#[divan::bench]
fn classify_article_command(bencher: Bencher) {
    bencher.bench(|| {
        NntpCommand::classify("ARTICLE <msg@example.com>")
    });
}

#[divan::bench]
fn parse_status_code(bencher: Bencher) {
    bencher.bench(|| {
        NntpResponse::parse_status_code(b"200 OK")
    });
}
```

Run benchmarks:
```bash
cargo bench
```

---

## Common Tasks

### 1. Adding a New Backend Server

**In `config.toml`:**
```toml
[[servers]]
host = "news.example.com"
port = 119
name = "Example Server"
max_connections = 10
use_tls = false
```

**Programmatically:**
```rust
use nntp_proxy::config::ServerConfig;

let server = ServerConfig::builder("news.example.com", 119)
    .name("Example Server")
    .max_connections(15)
    .use_tls(true)
    .tls_verify_cert(true)
    .build()?;
```

### 2. Adding TLS Support to a Backend

```toml
[[servers]]
host = "secure.example.com"
port = 563
name = "Secure Server"
use_tls = true
tls_verify_cert = true
# Optional custom CA cert
# tls_cert_path = "/path/to/ca.pem"
```

### 3. Implementing a New NNTP Command

**1. Add to command classifier:**

```rust
// src/command/classifier.rs

// Add case array
command_cases!(
    NEWCMD_CASES,
    "NEWCMD",
    "newcmd",
    "Newcmd",
    "[RFC XXXX §X.X](url) - NEWCMD command\nDescription"
);

// Add to classify() function
pub fn classify(command: &str) -> Self {
    // ... existing code
    
    if matches_any(cmd, NEWCMD_CASES) {
        return Self::NewCommand;
    }
    
    // ...
}

// Add variant
pub enum NntpCommand {
    // ... existing variants
    NewCommand,
}
```

**2. Add command handler:**

```rust
// src/command/handler.rs

impl CommandHandler {
    pub fn handle_command(command: &str) -> CommandAction {
        match NntpCommand::classify(command) {
            // ... existing cases
            NntpCommand::NewCommand => CommandAction::ForwardStateless,
        }
    }
}
```

**3. Add tests:**

```rust
#[test]
fn test_new_command_classification() {
    assert_eq!(
        NntpCommand::classify("NEWCMD"),
        NntpCommand::NewCommand
    );
}
```

### 4. Adding Client Authentication

**In `config.toml`:**
```toml
[client_auth]
username = "proxyuser"
password = "securepassword"
```

**Programmatically:**
```rust
let config = Config {
    client_auth: ClientAuthConfig {
        username: Some("proxyuser".to_string()),
        password: Some("securepassword".to_string()),
        greeting: None,
    },
    ..Default::default()
};
```

### 5. Enabling Article Caching

**Use the caching proxy binary:**

```bash
# Run caching proxy
./target/release/nntp-cache-proxy \
    --port 8120 \
    --config cache-config.toml \
    --cache-capacity 10000 \
    --cache-ttl 3600
```

**Configuration:**
```toml
# cache-config.toml
[cache]
max_capacity = 10000  # Number of articles
ttl = 3600           # Time-to-live in seconds
```

### 6. Debugging Connection Issues

**Enable verbose logging:**
```bash
RUST_LOG=debug ./target/release/nntp-proxy
```

**Check backend health:**
```rust
// Get pool status
let status = provider.status();
info!("Pool: {}/{} available, {} created", 
    status.available, status.max_size, status.created);

// Check backend load
if let Some(load) = router.backend_load(backend_id) {
    info!("Backend {:?} pending: {}", backend_id, load);
}
```

### 7. Monitoring Performance

**Key metrics to track:**

```rust
// Connection metrics
let (sent_bytes, recv_bytes) = session.handle_client(stream, addr).await?;
info!("Session: sent={}, received={}", sent_bytes, recv_bytes);

// Pool metrics
let status = provider.status();
let utilization = (status.max_size - status.available) as f64 / status.max_size as f64;
info!("Pool utilization: {:.1}%", utilization * 100.0);

// Router metrics
let load = router.backend_load(backend_id)?;
info!("Backend load: {} pending commands", load);
```

### 8. Detecting Terminal Output

**Check if running in interactive terminal:**

```rust
use std::io::IsTerminal;

if std::io::stdout().is_terminal() {
    // Interactive terminal - use colors, progress bars
    println!("✓ Running in interactive mode");
} else {
    // Piped/redirected - output machine-readable format
    println!("Running in non-interactive mode");
}

// Check stderr separately
if std::io::stderr().is_terminal() {
    eprintln!("\x1b[31mError\x1b[0m"); // Red color
} else {
    eprintln!("Error"); // Plain text
}
```

**Use cases:**
- Colored output for terminals only
- Progress bars in interactive mode
- JSON output when piped
- Different logging formats

### 9. Creating a Release Build

```bash
# Standard release
cargo build --release

# Optimized for size
cargo build --release --config profile.release.opt-level=\"z\"

# Cross-compile for other targets
cargo build --release --target x86_64-unknown-linux-gnu
cargo build --release --target aarch64-unknown-linux-gnu

# Create distribution package
./scripts/build-release.sh
```

### 10. Running with Systemd

**Service file (`nntp-proxy.service`):**

```ini
[Unit]
Description=NNTP Proxy Server
After=network.target

[Service]
Type=simple
User=nntp-proxy
Group=nntp-proxy
ExecStart=/usr/local/bin/nntp-proxy --config /etc/nntp-proxy/config.toml
Restart=always
RestartSec=5

[Install]
WantedBy=multi-user.target
```

**Install and start:**
```bash
sudo systemctl enable nntp-proxy
sudo systemctl start nntp-proxy
sudo systemctl status nntp-proxy
```

---

## Troubleshooting

### Common Issues

**1. "No backends available for routing"**
- Check config.toml has servers defined
- Verify backend servers are reachable
- Check connection pool status

**2. "Failed to get pooled connection"**
- Pool exhausted - increase `max_connections`
- Backend unhealthy - check health checks
- Network issues - verify connectivity

**3. "Command not supported"**
- Using stateful command in per-command mode
- Switch to hybrid or standard mode
- Or use message-ID based commands

**4. "Authentication failed"**
- Check backend credentials in config
- Verify backend supports AUTHINFO
- Check backend server logs

**5. High CPU usage**
- Too many worker threads - reduce with `--threads`
- Inefficient command classification - check logs
- Network issues causing retries

**6. Memory growth**
- Connection pool not releasing - check for panics
- Buffer pool exhaustion - increase pool size
- Cache too large - reduce cache capacity

---

## Additional Resources

### RFC References

- **[RFC 3977](https://datatracker.ietf.org/doc/html/rfc3977)** - NNTP Protocol
- **[RFC 4643](https://datatracker.ietf.org/doc/html/rfc4643)** - NNTP Authentication
- **[RFC 5536](https://datatracker.ietf.org/doc/html/rfc5536)** - Message-ID Format
- **[RFC 2980](https://datatracker.ietf.org/doc/html/rfc2980)** - Common NNTP Extensions (legacy)

### Code Organization Principles

1. **Separation of Concerns** - Each module has single responsibility
2. **Type Safety** - Newtype pattern for domain values
3. **Zero-Cost Abstractions** - No runtime overhead for safety
4. **Explicit over Implicit** - Clear, self-documenting code
5. **Performance by Default** - Hot paths optimized from the start

### Development Workflow

1. **Feature branches** - Create from `main`
2. **Tests first** - Write tests before implementation
3. **Benchmarks** - Profile before and after optimization
4. **Documentation** - Update docs with code changes
5. **Coverage** - Maintain 74%+ test coverage
6. **Pre-commit checks** - Always run before committing:
   ```bash
   cargo fmt
   cargo clippy --all-targets --all-features
   cargo test
   ```

**Note:** This project has Git pre-commit hooks that automatically run `cargo fmt` and `cargo clippy --all-targets --all-features`. If clippy finds issues or formatting is incorrect, the commit will be rejected. Fix all issues before committing.

### NixOS Development Environment

**⚠️ NEVER use `cd` commands in terminal operations on NixOS**

This project uses `direnv` to automatically manage the Nix development environment. When you navigate to the project directory, `direnv` automatically:
- Loads the Nix flake development shell
- Sets up all required dependencies (Rust toolchain, build tools, etc.)
- Configures environment variables
- Ensures reproducible builds

**Why this matters:**
- `cd` commands in automated scripts/tools bypass direnv's environment activation
- This can cause builds to fail due to missing dependencies
- The environment is only properly activated when the shell itself changes directory
- Terminal tools should rely on the already-activated environment, not try to navigate

**Correct approach:**
```bash
# ✅ CORRECT - Run commands in current directory (direnv already active)
cargo build
cargo test

# ❌ WRONG - Don't try to cd in scripts/automation
cd /home/mjc/projects/nntp-proxy && cargo build  # Bypasses direnv!
```

**For manual work:**
- Simply navigate to the project directory in your shell
- `direnv` will automatically activate (you'll see: "direnv: loading...")
- All tools (cargo, rustc, etc.) are then available
- Stay in the project directory for all development tasks

**If direnv isn't activating:**
```bash
# Allow direnv for this directory (one-time setup)
direnv allow

# Force reload if needed
direnv reload
```

---

**End of Instructions**

For more information, see:
- `README.md` - User documentation
- `CHANGELOG.md` - Version history
- `CACHE-PROXY.md` - Caching proxy documentation
- `TCP_TUNING.md` - Performance tuning guide
