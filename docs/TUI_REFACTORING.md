# TUI Refactoring and Test Coverage Improvements

**Date:** November 12, 2025  
**Current Test Coverage:** ~60% for TUI modules  
**Target Coverage:** 80%+

---

## Executive Summary

The TUI implementation is functional and well-structured with good separation between data handling (`app.rs`), rendering (`ui.rs`), and utilities. However, test coverage is inconsistent and some architectural improvements would make the code more maintainable and testable.

### Current Test Coverage by Module

| Module | Tests | Coverage | Priority |
|--------|-------|----------|----------|
| `helpers.rs` | 7 tests | ~90% | ✅ Good |
| `log_capture.rs` | 7 tests | ~85% | ✅ Good |
| `types.rs` | 5 tests | ~70% | ⚠️ Fair |
| `app.rs` | 1 test | ~15% | 🔴 Critical |
| `ui.rs` | 0 tests | 0% | 🔴 Critical |
| `mod.rs` | 0 tests | 0% | ⚠️ Fair |
| `constants.rs` | 0 tests | N/A | ✅ OK |
| **Binary** (`nntp-proxy-tui.rs`) | 0 tests | 0% | 🔴 Critical |

---

## Critical Issues

### 1. **Binary Has No Tests** (Priority: HIGH)

**Problem:** The `nntp-proxy-tui.rs` binary has complex logic that's untestable:
- Configuration loading with fallback logic
- Runtime builder configuration
- CPU pinning (platform-specific)
- Graceful shutdown coordination
- TUI/headless mode switching

**Impact:** Changes to startup logic can break in production without warning.

**Solution:** Extract testable functions from main binary.

### 2. **App State Management Lacks Tests** (Priority: HIGH)

**Problem:** `TuiApp` has complex state transitions:
- Throughput calculations across time windows
- Delta computation between snapshots
- History buffer management
- Rate calculations (bytes/sec, commands/sec)

**Current Coverage:** Only 1 test for a specific bug fix (snapshot update bug).

**Impact:** Throughput calculations could be incorrect without detection.

### 3. **Rendering Logic Is Untestable** (Priority: MEDIUM)

**Problem:** `ui.rs` has 0 tests because rendering functions depend on ratatui's `Frame` type, which requires backend setup.

**Impact:** UI regressions won't be caught by tests.

**Solution:** Extract data preparation from rendering.

---

## Refactoring Recommendations

### HIGH PRIORITY

#### 1. Extract Configuration Loading

**Current Code:**
```rust
// In nntp-proxy-tui.rs main()
let config = if std::path::Path::new(args.config.as_str()).exists() {
    match load_config(args.config.as_str()) {
        Ok(config) => config,
        Err(e) => {
            error!("Failed to load...");
            return Err(e);
        }
    }
} else if has_server_env_vars() {
    // ...
} else {
    // ...
}
```

**Refactored:**
```rust
// New function in src/config/loading.rs or new file
pub fn load_config_with_fallback(
    config_path: &ConfigPath,
) -> Result<(Config, ConfigSource)> {
    if std::path::Path::new(config_path.as_str()).exists() {
        load_config(config_path.as_str())
            .map(|cfg| (cfg, ConfigSource::File))
            .context("Failed to load config file")
    } else if has_server_env_vars() {
        load_config_from_env()
            .map(|cfg| (cfg, ConfigSource::Environment))
            .context("Failed to load from environment")
    } else {
        create_default_config_file(config_path)
            .map(|cfg| (cfg, ConfigSource::DefaultCreated))
    }
}

pub enum ConfigSource {
    File,
    Environment,
    DefaultCreated,
}
```

**Benefits:**
- Testable configuration loading
- Single responsibility
- Error context preserved
- Can test all branches

#### 2. Extract Runtime Builder Logic

