# NNTP Proxy Architecture

**Last Updated:** October 16, 2025  
**Status:** Living document - updated as architecture evolves

## Overview

This document describes the architecture, module organization, and design decisions for the NNTP Proxy project. It serves as a guide for contributors and maintainers to understand the system structure and make informed decisions about future changes.

## Current Structure

### Module Organization

```
src/
├── lib.rs              # Public API, module orchestration
├── bin/                # Binary entry points
│   ├── nntp-proxy.rs           (290 lines)
│   └── nntp-cache-proxy.rs     (353 lines)
├── auth/               # Authentication handling
│   ├── mod.rs
│   ├── handler.rs
│   └── backend.rs              (237 lines)
├── cache/              # Article caching
│   ├── mod.rs
│   ├── article.rs
│   └── session.rs              (226 lines)
├── command/            # Command parsing & classification
│   ├── mod.rs
│   ├── handler.rs              (367 lines)
│   └── classifier.rs           (902 lines) ⚠️ TOO LARGE
├── config/             # Configuration management
│   ├── mod.rs
│   ├── types.rs
│   ├── loading.rs
│   ├── defaults.rs
│   └── validation.rs
├── health/             # Health checking
│   ├── mod.rs                  (388 lines)
│   └── types.rs
├── network/            # Network optimizations
│   ├── mod.rs
│   └── optimizers.rs           (295 lines)
├── pool/               # Connection & buffer pooling
│   ├── mod.rs
│   ├── provider.rs             (313 lines)
│   ├── buffer.rs               (310 lines)
│   ├── deadpool_connection.rs  (216 lines)
│   ├── connection_guard.rs     (202 lines)
│   ├── prewarming.rs           (209 lines)
│   ├── health_check.rs
│   └── connection_trait.rs
├── protocol/           # NNTP protocol
│   ├── mod.rs
│   ├── commands.rs
│   ├── responses.rs
│   └── response.rs             (916 lines) ⚠️ TOO LARGE
├── router/             # Backend selection & routing
│   └── mod.rs                  (610 lines) ⚠️ LARGE
├── session/            # Client session handling
│   ├── mod.rs
│   ├── handlers.rs             (754 lines) ⚠️ LARGE
│   ├── backend.rs
│   ├── connection.rs           (231 lines)
│   ├── streaming.rs            (350 lines)
│   ├── streaming/
│   │   └── tail_buffer.rs
│   └── tests.rs                (434 lines)
├── types/              # Core type definitions
│   ├── mod.rs
│   ├── config.rs               (849 lines) ⚠️ TOO LARGE
│   ├── protocol.rs             (434 lines)
│   ├── validated.rs            (482 lines)
│   └── metrics.rs              (380 lines)
├── connection_error.rs         (219 lines)
├── formatting.rs
├── proxy.rs                    (427 lines)
├── stream.rs                   (237 lines)
├── streaming.rs                (200 lines)
└── tls.rs                      (345 lines)
```

## Module Responsibilities

### Core Modules

#### `proxy.rs`
**Purpose:** Main proxy orchestration and lifecycle management  
**Responsibilities:**
- Initialize proxy with configuration
- Coordinate between modules
- Handle graceful shutdown

#### `session/`
**Purpose:** Client session management and command handling  
**Operating Modes:**
1. **Standard 1:1**: One backend connection per client
2. **Per-Command**: New backend for each command (stateless)
3. **Hybrid**: Switches between modes based on command type

**Key Files:**
- `handlers.rs` (754 lines) - Main session logic
- `backend.rs` - Backend connection management
- `streaming.rs` - Streaming response handling

#### `router/`
**Purpose:** Backend selection and load balancing  
**Responsibilities:**
- Round-robin load balancing
- Stateful connection tracking
- Backend health monitoring

#### `command/`
**Purpose:** NNTP command parsing and classification  
**Responsibilities:**
- Parse client commands
- Classify as stateful/stateless
- Handle authentication actions

