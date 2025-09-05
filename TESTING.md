# Test Documentation

## Overview

The NNTP Proxy project includes comprehensive unit and integration tests covering all critical functionality.

## Test Structure

```
tests/
├── Unit Tests (src/lib.rs)          # 14 tests
└── Integration Tests                 # 4 tests
    └── tests/integration_tests.rs
```

## Unit Tests (14 tests)

Located in `src/lib.rs`, these tests cover the core library functionality:

### Configuration Tests
- `test_server_config_creation` - Tests ServerConfig struct creation
- `test_config_creation` - Tests Config struct with multiple servers  
- `test_config_serialization` - Tests TOML serialization/deserialization
- `test_create_default_config` - Tests default configuration creation

### Configuration Loading Tests  
- `test_load_config_from_file` - Tests loading config from TOML file
- `test_load_config_nonexistent_file` - Tests error handling for missing files
- `test_load_config_invalid_toml` - Tests error handling for malformed TOML

### Proxy Creation Tests
- `test_proxy_creation_with_servers` - Tests successful proxy creation
- `test_proxy_creation_with_empty_servers` - Tests error when no servers configured

### Round-Robin Load Balancing Tests
- `test_round_robin_server_selection` - Tests round-robin server selection across multiple servers
- `test_round_robin_with_single_server` - Tests behavior with single server
- `test_concurrent_round_robin` - Tests thread safety and fairness under concurrent access

### Data Proxying Tests
- `test_proxy_data_empty_stream` - Tests handling of empty data streams
- `test_proxy_data_with_data` - Tests bidirectional data forwarding

## Integration Tests (4 tests)

Located in `tests/integration_tests.rs`, these tests verify end-to-end functionality:

### Real Network Testing
- `test_proxy_with_mock_servers` - Tests complete proxy flow with mock NNTP servers
- `test_round_robin_distribution` - Tests actual round-robin behavior across multiple connections

### Configuration Integration
- `test_config_file_loading` - Tests loading configuration from actual TOML files

### Error Handling
- `test_proxy_handles_connection_failure` - Tests proxy behavior when backend servers are unavailable

## Running Tests

```bash
# Run all tests
cargo test

# Run only unit tests
cargo test --lib

# Run only integration tests  
cargo test --test integration_tests

# Run specific test
cargo test test_round_robin_server_selection

# Run tests with output
cargo test -- --nocapture

# List all available tests
cargo test -- --list
```

## Test Coverage

The tests provide comprehensive coverage of:

✅ **Configuration Management**
- TOML parsing and validation
- Error handling for invalid configs
- Default configuration generation

✅ **Round-Robin Load Balancing**  
- Proper server selection order
- Wraparound behavior
- Thread safety under concurrent load
- Single server edge case

✅ **Network Proxy Functionality**
- TCP connection handling
- Bidirectional data forwarding  
- Connection error handling
- Client-server lifecycle management

✅ **Data Integrity**
- Exact data forwarding without modification
- Proper stream handling and cleanup
- EOF and error condition handling

✅ **Concurrency and Safety**
- Thread-safe server selection
- Proper resource cleanup
- Async/await correctness

## Test Quality Features

- **Isolation**: Each test is independent and can run in any order
- **Deterministic**: Tests use controlled inputs and verify exact outputs
- **Comprehensive Error Testing**: Invalid inputs and failure scenarios are tested
- **Performance Testing**: Concurrent access patterns are validated
- **Real Network Conditions**: Integration tests use actual TCP connections
- **Temporary Resources**: Tests properly clean up temporary files and connections

## Continuous Integration

These tests are designed to run in CI environments and provide:
- Fast execution (completes in <1 second)
- No external dependencies  
- Reliable results across different systems
- Clear failure diagnostics