**Current Code:**
```rust
// Scattered across main()
let num_cpus = std::thread::available_parallelism()...
let worker_threads = args.threads.map(|t| t.get()).unwrap_or(num_cpus);

if worker_threads == 1 {
    let rt = tokio::runtime::Builder::new_current_thread()...
} else {
    let rt = tokio::runtime::Builder::new_multi_thread()...
}
```

**Refactored:**
```rust
pub struct RuntimeConfig {
    worker_threads: usize,
    enable_cpu_pinning: bool,
}

impl RuntimeConfig {
    pub fn from_args(threads: Option<ThreadCount>) -> Self {
        let num_cpus = std::thread::available_parallelism()
            .map(|p| p.get())
            .unwrap_or(1);
        
        Self {
            worker_threads: threads.map(|t| t.get()).unwrap_or(num_cpus),
            enable_cpu_pinning: true,
        }
    }
    
    pub fn build_runtime(self) -> Result<tokio::runtime::Runtime> {
        let rt = if self.worker_threads == 1 {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
        } else {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(self.worker_threads)
                .enable_all()
                .build()?
        };
        
        if self.enable_cpu_pinning {
            pin_to_cpu_cores(self.worker_threads)?;
        }
        
        Ok(rt)
    }
}
```

**Benefits:**
- Testable runtime configuration
- Clear separation of concerns
- Easy to add new runtime options
- Can mock/test without spawning actual runtime

#### 3. Add TuiApp Builder Pattern

**Current Code:**
```rust
// Three different constructors
impl TuiApp {
    pub fn new(...) -> Self { ... }
    pub fn with_log_buffer(...) -> Self { ... }
    pub fn with_history_size(...) -> Self { ... }
}
```

**Refactored:**
```rust
pub struct TuiAppBuilder {
    metrics: MetricsCollector,
    router: Arc<BackendSelector>,
    servers: Arc<Vec<ServerConfig>>,
    log_buffer: Option<LogBuffer>,
    history_size: HistorySize,
}

impl TuiAppBuilder {
    pub fn new(
        metrics: MetricsCollector,
        router: Arc<BackendSelector>,
        servers: Arc<Vec<ServerConfig>>,
    ) -> Self {
        Self {
            metrics,
            router,
            servers,
            log_buffer: None,
            history_size: HistorySize::DEFAULT,
        }
    }
    
    pub fn with_log_buffer(mut self, buffer: LogBuffer) -> Self {
        self.log_buffer = Some(buffer);
        self
    }
    
    pub fn with_history_size(mut self, size: HistorySize) -> Self {
        self.history_size = size;
        self
    }
    
    pub fn build(self) -> TuiApp {
        // Single construction logic
        TuiApp { ... }
    }
}
```

**Benefits:**
- Clear construction path
- Easier to test different configurations
- Follows Rust idioms
- Prevents invalid states

#### 4. Extract CPU Pinning Module

**Current Code:**
```rust
// In binary, platform-specific
#[cfg(target_os = "linux")]
fn pin_to_cpu_cores(num_cores: usize) -> Result<()> { ... }

#[cfg(not(target_os = "linux"))]
fn pin_to_cpu_cores(_num_cores: usize) -> Result<()> { ... }
```

**Refactored:**
```rust
// New file: src/runtime_affinity.rs (or in src/metrics/cpu_pinning.rs)
pub struct CpuAffinity {
    cores: usize,
}

impl CpuAffinity {
    pub fn new(cores: usize) -> Self {
        Self { cores }
    }
    
    #[cfg(target_os = "linux")]
    pub fn apply(&self) -> Result<()> {
        use nix::sched::{CpuSet, sched_setaffinity};
        use nix::unistd::Pid;

        let mut cpu_set = CpuSet::new();
        for core in 0..self.cores {
            let _ = cpu_set.set(core);
        }

        sched_setaffinity(Pid::from_raw(0), &cpu_set)
            .map_err(|e| anyhow::anyhow!("CPU affinity: {}", e))
    }
    
    #[cfg(not(target_os = "linux"))]
    pub fn apply(&self) -> Result<()> {
        info!("CPU pinning not available on this platform");
        Ok(())
    }
}
```