#### `protocol/`
**Purpose:** NNTP protocol implementation  
**Responsibilities:**
- Response parsing (RFC 3977)
- Response code handling
- Message-ID validation

#### `pool/`
**Purpose:** Connection and buffer pooling  
**Responsibilities:**
- Backend connection pooling (deadpool)
- Buffer pooling (bytes)
- Connection health checks
- Pre-warming connections

### Supporting Modules

#### `auth/`
**Purpose:** Authentication handling  
**Backends:** File-based, in-memory

#### `cache/`
**Purpose:** Article caching  
**Implementation:** moka cache with TTL

#### `config/`
**Purpose:** Configuration management  
**Features:**
- TOML loading
- Validation
- Default values

#### `health/`
**Purpose:** Health check endpoint  
**Implementation:** HTTP endpoint for monitoring

#### `network/`
**Purpose:** Network optimizations  
**Features:**
- TCP_NODELAY
- SO_KEEPALIVE
- Buffer sizing

#### `types/`
**Purpose:** Core type definitions  
**Categories:**
- Config newtypes
- Protocol types
- Validated types
- Metrics types

## Issues Identified

### 1. ⚠️ Large Files (Should be split)

#### A. `protocol/response.rs` (916 lines)
**Current:** Monolithic response parsing file  
**Issues:**
- Mixes response code enum, parsing logic, validation, and utilities
- Hard to navigate and maintain
- Tests are mixed with implementation

**Recommendation:** Split into:
```
protocol/response/
├── mod.rs              # Public exports
├── codes.rs            # ResponseCode enum
├── parser.rs           # NntpResponse parsing
├── validation.rs       # Message-ID validation
└── terminator.rs       # Multiline terminator detection
```

#### B. `command/classifier.rs` (902 lines)
**Current:** Single large file with all command classification logic  
**Issues:**
- Massive match statement makes it hard to extend
- Command groupings (stateful/stateless) scattered
- Difficult to test individual command categories

**Recommendation:** Split into:
```
command/classifier/
├── mod.rs              # Main classification logic
├── stateful.rs         # Stateful commands (GROUP, NEXT, LAST, etc.)
├── stateless.rs        # Stateless commands (ARTICLE, HEAD, etc.)
├── auth.rs             # Authentication commands
└── capabilities.rs     # CAPABILITIES, HELP, DATE, etc.
```

#### C. `types/config.rs` ✓ (Completed - Split into 849 → ~400 lines total)
**Status:** DONE - Split into focused submodules  
**Result:** Cleaner organization, easier navigation

**Implemented structure:**
```
types/config/
├── mod.rs              # Re-exports and tests (414 lines)
├── network.rs          # Port (97 lines)
├── limits.rs           # MaxConnections, MaxErrors (142 lines)
├── buffer.rs           # BufferSize, WindowSize (134 lines)
├── cache.rs            # CacheCapacity (66 lines)
└── duration.rs         # Duration serialization helpers (53 lines)
```

#### D. `session/handlers.rs` (754 lines)
**Current:** All session handling in one file  
**Issues:**
- Multiple complex async functions
- Hard to test individual flows
- Mixes different routing modes

**Recommendation:** Split into:
```
session/handlers/
├── mod.rs              # Core ClientSession impl
├── standard.rs         # 1:1 mode handling
├── per_command.rs      # Per-command routing
├── hybrid.rs           # Hybrid mode switching
└── execution.rs        # Command execution (hot path)
```

#### E. `router/mod.rs` (610 lines)
**Current:** All routing logic in mod.rs  
**Issues:**
- Backend selection, load tracking, stateful management all mixed
- Should be split for clarity

**Recommendation:** Split into:
```
router/
├── mod.rs                  # Public API
├── selector.rs             # BackendSelector impl
├── load_balancing.rs       # Round-robin & load tracking
└── stateful.rs             # Stateful connection management
```

### 2. 📁 Module Organization Issues

#### A. Inconsistent Submodule Structure
**Problem:** No clear pattern for when to split files into modules  
**Impact:** Hard to predict where code should go

