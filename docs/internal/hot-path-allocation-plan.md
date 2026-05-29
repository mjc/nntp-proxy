# Hot Path Allocation Plan

This document tracks the remaining allocation risks on the ARTICLE/BODY retrieval
hot path, starting from the client request and ending at client response write.

The target invariant is:

- RFC-sized client commands allocate nothing after connection setup.
- ARTICLE/BODY responses up to the normal article size use pooled buffers only.
- Unusually large multiline responses may use more pooled buffers.
- Heap allocation during response transfer is only acceptable for pool exhaustion,
  intentionally large multiline responses beyond configured pool capacity, or
  non-article commands such as XOVER/OVER/HDR.

## Current Hot Path Inventory

### Client Request Ingress

- `src/session/handlers/per_command.rs`
  - `handle_per_command_routing` allocates per connection via `TcpStream::into_split`,
    `Box::pin`, `BufReader::with_capacity`, and `SharedClientWriter::new`.
  - These are per-connection costs, not per-article costs, but they are still
    known allocations at the start of the path.

- `src/session/handlers/pipeline.rs`
  - `RequestBatch` stores pipelineable requests in `SmallVec<[RequestContext; 16]>`.
  - This is allocation-free up to the configured pipeline batch depth.
  - Complete command lines parse directly from `BufReader::buffer()`.
  - Split command lines copy into a fixed stack buffer, not heap.

- `src/protocol/request.rs`
  - `RequestContext` stores `verb` in `SmallVec<[u8; 16]>` and `args` in
    `SmallVec<[u8; 512]>`.
  - RFC-sized commands stay inline.
  - Non-RFC oversized commands can spill or be rejected.
  - The current parser copies verb and args even when the input line is already
    in the client read buffer.

### Backend Fetch And Response Buffering

- `src/session/handlers/command_execution.rs`
  - Each backend attempt acquires a pooled read buffer.
  - This is allocation-free if the regular buffer pool is sized correctly.
  - Pool exhaustion falls back to allocating a new buffer.

- `src/session/multiline_framing.rs` and `src/session/backend.rs`
  - Multiline responses are framed and completed through the framer/backend
    capture facade instead of caller-side boundary detection.
  - Retention paths can still move bytes into `ChunkedResponse` when ownership
    is required after the current write.
  - A packed suffix after a terminator can still freeze the current buffer into
    shared bytes and reacquire a pooled buffer. This is rare but still detaches
    that allocation from the normal pool lifecycle.

- `src/pool/buffer.rs`
  - `ChunkedResponse` has 16 inline response chunks.
  - More than 16 chunks can spill the `SmallVec`.
  - `write_all_to` writes chunks directly and no longer collects chunk references
    before writing.
  - Regular and capture buffer pool exhaustion still allocate.

### Cache And Precheck Side Effects

- `src/session/handlers/cache_operations.rs`
  - Payload cache upsert still owns `msg_id` and spawns a task.
  - Availability-only mode now skips no-op positive availability updates before
    `msg_id.to_owned()` and `tokio::spawn`.
  - Memory/hybrid cache modes still allocate for positive availability metadata.

- `src/session/precheck.rs`
  - Adaptive precheck allocates a `Vec<QueryResult>`.
  - It clones `RequestContext`, clones dependencies, spawns one task per backend,
    and collects via `FuturesUnordered`.
  - This is acceptable only while adaptive precheck is outside the normal
    zero-allocation retrieval path.

- `src/cache/hybrid.rs`
  - Hybrid cache operations convert message IDs to owned string keys.
  - This is disk/cache-mode cost, not pure pass-through ARTICLE/BODY cost.

## Ranked Fix Plan

### 1. Make Pool Exhaustion Observable And Enforceable

Add counters for:

- regular buffer pool fallback allocations
- capture buffer pool fallback allocations
- `ChunkedResponse` chunk metadata spills
- packed-suffix buffer freezes/detaches

Expose these through existing metrics or a bench-only snapshot API.

Acceptance criteria:

- `1/1/1` ARTICLE/BODY 100GB shows zero regular pool fallback allocations.
- `4/8/8` ARTICLE/BODY 100GB shows zero regular pool fallback allocations.
- Capture pool fallback is zero for pass-through cache-miss retrieval.

### 2. Remove `Box::pin` From Per-Connection Entry

