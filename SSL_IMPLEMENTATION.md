# SSL/TLS Implementation Guide

This document outlines how to add SSL/TLS support to the NNTP proxy. The codebase has been refactored to make SSL implementation straightforward.

## Overview

The proxy is now prepared for SSL with:
- Stream abstraction (`ConnectionStream` enum)
- Generic stream handling (AsyncRead/AsyncWrite traits)
- Detailed error types for connection failures
- Stream-agnostic session and auth handlers

## Implementation Steps

### 1. Add TLS Dependencies

Add to `Cargo.toml`:
```toml
[dependencies]
tokio-native-tls = "0.3"
native-tls = "0.2"
# OR for rustls:
tokio-rustls = "0.26"
rustls = "0.23"
```

### 2. Update ConnectionStream Enum

**File**: `src/stream.rs`

Add the TLS variant:
```rust
pub enum ConnectionStream {
    Plain(TcpStream),
    Tls(TlsStream<TcpStream>),  // Add this line
}
```

Update helper methods:
```rust
pub fn is_tls(&self) -> bool {
    matches!(self, Self::Tls(_))
}

pub fn as_tls_stream(&self) -> Option<&TlsStream<TcpStream>> {
    match self {
        Self::Tls(tls) => Some(tls),
        _ => None,
    }
}
```

Update AsyncRead/AsyncWrite implementations to handle both variants.

### 3. Add TLS Configuration

**File**: `src/config.rs`

Add TLS config fields to `ServerConfig`:
```rust
pub struct ServerConfig {
    // ... existing fields ...
    pub use_tls: bool,
    pub tls_verify_cert: bool,
    pub tls_cert_path: Option<String>,
}
```

### 4. Update ConnectionError

**File**: `src/connection_error.rs`

Uncomment the TLS error variants (around line 58):
```rust
/// TLS handshake failed
TlsHandshake {
    backend: String,
    source: Box<dyn std::error::Error + Send + Sync>,
},

/// Certificate verification failed
CertificateVerification {
    backend: String,
    reason: String,
},
```

### 5. Update TcpManager for TLS

**File**: `src/pool/deadpool_connection.rs`

The connection creation is already split into testable functions. Add TLS handshake after TCP connection:

```rust
async fn create_optimized_tcp_stream(&self) -> Result<TcpStream, anyhow::Error> {
    let addr = self.resolve_address().await?;
    let socket = self.create_configured_socket(&addr)?;
    let tcp_stream = self.connect_socket(socket, &addr).await?;
    
    // Add TLS handshake here if enabled
    if self.use_tls {
        let tls_stream = self.tls_handshake(tcp_stream).await?;
        Ok(ConnectionStream::Tls(tls_stream))
    } else {
        Ok(ConnectionStream::Plain(tcp_stream))
    }
}

async fn tls_handshake(&self, stream: TcpStream) -> Result<TlsStream<TcpStream>, ConnectionError> {
    let connector = TlsConnector::new()?;
    connector.connect(&self.host, stream)
        .await
        .map_err(|e| ConnectionError::TlsHandshake {
            backend: self.name.clone(),
            source: Box::new(e),
        })
}
```

### 6. Update Pool Manager

**File**: `src/pool/deadpool_connection.rs`

Change the pool type to store `ConnectionStream` instead of `TcpStream`:

```rust
impl managed::Manager for TcpManager {
    type Type = ConnectionStream;  // Changed from TcpStream
    type Error = anyhow::Error;
    
    async fn create(&self) -> Result<ConnectionStream, anyhow::Error> {
        let mut stream = self.create_optimized_stream().await?;
        self.consume_greeting(&mut stream).await?;
        self.authenticate_stream(&mut stream).await?;
        Ok(stream)
    }
}
```

### 7. Update consume_greeting() and authenticate_stream()

These methods are already generic over AsyncRead + AsyncWrite, so they'll work with both TCP and TLS streams without changes!

### 8. Network Optimizer

**File**: `src/network.rs`

The `optimize_connection_stream()` method already handles this:
- For Plain TCP: optimizes the socket
- For TLS: returns Ok(()) (no socket-level optimization needed)

### 9. Testing

Add tests for TLS connections:

```rust
#[tokio::test]
async fn test_tls_connection() {
    // Create TLS connector
    // Connect to TLS-enabled NNTP server
    // Verify ConnectionStream::is_tls() returns true
    // Verify can read/write through the stream
}

#[tokio::test]
async fn test_tls_handshake_failure() {
    // Attempt connection with invalid cert
    // Verify ConnectionError::TlsHandshake is returned
}
```

