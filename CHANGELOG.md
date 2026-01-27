# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] - 2026-01-27

### Added

- **Backend Server Tiering** ([#48](https://github.com/mjc/nntp-proxy/pull/48))
  - Servers can be assigned tier levels (0=primary, higher=backup/archive)
  - Tier-aware selection ensures lower-tier backends are prioritized for article requests
  - Configuration via file-based `tier = N` or environment `NNTP_SERVER_N_TIER=N`
  - Backwards compatible: servers without tier assignment default to tier 0

- **Tier-Aware Cache TTL** ([#48](https://github.com/mjc/nntp-proxy/pull/48))
  - Exponential TTL scaling: `effective_ttl = base_ttl * 2^tier`
  - Higher-tier backends (backups/archives) get exponentially longer cache retention
  - Dramatically reduces expensive queries to slow backup servers
  - Examples with 1-hour base TTL:
    - Tier 0 (primary): 1 hour
    - Tier 5 (backup): 32 hours (~1.3 days)
    - Tier 10 (archive): 1024 hours (~42.7 days)
    - Tier 15: ~1365 days (~3.7 years)

- **Hybrid Disk Cache** ([#47](https://github.com/mjc/nntp-proxy/pull/47))
  - New two-tier cache: memory + disk backing using foyer
  - Memory tier for hot articles, disk tier for larger capacity
  - Automatic LRU promotion for frequently accessed disk entries
  - LZ4 compression reduces disk usage by ~60%
  - Configuration via `[cache.disk]` section with path and capacity

- **Adaptive Precheck for STAT/HEAD** ([#46](https://github.com/mjc/nntp-proxy/pull/46))
  - Concurrent backend queries for STAT/HEAD commands
  - Returns first successful response immediately
  - Background task updates full backend availability
  - Reduces latency for article metadata lookups

### Changed

- **⚠️ Breaking: Default Backend Selection Strategy** ([#47](https://github.com/mjc/nntp-proxy/pull/47))
  - Default changed from `WeightedRoundRobin` to `LeastLoaded`
  - Existing deployments relying on previous default should explicitly set `backend_selection = "WeightedRoundRobin"` in config

- **Metrics Always Enabled** ([#47](https://github.com/mjc/nntp-proxy/pull/47)) - Metrics collection is no longer optional for better observability

- **⚠️ Breaking: Disk Cache Format** ([#48](https://github.com/mjc/nntp-proxy/pull/48)) - Existing disk cache will be discarded on upgrade due to format changes for tier-aware TTL

### Fixed

- **Windows Cross-Compilation with TUI** - Fixed build failures when cross-compiling for Windows from non-Windows hosts. The TUI binary's dependencies (ratatui, crossterm, sysinfo) pull windows-sys 0.61+ which requires Windows library stubs during cross-compilation. Now properly links against mingw-w64 libraries from nixpkgs in the flake-based build environment.
- **Stale Connection Handling** ([#46](https://github.com/mjc/nntp-proxy/pull/46)) - Fixed "overnight 430" bug where pooled connections become stale after idle periods, causing false article-not-found errors
- **Critical Pending Count Leak** - Fixed pending request count not being properly decremented in precheck path
- **Precheck Performance** - Fixed precheck stalling at 53k cache entries by using atomic moka APIs
- **Disk Cache Errors** - Improved error messages and handling for disk full scenarios

## [0.3.0] - 2025-11-21

### Added

- **Terminal User Interface (TUI)** ([#38](https://github.com/mjc/nntp-proxy/pull/38), [#40](https://github.com/mjc/nntp-proxy/pull/40))
  - New `nntp-proxy-tui` binary with real-time monitoring dashboard
  - Live metrics: connections, throughput, backend health, user activity
  - System resource monitoring: CPU usage, memory usage, peak tracking
  - Interactive log viewer with fullscreen mode ('L' hotkey)
  - Per-user statistics tracking with article counts
  - Per-backend statistics with load balancing visualization
  - Connection statistics and pool utilization graphs
  - Built with ratatui for responsive terminal UI

- **Comprehensive Metrics System**
  - Lock-free metrics collection using DashMap for zero-allocation recording
  - Modular metrics architecture: `MetricsCollector`, `MetricsSnapshot`
  - Backend statistics: bytes transferred, command counts, error tracking
  - User statistics: active users, commands per user, article tracking
  - Connection statistics: pool utilization, connection lifecycle tracking
  - Directional byte tracking: separate client-to-backend and backend-to-client metrics
  - Snapshot-based metrics export for TUI consumption (zero-copy)

- **Property-Based Testing** ([#40](https://github.com/mjc/nntp-proxy/pull/40))
  - Comprehensive proptest integration for all newtype wrappers
  - Reusable test macros: `test_nonzero_newtype_full!`, `test_validated_string!`
  - 10,000+ generated test cases covering edge cases (0, MAX, overflow)
  - Code reduction: 26% fewer test lines (571 lines) with 100x more coverage
  - Integration property tests for routing modes and session handling

- **Massive Test Coverage Improvements**
  - Overall coverage: 74% → 85%+ across codebase
  - 500+ new tests added across 30+ modules
  - Module-specific improvements:
    - `pool/buffer.rs`: 0% → 100% (+21 tests)
    - `session/mod.rs`: 62.5% → 100% (+15 tests)
    - `cache/article.rs`: 69% → 98.7% (+12 tests)
    - `config/types.rs`: 31% → 99.8% (+35 tests)
    - `types/protocol.rs`: 52% → 91% (+18 tests)
    - `pool/deadpool_connection.rs`: 54.5% → 78.7% (+24 tests)
  - New test suites: routing modes, session handling, metrics, cache helpers

- **API Improvements**
  - Shared CLI args module (`args.rs`) with `CommonArgs` and `CacheArgs`
  - `PooledBuffer::as_mut_slice()` for better API discoverability
  - `BufferPool::acquire()` replacing `get_buffer()` for clearer semantics
  - Type-safe directional byte tracking (`BytesSentToBackend`, `BytesReceivedFromBackend`)
  - Anonymous user constant centralization (`ANONYMOUS_USER`)

### Changed

- **Routing Mode Rename** - `RoutingMode::Standard` → `RoutingMode::Stateful` for clarity
- **API Renames for Clarity**
  - `CommandHandler::handle_command()` → `classify()`
  - `NntpCommand::classify()` → `parse()`
  - `ResponseCode` → `Response` (matches RFC 3977 terminology)
  - `DeadpoolConnectionProviderBuilder` → `Builder`
  - Removed `get_` prefixes, `_sync` suffixes from method names
  - Removed redundant `Config` suffix from config types

- **Code Quality & Refactoring**
  - Integrated caching directly into `ClientSession` - eliminated `CachingSession` duplication
  - Consolidated per-command routing handlers and shutdown signal handling
  - Extracted testable pure functions from session handlers
  - Functional style refactoring in cache logic
  - AuthResult changed from struct to enum for better type safety
  - Removed conditionals from match guards - pure pattern matching
  - Eliminated duplicate command parsing in cache path
  - Removed verbose debug logging to reduce code bloat (~200 lines)

- **Configuration Enhancements**
  - Added proxy config section with threading model configuration
  - Environment variable support for all configuration options
  - Extended timeout configuration options
  - Enhanced validation with better error messages

### Fixed

- **Test Infrastructure**
  - Fixed flaky tests and dead code warnings
  - Added missing `#[tokio::test]` attribute to async tests
  - Improved test isolation with random port allocation

### Performance

- **Lock-Free Metrics** - Zero-allocation metrics recording using atomic operations and DashMap
- **TUI Optimizations** - Eliminated snapshot cloning, optimized data conversions
- **Buffer Tuning** - Optimized for real-world article sizes (724KB average)

### Technical Details

- **Test Infrastructure**: Reusable test macros, property-based testing, MockNntpServer builder
- **Metrics Architecture**: Modular design with snapshot isolation, lock-free recording
- **TUI Stack**: ratatui + crossterm, system stats via sysinfo crate
- **Code Reduction**: 571 lines of test code eliminated while increasing coverage 100x
- **New Files**: 25+ new modules (metrics, TUI, test infrastructure)
- **Total Changes**: 24,591 insertions, 3,038 deletions across 112 files

## [0.2.3] - 2025-11-12

### Added

- **Domain Types Refactoring** ([#35](https://github.com/mjc/nntp-proxy/pull/35))
  - Comprehensive newtype pattern implementation across codebase
  - Type-safe newtypes for all domain values:
    - `Username`, `Password` - Validated authentication credentials
    - `StatusCode` - NNTP response status codes
    - `TransferMetrics` - Replaces (u64, u64) tuples
    - `AuthResult`, `QuitStatus` - Return value newtypes
    - `Timeout` newtypes for type-safe duration handling
    - Connection pool metrics: `AvailableConnections`, `CreatedConnections`, `InUseConnections`
  - Comprehensive config validation tests (25 new tests)
  - `MockNntpServer` builder pattern to eliminate test duplication
  - Test factory functions reducing boilerplate by ~800 lines
  - GitHub Actions CI workflow with cargo-nextest (7x faster)
  - **Fixes**: [#31](https://github.com/mjc/nntp-proxy/issues/31) - Config field naming inconsistencies
  - **Fixes**: [#33](https://github.com/mjc/nntp-proxy/issues/33) - Client auth appearing to be required

- **Docker Support** ([#36](https://github.com/mjc/nntp-proxy/pull/36))
  - Multi-stage Docker build with Rust nightly (for let-chains feature)
  - Environment variable configuration for containerized deployments
  - Backend server configuration via indexed env vars: `NNTP_SERVER_N_*`
  - Full TLS configuration support via environment variables
  - Health check configuration: max per cycle, pool timeout
  - Connection keepalive duration configuration
  - Docker Compose examples with .env.example pattern
  - Secure credential management (no hardcoded defaults)
  - **Fixes**: [#32](https://github.com/mjc/nntp-proxy/pull/32) - Docker build issues

### Fixed

- **Critical: Greeting Flush Bug** ([#37](https://github.com/mjc/nntp-proxy/pull/37))
  - Fixed connection resets for NNTP clients that send commands immediately after connecting
  - Root cause: greetings buffered in TCP send buffer without flush
  - Added `flush()` immediately after all greeting writes
  - Affects per-command and hybrid routing modes
  
- **223 Response Code Handling**
  - Backends commonly return 223 for missing articles (even on message-ID requests)
  - Changed log level from WARN to DEBUG for 223 responses
  - Added integration test demonstrating real-world backend behavior

- **Docker Build Issues**
  - Fixed empty NNTP_PROXY_THREADS env var causing parse errors
  - Fixed Duration type conversions from u64 environment variables
  - Fixed health check variable expansion in docker-compose
  - Improved error messages with specific variable names

- **Security Improvements**
  - Removed insecure credential defaults from docker-compose
  - Fail-fast behavior when required credentials missing

### Changed

- **Domain Types Code Quality** ([#35](https://github.com/mjc/nntp-proxy/pull/35))
  - Comprehensive newtype pattern across codebase for type safety
  - Consolidated config/pool/protocol types with macros
  - Code reduction: ~1,840 lines while improving type safety
  - Refactored integration tests: -800 lines of boilerplate
  - Improved Rust idioms: const fn, #[must_use], #[inline]
  - Extracted common auth and QUIT handling

- **Documentation**
  - Clarified greeting flow in code comments
  - Updated function documentation for greeting handling

### Technical Details

- **Greeting Flow**: Backend greetings consumed when connections created in pool; proxy sends own greeting with immediate flush to client
- **Type Safety**: All domain values now use validated newtypes (Username, Password, StatusCode, TransferMetrics, etc.)
- **Testing**: Added MockNntpServer builder, cargo-nextest CI, 25 config validation tests, random ports for parallel execution

## [0.2.2] - 2025-11-07

### Added

- **Client Authentication System** ([#29](https://github.com/mjc/nntp-proxy/pull/29))
  - Config-file based client authentication support
  - Credential validation against configured users
  - Authentication caching for improved performance
  - RFC-compliant NNTP response codes (480, 481, 482)
  - Comprehensive authentication security tests (bypass prevention, integration, security)
  - **Closes**: [#28](https://github.com/mjc/nntp-proxy/issues/28) - Basic user auth/permissions
  
### Changed

- **Performance Optimizations**
  - Introduced `PooledBuffer` type to eliminate repeated 64KB Vec allocations
  - Optimized authentication hot path to restore 80MB/s throughput
  - Eliminated Vec allocations in streaming functions
  - Reduced buffer allocation overhead with smarter initialization tracking
  
- **Code Quality**
  - Made `AuthHandler` more idiomatic Rust
  - Extracted `route_command_with_error_handling` helper to reduce duplication
  - Simplified `PooledBuffer` API to prevent undefined behavior
  - Removed unused macro parameters from `validated_string!`

### Fixed

- **Response Parsing**
  - Fixed bug in `has_spanning_terminator` with comprehensive test coverage
  - Proper tracking of initialized bytes in `PooledBuffer` to prevent reading uninitialized memory

### Improved

- **Test Coverage**
  - Added comprehensive authentication tests (bypass prevention, integration, security scenarios)
  - Added tests for RFC-compliant response codes
  - Added tests proving authentication design correctness
  - Overall test suite expanded significantly for authentication features

## [0.2.1] - 2025-10-27

### Fixed

- **Error handling and logging improvements** ([#24](https://github.com/mjc/nntp-proxy/pull/24))
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

[0.2.2]: https://github.com/mjc/nntp-proxy/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/mjc/nntp-proxy/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/mjc/nntp-proxy/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/mjc/nntp-proxy/releases/tag/v0.1.0