Refactor `handle_per_command_routing` so the per-command loop does not require
boxing the future.

Candidate approaches:

- move the loop into a free async helper with smaller captured state
- split setup from loop execution to reduce generated future size
- avoid recursive or large enum state that forces boxing

This removes one per-connection allocation.

### 3. Avoid `SharedClientWriter` For Sequential Per-Command Delivery

The normal per-command path writes responses sequentially. It should use a direct
`&mut OwnedWriteHalf` where no backend worker needs shared writer ownership.

Plan:

- introduce a writer abstraction that can be either direct or shared
- route the normal path through direct mutable writer access
- keep `SharedClientWriter` only for code paths that genuinely need cross-task
  writer sharing

This removes an `Arc<Mutex<_>>` allocation and lock overhead from the common path.

### 4. Stop Copying RFC-Sized Request Lines Into Owned Verb/Args

Current request parsing is heap-free for RFC-sized commands but still copies
verb and args into `SmallVec`s.

Plan:

- store the full command line in a fixed inline buffer or batch-owned arena
- make `RequestContext` hold spans for verb, args, and message id
- preserve typed request classification and existing metadata fields
- keep owned fallback only for generated/internal requests or rare long commands

This should remove per-command byte copies without permitting heap allocation
for normal RFC-sized client requests.

### 5. Make `ChunkedResponse` Spill Policy Match The Contract

`ChunkedResponse` currently has 16 inline chunks. That is fine for typical
1 MiB buffers, but the spill behavior should be explicit.

Plan:

- add a test/metric for chunk metadata spills
- size inline chunks for all normal article responses under the configured
  buffer size
- allow spill only for unusually large multiline responses
- consider an `ArrayVec` plus explicit large-response fallback if we want a hard
  no-heap boundary

### 6. Handle Packed Suffixes Without Freezing Normal Read Buffers

Packed suffixes currently freeze the backing buffer into shared bytes. This is
correct, but it detaches the allocation from the regular pool.

Plan:

- add a small per-connection leftover slab/ring for packed status/request-sized
  suffixes
- copy only the suffix into that preallocated storage
- keep the main response buffer eligible to return to the regular pool
- retain the current shared-bytes fallback for unusually large suffixes

### 7. Gate Cache Update Allocations More Aggressively

Payload caching legitimately owns data. Pass-through retrieval should not pay
for cache work unless configured cache mode needs it.

Plan:

- keep availability-only positive updates as no-op before allocation
- for memory/hybrid metadata updates, batch or coalesce positive availability
  updates so `msg_id.to_owned()` and `tokio::spawn` happen only when useful
- consider an inline/synchronous fast path for cheap memory-cache metadata
  updates if it is lower overhead than spawning

### 8. Keep Adaptive Precheck Outside The Zero-Allocation Contract

Adaptive precheck is intentionally a backend fanout mode and currently allocates
and spawns per backend.

Plan if it becomes hot:

- replace per-backend `tokio::spawn` fanout with direct polling over a bounded
  set of backend futures
- avoid cloning full `RequestContext` for every backend
- replace `Vec<QueryResult>` with fixed inline storage sized by backend count
  where practical

Until then, keep `adaptive_precheck=false` for retrieval benchmarks that assert
the zero-allocation path.

### 9. Treat XOVER/OVER/HDR As Explicit Exceptions

XOVER, OVER, HDR, and similar list-oriented commands have different response
shape and can be legitimately large.

Plan:

- keep their buffering behavior separate from ARTICLE/BODY retrieval tests
- allow allocation where response shape requires it
- avoid complicating the ARTICLE/BODY hot path to support these commands

### 10. Add Allocation Regression Tests

Add a benchmark/test mode that can assert the allocation contract.

Options:

- global allocator counter around synthetic ARTICLE/BODY runs
- pool fallback counters exposed through a test-only metrics snapshot
- `nntpbench` scenario checks that fail if fallback allocation counters increase

Minimum assertions:

- RFC-sized client commands do not heap allocate after connection setup.
- ARTICLE/BODY responses up to normal size use pooled buffers only.
- Pass-through cache-miss retrieval does not use capture buffers.
- Large multiline responses may consume additional pooled buffers.
- Heap allocation only occurs when configured pools are exhausted or for
  explicitly exempt commands.