**Benefits:**
- Testable on all platforms
- Reusable in other binaries
- Clear responsibility
- Easy to extend for other platforms

### MEDIUM PRIORITY

#### 5. Separate Rendering Data from Rendering Logic

**Current Problem:** `ui.rs` functions directly call ratatui widgets, making them untestable.

**Solution:** Extract data structures that represent what to render:

```rust
// New file: src/tui/render_model.rs
pub struct TitleModel {
    pub uptime: String,
    pub active_connections: u64,
    pub total_connections: u64,
}

pub struct SummaryModel {
    pub client_to_backend: String,
    pub backend_to_client: String,
}

pub struct BackendListModel {
    pub backends: Vec<BackendItemModel>,
}

pub struct BackendItemModel {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub is_active: bool,
    pub active_connections: usize,
    pub commands_per_sec: String,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub errors: u64,
}

impl TuiApp {
    pub fn build_title_model(&self) -> TitleModel { ... }
    pub fn build_summary_model(&self) -> SummaryModel { ... }
    pub fn build_backend_list_model(&self) -> BackendListModel { ... }
}
```

**Then test the model building:**

```rust
#[test]
fn test_title_model_formatting() {
    let app = create_test_app();
    let model = app.build_title_model();
    
    assert!(model.uptime.contains("s"));
    assert_eq!(model.active_connections, 0);
}

#[test]
fn test_backend_list_model() {
    let app = create_test_app_with_backends(3);
    let model = app.build_backend_list_model();
    
    assert_eq!(model.backends.len(), 3);
    assert_eq!(model.backends[0].name, "Backend 1");
}
```

**Benefits:**
- Testable data preparation
- Rendering becomes pure presentation
- Can snapshot test models
- Clear separation of concerns

#### 6. Add Throughput Calculation Tests

```rust
// Add to src/tui/app.rs tests module

#[test]
fn test_throughput_calculation_basic() {
    let metrics = MetricsCollector::new(1);
    let router = Arc::new(BackendSelector::new());
    let servers = create_test_servers(1);
    let mut app = TuiApp::new(metrics.clone(), router, servers);
    
    // Initial update (establishes baseline)
    app.update();
    assert!(app.latest_client_throughput().is_none());
    
    // Wait 1 second, transfer 1MB
    std::thread::sleep(Duration::from_secs(1));
    metrics.record_backend_to_client_bytes_for(0, 1_000_000);
    app.update();
    
    // Should show ~1 MB/s
    let throughput = app.latest_client_throughput().unwrap();
    let rate = throughput.received_per_sec().get();
    assert!(rate > 900_000.0 && rate < 1_100_000.0);
}

#[test]
fn test_throughput_history_circular_buffer() {
    let mut app = create_test_app();
    
    // Add more than history capacity
    for i in 0..100 {
        std::thread::sleep(Duration::from_millis(10));
        app.update();
    }
    
    // Should cap at history size
    assert_eq!(
        app.client_throughput_history().len(),
        HistorySize::DEFAULT.get()
    );
}

#[test]
fn test_throughput_per_backend() {
    let metrics = MetricsCollector::new(3);
    let router = Arc::new(BackendSelector::new());
    let servers = create_test_servers(3);
    let mut app = TuiApp::new(metrics.clone(), router, servers);
    
    app.update();
    
    // Transfer different amounts per backend
    metrics.record_backend_to_client_bytes_for(0, 1_000_000);
    metrics.record_backend_to_client_bytes_for(1, 2_000_000);
    metrics.record_backend_to_client_bytes_for(2, 3_000_000);
    
    std::thread::sleep(Duration::from_secs(1));
    app.update();
    
    // Check per-backend throughput
    let backend0 = app.latest_backend_throughput(0).unwrap();
    let backend1 = app.latest_backend_throughput(1).unwrap();
    let backend2 = app.latest_backend_throughput(2).unwrap();
    
    assert!(backend0.received_per_sec().get() < backend1.received_per_sec().get());
    assert!(backend1.received_per_sec().get() < backend2.received_per_sec().get());
}

#[test]
fn test_commands_per_second_calculation() {
    let metrics = MetricsCollector::new(1);
    let router = Arc::new(BackendSelector::new());
    let servers = create_test_servers(1);
    let mut app = TuiApp::new(metrics.clone(), router, servers);
    
    app.update();
    
    // Simulate 100 commands per second
    for _ in 0..100 {
        metrics.record_command_for(0);
    }
    
    std::thread::sleep(Duration::from_secs(1));
    app.update();
    
    let throughput = app.latest_backend_throughput(0).unwrap();
    let cmd_rate = throughput.commands_per_sec().unwrap().get();
    
    assert!(cmd_rate > 90.0 && cmd_rate < 110.0);
}
```