**Recommendation:** Establish rules:
- Files > 400 lines → Split into module directory
- Files with >3 distinct responsibilities → Split into module directory
- Files with extensive tests → Move tests to separate file

#### B. Unclear Module Boundaries
**Problem:** Naming conflicts and confusion
- `streaming.rs` at root vs `session/streaming.rs`
- `stream.rs` and `streaming.rs` - too similar
- `network.rs` and `network/` directory both exist

**Recommendation:**
- Consolidate `streaming.rs` → `session/streaming/`
- Rename `stream.rs` → `connection_stream.rs` or move to `pool/stream.rs`
- Move all network code into `network/` directory

#### C. Flat Type Organization
**Problem:** All types in one `types/` directory with no grouping

**Recommendation:** Reorganize types by domain:
```
types/
├── mod.rs
├── ids.rs              # ClientId, BackendId, RequestId
├── protocol.rs         # MessageId, etc. (keep as is)
├── validated.rs        # HostName, ServerName (keep as is)
├── metrics.rs          # BytesTransferred, etc. (keep as is)
└── config/             # Config types (split as shown above)
```

### 3. 🔄 Dependency Flow

#### Current Dependencies
```
┌─────────┐
│  proxy  │ (orchestrates everything)
└────┬────┘
     │
     ├─→ session ──→ router ──→ pool ──→ network
     ├─→ command
     ├─→ protocol
     ├─→ auth
     ├─→ cache
     └─→ config
```

#### Potential Issues
- `proxy.rs` knows about everything (god module syndrome)
- Potential circular dependencies between session/router/pool
- Types spread across multiple modules

**Current mitigation:** Good use of trait objects and Arc

**Recommendations:**
- Document dependency graph clearly
- Use `cargo-depgraph` to visualize
- Keep `proxy.rs` thin - just orchestration
- Consider extracting shared types

### 4. 🧪 Test Organization

#### Current State: Inconsistent
- Some modules: inline `#[cfg(test)]`
- Some modules: separate `tests.rs` files
- Integration tests: `tests/` directory

**Recommendation:** Standardize:
- **Unit tests:** Inline `#[cfg(test)]` for < 50 lines
- **Module tests:** Separate `tests.rs` for > 50 lines
- **Integration tests:** `tests/` directory

#### Coverage Gaps
- No tests for error paths with `#[cold]`
- Limited property-based testing
- No fuzz testing for protocol parsing

**Recommendation:**
- Add `proptest` for protocol parsing
- Add error injection tests
- Consider `cargo-fuzz` for protocol handlers

### 5. 🎯 API Design

#### Current Issues

**A. Inconsistent Public API**
- Some modules export everything (types)
- Some hide internals (command)
- No clear strategy

**Recommendation:**
- Document public API strategy in `lib.rs`
- Use `pub(crate)` more aggressively
- Consider sealed traits for extension points

**B. Builder Patterns ✓ (Implemented)**
- ✅ `NntpProxyBuilder` with fluent API
- ✅ Optional configuration overrides (buffer pool size/count)
- ✅ Backward compatibility maintained (`NntpProxy::new()` still works)

**Implemented API:**
```rust
NntpProxy::builder(config)
    .with_routing_mode(RoutingMode::Hybrid)
    .with_buffer_pool_size(512 * 1024)
    .with_buffer_pool_count(64)
    .build()?
```

**Future consideration:** Config builder for validation at construction time

### 6. 🔧 Code Quality

#### A. Magic Numbers
- Hard-coded buffer sizes
- Timeout values scattered
- No central constants for some values

**Recommendation:**
- Audit all magic numbers
- Move to `constants/` module with documentation
- Use const assertions for invariants

#### B. Error Handling
**Current:** Mix of `anyhow` and custom errors

**Recommendation:**
- Custom errors for public API
- `anyhow` for internal errors
- Document error handling strategy

## Priority Roadmap

### Phase 1: File Splits ✓ (Completed)
- [x] Split protocol/response.rs → protocol/response/
- [x] Split command/classifier.rs → command/classifier/
- [x] Split types/config.rs → types/config/
- [x] Update imports and tests