## Key Touchpoints

### Files that need TLS-specific changes:
1. ✅ `src/stream.rs` - Add Tls variant to ConnectionStream
2. ✅ `src/connection_error.rs` - Uncomment TLS error types
3. ✅ `src/config.rs` - Add TLS configuration fields
4. ✅ `src/pool/deadpool_connection.rs` - Add TLS handshake logic

### Files that work without changes (already generic):
1. ✅ `src/session.rs` - Generic over AsyncRead + AsyncWrite
2. ✅ `src/cache/session.rs` - Generic over AsyncRead + AsyncWrite  
3. ✅ `src/auth/backend.rs` - All methods are generic
4. ✅ `src/streaming.rs` - high_throughput_transfer is generic
5. ✅ `src/network.rs` - optimize_connection_stream handles both types

## Configuration Example

```toml
[[servers]]
host = "secure.newsserver.com"
port = 563  # NNTPS port
name = "Secure Server"
use_tls = true
tls_verify_cert = true
max_connections = 20
```

## Certificate Verification

### System Certificate Store (Default)

The implementation uses `native-tls`, which automatically uses the operating system's trusted certificate store:

- **Linux**: Uses system CA bundle (typically `/etc/ssl/certs/ca-certificates.crt` or `/etc/pki/tls/certs/ca-bundle.crt`)
- **macOS**: Uses Security.framework (system Keychain)
- **Windows**: Uses SChannel (Windows Certificate Store)

**No additional configuration needed** for standard TLS connections - the system certificates are used by default when `tls_verify_cert = true`.

### Custom CA Certificates

For self-signed certificates or private CAs, use `tls_cert_path`:

```toml
[[servers]]
host = "private-news.company.com"
port = 563
use_tls = true
tls_verify_cert = true
tls_cert_path = "/etc/nntp-proxy/custom-ca.pem"  # PEM format
```

**Important**: Custom certificates are **added to** the system certificate store, not replacing it. Both system and custom certificates are trusted.

### Testing/Development Only

For testing with self-signed certificates (NOT for production):

```toml
[[servers]]
host = "test-news.local"
port = 563
use_tls = true
tls_verify_cert = false  # ⚠️ INSECURE - skips all certificate verification
```

## Performance Considerations

- TLS adds ~10-20% CPU overhead
- Buffer sizes remain the same (16MB for throughput)
- Connection pooling is even more important with TLS (expensive handshakes)
- Health checks work identically (DATE command over TLS)

## Migration Path

1. Deploy with TLS disabled (current state)
2. Add TLS support to one backend for testing
3. Gradually enable TLS for all backends
4. Monitor performance and connection pool metrics

## Security Notes

- **Always verify certificates in production** (`tls_verify_cert = true`)
- Uses TLS 1.2 or higher (enforced by native-tls)
- System certificate store is automatically trusted
- Custom CAs can be added without replacing system trust
- Log TLS handshake failures for monitoring
- Consider adding mutual TLS (client certificates) support in future

## Troubleshooting

### Certificate Verification Failures

If you see `CertificateVerification` errors:

1. **Check server certificate**: Ensure the server's certificate is valid and not expired
2. **Verify hostname**: The certificate must match the hostname in the `host` field
3. **System certificates**: Ensure your OS certificate store is up to date
   - Linux: `sudo update-ca-certificates` (Debian/Ubuntu) or `sudo update-ca-trust` (RHEL/CentOS)
   - macOS: Certificates auto-update with system updates
   - Windows: Certificates auto-update via Windows Update

4. **Private CA**: If using a private CA, add it via `tls_cert_path`

### Debug Logging

Enable debug logging to see TLS handshake details:

```bash
RUST_LOG=debug ./nntp-proxy
```

Look for log lines like:
- `TLS: Using system certificate store for verification`
- `TLS: Adding custom CA certificate from: /path/to/cert.pem`
- `TLS: Connecting to hostname with TLS`

## References

- [tokio-native-tls documentation](https://docs.rs/tokio-native-tls)
- [native-tls documentation](https://docs.rs/native-tls)
- [tokio-rustls documentation](https://docs.rs/tokio-rustls) (alternative TLS implementation)
- RFC 4642: Using TLS with NNTP