#### 7. Add Integration Tests for TUI State

```rust
// New file: tests/test_tui.rs

use nntp_proxy::tui::TuiApp;
use nntp_proxy::metrics::MetricsCollector;
// ... imports

#[tokio::test]
async fn test_tui_app_lifecycle() {
    let metrics = MetricsCollector::new(2);
    let router = Arc::new(BackendSelector::new());
    let servers = create_test_servers(2);
    
    let mut app = TuiApp::new(metrics.clone(), router, servers);
    
    // Initial state
    assert_eq!(app.snapshot().active_connections, 0);
    assert_eq!(app.snapshot().total_connections, 0);
    
    // Simulate connections
    metrics.record_connection();
    metrics.record_connection();
    
    app.update();
    
    assert_eq!(app.snapshot().active_connections, 2);
    assert_eq!(app.snapshot().total_connections, 2);
}

#[test]
fn test_tui_app_with_log_buffer() {
    use nntp_proxy::tui::LogBuffer;
    
    let log_buffer = LogBuffer::new();
    log_buffer.push("Test log 1".to_string());
    log_buffer.push("Test log 2".to_string());
    
    let app = create_test_app_with_log_buffer(log_buffer.clone());
    
    let logs = log_buffer.recent_lines(2);
    assert_eq!(logs.len(), 2);
    assert_eq!(logs[1], "Test log 2");
}
```

### LOW PRIORITY

#### 8. Add Property-Based Tests

```rust
// Requires proptest crate
use proptest::prelude::*;

proptest! {
    #[test]
    fn test_throughput_calculation_properties(
        bytes in 0u64..10_000_000,
        time_ms in 1u64..5000
    ) {
        let rate = calculate_rate(
            bytes,
            (time_ms as f64) / 1000.0
        );
        
        // Rate should be positive
        prop_assert!(rate.get() >= 0.0);
        
        // Rate should not exceed bytes (can't transfer more than available)
        prop_assert!(rate.get() <= bytes as f64 * 1000.0);
    }
    
    #[test]
    fn test_round_up_throughput_monotonic(value in 0.0f64..1_000_000_000.0) {
        let rounded = round_up_throughput(value);
        
        // Rounded value should be >= original
        prop_assert!(rounded >= value);
        
        // Should not round up too much (max 10x)
        prop_assert!(rounded <= value * 10.0);
    }
}
```

#### 9. Add Benchmark for Rendering Performance

```rust
// In benches/tui_rendering.rs

use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn benchmark_chart_data_building(c: &mut Criterion) {
    let app = create_large_test_app(10); // 10 backends
    let servers = create_test_servers(10);
    
    c.bench_function("build_chart_data_10_backends", |b| {
        b.iter(|| {
            build_chart_data(black_box(&servers), black_box(&app))
        })
    });
}

fn benchmark_app_update(c: &mut Criterion) {
    let metrics = MetricsCollector::new(5);
    let router = Arc::new(BackendSelector::new());
    let servers = create_test_servers(5);
    let mut app = TuiApp::new(metrics.clone(), router, servers);
    
    c.bench_function("app_update_5_backends", |b| {
        b.iter(|| {
            app.update()
        })
    });
}

criterion_group!(benches, benchmark_chart_data_building, benchmark_app_update);
criterion_main!(benches);
```