### Phase 2: Module Reorganization ✓ (Completed)
- [x] Fix streaming.rs confusion
- [x] Consolidate network code
- [x] Reorganize types
- [x] Update documentation

### Phase 3: API Improvements ✓ (Completed)
- [x] Add builder patterns
- [x] Document public API
- [x] Add examples

### Phase 4: Advanced Improvements (Future)
- [ ] Add property-based tests
- [ ] Improve error path coverage
- [ ] Seal appropriate traits
- [ ] Profile-guided optimization

## Success Metrics

- [x] No files > 500 lines (except tests)
- [x] Clear module boundaries documented
- [ ] 90%+ test coverage (current: ~80%)
- [x] Zero clippy warnings ✓
- [ ] Public API fully documented with examples
- [ ] Architecture decision records (ADRs) in place

## Design Principles

### Performance
- **Zero-cost abstractions:** All abstractions must compile to optimal code
- **Hot path optimization:** Critical paths marked with `#[inline]` and `#[cold]`
- **Connection pooling:** Reuse connections aggressively
- **Buffer pooling:** Minimize allocations in hot paths

### Maintainability
- **Module size:** Keep files under 500 lines
- **Single responsibility:** Each module has one clear purpose
- **Clear boundaries:** Minimize inter-module dependencies
- **Documentation:** All public APIs documented with examples

### Reliability
- **Type safety:** Use newtypes to prevent mistakes
- **Error handling:** Explicit error types, no panics in production
- **Testing:** Comprehensive unit, integration, and property tests
- **Graceful degradation:** Handle backend failures gracefully

## Extension Points

### Adding a New Routing Mode
1. Add variant to `RoutingMode` enum in `config/`
2. Implement handling in `session/handlers.rs`
3. Add tests in `session/tests.rs`
4. Update documentation

### Adding a New Command Type
1. Add command to `protocol/commands.rs`
2. Add classification in `command/classifier/`
3. Add handling in `command/handler.rs`
4. Update tests

### Adding a New Authentication Backend
1. Implement `AuthBackend` trait in `auth/backend.rs`
2. Add configuration in `config/types.rs`
3. Wire up in `auth/handler.rs`
4. Add tests

## Performance Characteristics

### Connection Pooling
- **Default pool size:** 10 connections per backend
- **Idle timeout:** 60 seconds
- **Connection reuse:** Aggressive reuse for stateless commands

### Caching
- **Cache type:** moka (concurrent, TTL-based)
- **Default capacity:** 10,000 articles
- **Eviction:** LRU with TTL

### Buffer Management
- **Read buffer:** 64KB (configurable)
- **Write buffer:** 64KB (configurable)
- **Buffer pooling:** Yes, via bytes crate

## Known Limitations

1. **Single-threaded per client:** Each client session runs on one task
2. **No persistent state:** Proxy state is in-memory only
3. **Limited NNTP support:** Focuses on core commands
4. **No TLS termination:** TLS pass-through only

## Future Considerations

1. **Metrics & Observability:** Add Prometheus metrics, structured logging
2. **Dynamic Configuration:** Hot-reload configuration without restart
3. **Advanced Routing:** Content-based routing, sticky sessions
4. **Protocol Extensions:** COMPRESS, STREAMING
5. **Performance:** SIMD for protocol parsing, io_uring support

## References

- [RFC 3977](https://tools.ietf.org/html/rfc3977) - NNTP Protocol
- [RFC 4643](https://tools.ietf.org/html/rfc4643) - NNTP Authentication
- [deadpool](https://docs.rs/deadpool) - Connection pooling
- [tokio](https://docs.rs/tokio) - Async runtime
- [moka](https://docs.rs/moka) - Concurrent cache

## Contributing

When making architectural changes:
1. Update this document
2. Run `cargo test` and `cargo clippy`
3. Ensure no performance regressions
4. Update relevant examples
5. Add tests for new functionality

---

**Note:** This is a living document. Update it as the architecture evolves.
