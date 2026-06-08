# Type Audit Refactor Plan

## Goal

Reduce unnecessary ownership indirection and clean up APIs that currently expose `Arc` or other shared wrappers where plain references or flatter internal representations would be clearer and cheaper.

## Findings To Address

1. `Arc` leaks through many APIs as `&Arc<T>` or `-> &Arc<T>` instead of `&T`.
2. `CommandGuard` stores `Arc<BackendSelector>`, which pushes `Arc` requirements upward into unrelated router/session helpers.
3. `NntpProxy` stores `Arc<AtomicU64>` and `Arc<AtomicUsize>` for fields that appear to be owned exclusively by the already-`Arc`-shared proxy.
4. `ConnectionStatsAggregator` clones a struct containing several independent `Arc` fields instead of one shared inner object.
5. A few smaller cleanup candidates remain low priority (`Range::clone()`, non-hot-path lock shapes), but they should come after the structural fixes.

## Phase 1: Narrow Arc-Heavy APIs

### Targets

- `src/proxy/mod.rs`
- `src/bin/nntp-proxy.rs`
- `src/runtime.rs`
- `src/session/common.rs`
- `src/session/handlers/cache_operations.rs`
- `src/session/handlers/command_execution.rs`
- `src/session/handlers/per_command.rs`
- `src/session/handlers/article_retry.rs`
- `src/tui/app.rs`

### Changes

- Change helpers that currently take `&Arc<T>` to take `&T` when they only need borrowed access.
- Change accessors that currently return `&Arc<T>` to return `&T` when callers do not need the container type.
- Keep `Arc` only at actual shared-ownership boundaries:
  - task spawning
  - guard/storage fields that truly own a share
  - builder/session state that must retain ownership across async boundaries

### Validation

- `nix develop -c cargo check`
- `nix develop -c cargo clippy --all-targets --all-features`

## Phase 2: Decouple CommandGuard From Arc Leakage

### Targets

- `src/router/mod.rs`
- dependent call sites in `src/session/handlers/*`

### Changes

- Rework `CommandGuard` so callers that only need routing behavior do not have to traffic in `Arc<BackendSelector>`.
- Decide whether the right shape is:
  - keep owned `Arc` only at guard construction boundaries, while upstream APIs use `&BackendSelector`, or
  - move completion responsibility to a narrower abstraction that hides the `Arc`.
- Avoid a fake cleanup that only changes parameter spelling while still forcing `Arc` everywhere.

### Risks

- Drop semantics must remain exact; pending counts cannot drift.
- Guard changes affect error paths, so regression risk is concentrated here.

### Validation

- targeted router/session tests around pending-count behavior
- targeted hybrid/per-command/article retry tests

## Phase 3: Flatten Proxy Atomics

### Targets

- `src/proxy/mod.rs`
- `src/proxy/builder.rs`
- `src/proxy/lifecycle.rs`
- runtime call sites that read idle/active state

### Changes

- Verify `last_activity_nanos` and `active_clients` do not need independent cloning.
- If confirmed, replace:
  - `Arc<AtomicU64>` -> `AtomicU64`
  - `Arc<AtomicUsize>` -> `AtomicUsize`
- Keep access patterns identical from `&self` methods on `NntpProxy`.

### Risks

- Only safe if no field-level sharing escapes the proxy object.
- Must re-check tests and helper code for hidden `Arc::clone` use before editing.

## Phase 4: Consolidate ConnectionStatsAggregator

### Targets

- `src/metrics/connection_stats.rs`
- `src/proxy/lifecycle.rs`
- `src/session/core.rs`
- `src/runtime.rs`

### Changes

- Refactor `ConnectionStatsAggregator` to mirror `MetricsCollector`:
  - one outer handle struct
  - one inner shared struct behind a single `Arc`
- Move maps, sets, and `last_flush` into the inner struct.
- Preserve existing public behavior and clone semantics.

### Expected Payoff

- Fewer refcount bumps per clone
- cleaner ownership model
- easier future extension

## Phase 5: Small Follow-Up Cleanups

### Candidates

- remove redundant `Range::clone()` uses
- re-check oversized enums or boxed payloads only where clippy or profiling indicates real wins
- leave justified shapes alone (`ConnectionStream` boxing, precheck boxing, async/shared writer mutex)

## Implementation Order

1. Narrow API signatures and accessors first.
2. Refactor `CommandGuard` once upstream signatures are cleaner.
3. Flatten proxy atomics after confirming no field-level sharing escapes.
4. Consolidate `ConnectionStatsAggregator`.
5. Do minor cleanup only after the structural work settles.

## Verification Strategy

After each phase:

1. `nix develop -c cargo check`
2. `nix develop -c cargo clippy --all-targets --all-features`
3. run the smallest relevant targeted tests

At the end:

1. `nix develop -c cargo nextest run`
2. review for accidental ownership regressions, especially:
   - spawned tasks
   - guard drop behavior
   - session/router lifetime boundaries

## Non-Goals

- Do not change multiline framing behavior.
- Do not rewrite justified shared ownership patterns just to reduce `Arc` count.
- Do not chase style-only cleanups that do not improve ownership semantics or hot-path structure.