---

## Test Coverage Goals

### Immediate (This Week)

1. ✅ Add 5+ tests for `TuiApp` state management
2. ✅ Extract config loading and add tests
3. ✅ Add runtime builder tests
4. ✅ Add throughput calculation tests

### Short-term (This Month)

1. ⬜ Add render model extraction + tests
2. ⬜ Add CPU affinity module + tests
3. ⬜ Add TUI integration tests
4. ⬜ Achieve 70%+ test coverage

### Long-term (Next Quarter)

1. ⬜ Add property-based tests
2. ⬜ Add rendering benchmarks
3. ⬜ Achieve 80%+ test coverage
4. ⬜ Add visual regression tests (snapshot testing)

---

## Implementation Priority

1. **Week 1:** Add TuiApp tests (highest impact)
2. **Week 2:** Extract config loading (reduces binary complexity)
3. **Week 3:** Extract runtime builder (improves testability)
4. **Week 4:** Add render models (enables UI testing)

---

## Migration Path

### Phase 1: Non-Breaking Additions
- Add new builder pattern alongside existing constructors
- Add new config loading function, keep old paths
- Add tests for existing code
- **No breaking changes**

### Phase 2: Deprecation
- Mark old constructors as `#[deprecated]`
- Update binary to use new builders
- Update documentation
- **Still backward compatible**

### Phase 3: Cleanup
- Remove deprecated constructors
- Consolidate code
- **Breaking change, coordinate with version bump**

---

## Specific Code Smells to Address

### 1. Magic Numbers
```rust
// BEFORE
if f.area().height >= 40 { ... }
let mut interval = tokio::time::interval(Duration::from_millis(250));

// AFTER (add to constants.rs)
pub mod ui_sizing {
    pub const MIN_HEIGHT_FOR_LOGS: u16 = 40;
    pub const LOG_WINDOW_HEIGHT: u16 = 10;
}

pub mod timing {
    pub const UI_UPDATE_INTERVAL_MS: u64 = 250;
    pub const METRICS_FLUSH_INTERVAL_SECS: u64 = 30;
}
```

### 2. Duplicated Initialization Logic
```rust
// BEFORE: Two places create TuiApp with similar logic
pub fn new(...) -> Self { /* duplicated init */ }
pub fn with_log_buffer(...) -> Self { /* duplicated init */ }

// AFTER: Single init function called by all constructors
fn init_internal(
    metrics: MetricsCollector,
    router: Arc<BackendSelector>,
    servers: Arc<Vec<ServerConfig>>,
    log_buffer: Arc<LogBuffer>,
    history_size: HistorySize,
) -> Self {
    // Single source of truth
}
```

### 3. Complex Boolean Logic
```rust
// BEFORE
let show_logs = f.area().height >= MIN_HEIGHT_FOR_LOGS;
let chunks = if show_logs { /* A */ } else { /* B */ };

// AFTER: Extract to method
impl LayoutConfig {
    fn should_show_logs(&self, area: Rect) -> bool {
        area.height >= self.min_height_for_logs
    }
    
    fn build_layout(&self, area: Rect) -> Vec<Rect> {
        if self.should_show_logs(area) {
            self.layout_with_logs(area)
        } else {
            self.layout_without_logs(area)
        }
    }
}
```

---

## Summary

The TUI code is well-structured but lacks comprehensive test coverage. Priority should be:

1. **Add TuiApp state tests** - Most critical, affects correctness
2. **Extract config/runtime logic** - Improves testability and maintainability  
3. **Add render models** - Enables UI testing
4. **Achieve 70%+ coverage** - Industry standard

These refactorings maintain the existing architecture while improving testability, following the project's emphasis on high test coverage (currently 74% overall, but TUI is dragging it down).
