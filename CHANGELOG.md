# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.1] - 2025-10-27

### Fixed
- **Error handling and logging improvements**
  - Fixed spurious "broken pipe" error logs that masked real issues like authentication failures
  - Client disconnects after successful data transfer now correctly log at DEBUG level instead of ERROR/WARN
  - Authentication failures now prominently visible in logs with full context (backend name, host, port, username, server response)
  
### Changed
- **Code quality and maintainability**
  - Refactored per-command routing handler to eliminate nested helper functions
  - Reduced `per_command.rs` from 620 to 550 lines (-11%)
  - Flattened main command loop from 4-5 nesting levels to 2 maximum
  - Simplified error handling with inline logic instead of jumping between functions
  - Added `ErrorClassifier` utility for consistent error classification across codebase
  - Enhanced `ConnectionError` with classification methods (`is_client_disconnect()`, `is_authentication_error()`, `is_network_error()`)
  
- **Streaming improvements**
  - Refactored `handle_client_write_error()` to distinguish complete vs incomplete chunk disconnects
  - Complete chunk disconnects now log at DEBUG (normal behavior)
  - Incomplete chunk disconnects log at WARN (unusual but handled)
  
### Added
- **Test coverage**
  - Added 8 tests for ConnectionError classification
  - Added 7 tests for ErrorClassifier functionality
  - Added 3 tests for streaming error scenarios
  - Added 10 tests for message ID extraction
  - All 378 tests passing with 74.08% overall coverage

## [0.2.0] - 2025-10-09

### Added
- **SSL/TLS Support**
  - Full SSL/TLS implementation for secure connections
  - Configurable certificate validation
  - Stream abstraction layer to support both plain and TLS connections
  
- **Enhanced TCP Tuning**
  - Cross-platform TCP optimization with platform-specific features
  - Configurable socket buffer sizes
  - TCP_NODELAY support for reduced latency
  - Platform-specific optimizations (TCP_QUICKACK on Linux, TCP_NOPUSH on BSD/macOS)
  
- **Caching Proxy**
  - Article caching support to reduce backend bandwidth
  - Configurable cache capacity
  - LRU eviction policy
  - Bandwidth optimization for frequently accessed articles
  
- **Health Checking System**
  - Fast TCP-level health checks instead of expensive DATE commands
  - Configurable health check intervals and failure thresholds
  - Automatic backend recovery tracking
  - Health metrics for monitoring
  
- **Builder Patterns**
  - `ServerConfigBuilder` for cleaner test code
  - `ClientSessionBuilder` for flexible session creation
  - `DeadpoolConnectionProviderBuilder` for connection provider setup
  
### Changed
- **Architecture improvements**
  - Split session handlers into routing mode modules (standard, per-command, hybrid)
  - Reorganized types/config into focused submodules
  - Modularized codebase for better maintainability
  - Consolidated constants into dedicated module
  
- **Performance optimizations**
  - Line buffer reuse in streaming to avoid allocations
  - Optimized cache session memory operations
  - Capacity hints for allocations
  - Inline hints for frequently-called functions
  - Iterator combinators instead of manual loops
  
- **Configuration**
  - Added environment variable support for CLI and backend servers
  - Comprehensive configuration validation with error messages
  - Removed duplicate buffer constants
  
### Fixed
- **Per-command routing fixes**
  - Fixed multiline response detection
  - Removed unnecessary flush calls
  - Fixed duplicate greeting in per-command routing mode
  - Fixed QUIT command broken pipe errors
  
- **Connection management**
  - Fixed connection pool exhaustion by using correct pooled connection method
  - Fixed TCP stream corruption in connection recycling
  - Proper handling of fresh vs pooled connections
  
### Improved
- **Code quality**
  - Applied clippy suggestions for idiomatic Rust
  - Added comprehensive documentation with `#[must_use]` attributes
  - Improved error types with detailed context
  - Thread-safe environment variable tests
  - Reduced cloning overhead with `from_server_config` constructor

## [0.1.0] - 2025-09-05

### Added
- **Core NNTP Proxy Functionality**
  - High-performance NNTP proxy server
  - Connection pooling with deadpool
  - Multiple routing modes (standard, per-command, hybrid)
  - Round-robin load balancing across backend servers
  - Stateful and stateless connection modes
  
- **Authentication**
  - Client authentication interception
  - Backend authentication handling
  - Credential management
  
- **Connection Management**
  - Deadpool-based connection pooling
  - Connection prewarming
  - Graceful shutdown support
  - Signal handling (SIGTERM, SIGINT)
  
- **Performance Features**
  - High-throughput TCP optimizations
  - Efficient buffer management with buffer pools
  - Zero-copy operations where possible
  - Per-request connection provider trait
  
- **Configuration**
  - TOML-based configuration
  - Configurable buffer sizes
  - Pool size and timeout settings
  - Backend server configuration
  
- **Testing**
  - Comprehensive test coverage
  - Mock server infrastructure
  - Integration tests
  - Test helpers and utilities
  
- **Development Tools**
  - Git hooks for code quality
  - Nix flake for reproducible builds
  - Docker support
  - Development documentation

[0.2.1]: https://github.com/mjc/nntp-proxy/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/mjc/nntp-proxy/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mjc/nntp-proxy/releases/tag/v0.1.0
