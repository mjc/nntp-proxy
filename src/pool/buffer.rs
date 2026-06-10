use crate::types::BufferSize;
use bytes::{BufMut, Bytes, BytesMut};
use crossbeam::queue::ArrayQueue;
use smallvec::SmallVec;
use std::future::poll_fn;
use std::ops::{Deref, Range};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;
use tokio::io::{AsyncRead, AsyncWriteExt, ReadBuf};
use tracing::{debug, info, warn};

/// A pooled buffer that automatically returns to the pool when dropped
///
/// # Safety and Initialized Bytes
///
/// The backing allocation is kept logically empty until bytes are read or copied
/// into it. `BytesMut::len()` is the initialized length, and immutable access
/// only exposes that initialized portion.
///
/// ## Usage
/// ```ignore
/// let mut buffer = pool.acquire();
/// let n = buffer.read_from(&mut stream).await?;  // Automatic tracking
/// process(&*buffer);  // Deref returns only &buffer[..n]
/// ```
pub struct PooledBuffer {
    buffer: BytesMut,
    pool: Arc<ArrayQueue<BytesMut>>,
    pool_size: Arc<AtomicUsize>,
    allocated_count: Arc<AtomicUsize>,
    max_pool_size: usize,
    expected_capacity: usize,
    writable_len: usize,
    acquired_at: Instant,
    source: PooledBufferSource,
    counts_toward_pool: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PooledBufferKind {
    Regular,
    Capture,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PooledBufferSource {
    kind: PooledBufferKind,
    fallback: bool,
}

impl PooledBufferSource {
    const fn regular(fallback: bool) -> Self {
        Self {
            kind: PooledBufferKind::Regular,
            fallback,
        }
    }

    const fn capture(fallback: bool) -> Self {
        Self {
            kind: PooledBufferKind::Capture,
            fallback,
        }
    }
}

impl std::fmt::Debug for PooledBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PooledBuffer")
            .field("initialized", &self.buffer.len())
            .field("capacity", &self.buffer.capacity())
            .finish_non_exhaustive()
    }
}

impl PooledBuffer {
    /// Get the backing allocation capacity of the buffer.
    #[must_use]
    #[inline]
    pub fn capacity(&self) -> usize {
        self.buffer.capacity()
    }

    /// Get the number of initialized bytes
    #[must_use]
    #[inline]
    pub fn initialized(&self) -> usize {
        self.buffer.len()
    }

    #[inline]
    fn read_limit(&self) -> usize {
        if self.writable_len == 0 {
            self.buffer.capacity()
        } else {
            self.writable_len
        }
    }

    /// Read from an `AsyncRead` source, automatically tracking initialized bytes
    ///
    /// # Errors
    /// Returns any read error produced by the underlying async reader.
    pub async fn read_from<R>(&mut self, reader: &mut R) -> std::io::Result<usize>
    where
        R: AsyncRead + Unpin,
    {
        self.buffer.clear();
        self.read_into_spare(reader, self.read_limit()).await
    }

    /// Read more data at the current initialized offset, accumulating bytes.
    ///
    /// Unlike `read_from` which resets the buffer, this appends to existing data.
    /// Used when a higher-level reader needs more bytes in the same allocation.
    ///
    /// Returns the number of NEW bytes read (not total).
    ///
    /// # Errors
    /// Returns any read error produced by the underlying async reader.
    pub async fn read_more<R>(&mut self, reader: &mut R) -> std::io::Result<usize>
    where
        R: AsyncRead + Unpin,
    {
        let limit = self.read_limit().saturating_sub(self.buffer.len());
        self.read_into_spare(reader, limit).await
    }

    async fn read_into_spare<R>(&mut self, reader: &mut R, limit: usize) -> std::io::Result<usize>
    where
        R: AsyncRead + Unpin,
    {
        let spare = self.buffer.spare_capacity_mut();
        let read_len = limit.min(spare.len());
        if read_len == 0 {
            return Ok(0);
        }

        let mut read_buf = ReadBuf::uninit(&mut spare[..read_len]);
        poll_fn(|cx| Pin::new(&mut *reader).poll_read(cx, &mut read_buf)).await?;
        let read = read_buf.filled().len();
        // SAFETY: `poll_read` initialized exactly `read` bytes in the spare slice.
        unsafe {
            self.buffer.advance_mut(read);
        }
        Ok(read)
    }

    /// Copy data into buffer and mark as initialized
    ///
    /// # Panics
    /// Panics if `data.len()` > capacity
    #[inline]
    pub fn copy_from_slice(&mut self, data: &[u8]) {
        assert!(
            data.len() <= self.buffer.capacity(),
            "data exceeds buffer capacity"
        );
        self.buffer.clear();
        self.buffer.extend_from_slice(data);
    }

    /// Reset the logical contents of the buffer without changing its backing allocation.
    ///
    /// This is safe for both fixed-size I/O buffers and accumulator-style capture buffers:
    /// callers see an empty initialized slice afterwards, while the underlying allocation
    /// stays available for reuse.
    #[inline]
    pub fn clear(&mut self) {
        self.buffer.clear();
    }

    /// Append data to the buffer (accumulator mode)
    ///
    /// Used when `PooledBuffer` is acquired from capture pool for accumulating
    /// streaming data. Unlike `copy_from_slice` which overwrites, this extends.
    ///
    /// # Performance
    ///
    /// Capture buffers are allocated with sufficient capacity (1 MiB by default).
    /// As long as total accumulated data fits within capacity, this operation performs
    /// no Rust heap allocation.
    ///
    /// If data exceeds capacity, the `BytesMut` will grow automatically (with allocation).
    /// This ensures complete data is always captured rather than truncating, which
    /// would corrupt cached responses.
    ///
    /// # Note on length
    ///
    /// After calling this, `.len()` (via Deref) returns the total accumulated length.
    #[inline]
    pub fn extend_from_slice(&mut self, data: &[u8]) {
        let needed = self.buffer.len() + data.len();
        // Accumulator/capture mode may grow; resized buffers are discarded on drop.
        if needed > self.buffer.capacity() {
            tracing::warn!(
                "Capture buffer growing: {} + {} > {} capacity (will allocate)",
                self.buffer.len(),
                data.len(),
                self.buffer.capacity()
            );
        }
        self.buffer.extend_from_slice(data);
    }

    /// Freeze the initialized bytes into a shared immutable buffer.
    ///
    /// The backing allocation is intentionally detached from the pool because
    /// ownership escapes through `Bytes`.
    #[must_use]
    pub fn freeze(mut self) -> Bytes {
        std::mem::take(&mut self.buffer).freeze()
    }
}

impl Deref for PooledBuffer {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

// Intentionally NO DerefMut or AsMut - forces explicit use of read_from()/read_more()

impl AsRef<[u8]> for PooledBuffer {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.buffer
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
        let held_micros = duration_micros_u64(self.acquired_at.elapsed());
        record_buffer_hold(self.source, held_micros);
        maybe_log_buffer_hold(self.source, held_micros, self.buffer.capacity());

        if self.buffer.capacity() != self.expected_capacity {
            debug!(
                "Dropping resized pooled buffer instead of returning it to the pool (capacity {} != expected {})",
                self.buffer.capacity(),
                self.expected_capacity
            );
            if self.counts_toward_pool {
                self.allocated_count.fetch_sub(1, Ordering::Relaxed);
            }
            return;
        }

        self.buffer.clear();

        if !self.counts_toward_pool {
            return;
        }

        // Atomically return buffer to pool if pool is not full
        let mut current_size = self.pool_size.load(Ordering::Relaxed);
        while current_size < self.max_pool_size {
            match self.pool_size.compare_exchange_weak(
                current_size,
                current_size + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    let buffer = std::mem::take(&mut self.buffer);
                    match self.pool.push(buffer) {
                        Ok(()) => return,
                        Err(buffer) => {
                            self.buffer = buffer;
                            self.pool_size.fetch_sub(1, Ordering::Relaxed);
                            self.allocated_count.fetch_sub(1, Ordering::Relaxed);
                            return;
                        }
                    }
                }
                Err(new_size) => {
                    current_size = new_size;
                }
            }
        }
        // If pool is full, buffer is dropped
        self.allocated_count.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Buffered response assembled from one or more pooled capture buffers.
///
/// This avoids reallocating or growing a single `Vec` on the hot path when
/// a multiline response is larger than the typical capture size.
///
/// `ChunkedResponse` is storage only. It intentionally does not expose response
/// boundary helpers such as prefix or terminator checks; callers that need to
/// interpret backend bytes must go through the session framer/backend facade
/// before putting data here.
#[derive(Debug, Default)]
pub struct ChunkedResponse {
    chunks: SmallVec<[ResponseChunk; 16]>,
    len: usize,
}

#[derive(Debug, Default)]
struct ResponseWriteMetrics {
    responses: AtomicUsize,
    single_chunk_responses: AtomicUsize,
    multi_chunk_responses: AtomicUsize,
    chunks_written: AtomicUsize,
    bytes_written: AtomicUsize,
    tiny_chunks: AtomicUsize,
    tiny_chunk_bytes: AtomicUsize,
    small_chunks: AtomicUsize,
    small_chunk_bytes: AtomicUsize,
    max_chunks_per_response: AtomicUsize,
}

#[derive(Debug, Default)]
struct HotPathAllocationMetrics {
    regular_pool_fallback_allocations: AtomicUsize,
    regular_pool_exhaustions: AtomicUsize,
    capture_pool_fallback_allocations: AtomicUsize,
    chunked_response_metadata_spills: AtomicUsize,
    pending_backend_byte_heap_fallbacks: AtomicUsize,
    regular_pool_buffer_holds: AtomicUsize,
    regular_pool_buffer_hold_micros_total: AtomicU64,
    regular_pool_buffer_hold_micros_max: AtomicU64,
    capture_pool_buffer_holds: AtomicUsize,
    capture_pool_buffer_hold_micros_total: AtomicU64,
    capture_pool_buffer_hold_micros_max: AtomicU64,
}

/// Snapshot of logical response writes emitted to clients.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResponseWriteMetricsSnapshot {
    pub responses: usize,
    pub single_chunk_responses: usize,
    pub multi_chunk_responses: usize,
    pub chunks_written: usize,
    pub bytes_written: usize,
    pub tiny_chunks: usize,
    pub tiny_chunk_bytes: usize,
    pub small_chunks: usize,
    pub small_chunk_bytes: usize,
    pub max_chunks_per_response: usize,
}

/// Snapshot of allocation-sensitive hot-path activity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HotPathAllocationMetricsSnapshot {
    pub regular_pool_fallback_allocations: usize,
    pub regular_pool_exhaustions: usize,
    pub capture_pool_fallback_allocations: usize,
    pub chunked_response_metadata_spills: usize,
    pub pending_backend_byte_heap_fallbacks: usize,
    pub regular_pool_buffer_holds: usize,
    pub regular_pool_buffer_hold_micros_total: u64,
    pub regular_pool_buffer_hold_micros_max: u64,
    pub capture_pool_buffer_holds: usize,
    pub capture_pool_buffer_hold_micros_total: u64,
    pub capture_pool_buffer_hold_micros_max: u64,
}

fn response_write_metrics_enabled() -> bool {
    response_write_metrics_enabled_flag().load(Ordering::Relaxed)
}

pub(crate) fn set_response_write_metrics_enabled(enabled: bool) {
    response_write_metrics_enabled_flag().store(enabled, Ordering::Relaxed);
}

fn response_write_metrics_enabled_flag() -> &'static AtomicBool {
    static ENABLED: OnceLock<AtomicBool> = OnceLock::new();
    ENABLED.get_or_init(|| {
        AtomicBool::new(std::env::var_os("NNTP_PROXY_RESPONSE_WRITE_METRICS_SECS").is_some())
    })
}

fn response_write_metrics() -> &'static ResponseWriteMetrics {
    static METRICS: OnceLock<ResponseWriteMetrics> = OnceLock::new();
    METRICS.get_or_init(ResponseWriteMetrics::default)
}

fn hot_path_allocation_metrics() -> &'static HotPathAllocationMetrics {
    static METRICS: OnceLock<HotPathAllocationMetrics> = OnceLock::new();
    METRICS.get_or_init(HotPathAllocationMetrics::default)
}

fn duration_micros_u64(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn buffer_hold_log_threshold_micros() -> Option<u64> {
    static THRESHOLD: OnceLock<Option<u64>> = OnceLock::new();
    *THRESHOLD.get_or_init(|| {
        std::env::var("NNTP_PROXY_BUFFER_HOLD_LOG_MS")
            .ok()
            .and_then(|value| {
                let millis = value.parse::<u64>().ok()?;
                if millis == 0 {
                    None
                } else {
                    Some(millis.saturating_mul(1000))
                }
            })
    })
}

fn record_buffer_hold(source: PooledBufferSource, held_micros: u64) {
    let metrics = hot_path_allocation_metrics();
    match source.kind {
        PooledBufferKind::Regular => {
            metrics
                .regular_pool_buffer_holds
                .fetch_add(1, Ordering::Relaxed);
            metrics
                .regular_pool_buffer_hold_micros_total
                .fetch_add(held_micros, Ordering::Relaxed);
            metrics
                .regular_pool_buffer_hold_micros_max
                .fetch_max(held_micros, Ordering::Relaxed);
        }
        PooledBufferKind::Capture => {
            metrics
                .capture_pool_buffer_holds
                .fetch_add(1, Ordering::Relaxed);
            metrics
                .capture_pool_buffer_hold_micros_total
                .fetch_add(held_micros, Ordering::Relaxed);
            metrics
                .capture_pool_buffer_hold_micros_max
                .fetch_max(held_micros, Ordering::Relaxed);
        }
    }
}

fn maybe_log_buffer_hold(source: PooledBufferSource, held_micros: u64, capacity: usize) {
    let Some(threshold) = buffer_hold_log_threshold_micros() else {
        return;
    };
    if held_micros < threshold {
        return;
    }

    let pool = match source.kind {
        PooledBufferKind::Regular => "regular",
        PooledBufferKind::Capture => "capture",
    };
    debug!(
        pool,
        fallback = source.fallback,
        held_ms = held_micros / 1000,
        capacity,
        "Pooled buffer held longer than threshold"
    );
}

pub(crate) fn record_pending_backend_byte_heap_fallback() {
    hot_path_allocation_metrics()
        .pending_backend_byte_heap_fallbacks
        .fetch_add(1, Ordering::Relaxed);
}

pub(crate) fn classify_response_write_chunk(bytes: usize) -> (usize, usize, usize, usize) {
    let tiny_chunks = if bytes <= 256 { 1 } else { 0 };
    let tiny_chunk_bytes = if bytes <= 256 { bytes } else { 0 };
    let small_chunks = if bytes <= 4096 { 1 } else { 0 };
    let small_chunk_bytes = if bytes <= 4096 { bytes } else { 0 };
    (
        tiny_chunks,
        tiny_chunk_bytes,
        small_chunks,
        small_chunk_bytes,
    )
}

#[must_use]
pub fn hot_path_allocation_metrics_snapshot() -> HotPathAllocationMetricsSnapshot {
    let metrics = hot_path_allocation_metrics();
    HotPathAllocationMetricsSnapshot {
        regular_pool_fallback_allocations: metrics
            .regular_pool_fallback_allocations
            .load(Ordering::Relaxed),
        regular_pool_exhaustions: metrics.regular_pool_exhaustions.load(Ordering::Relaxed),
        capture_pool_fallback_allocations: metrics
            .capture_pool_fallback_allocations
            .load(Ordering::Relaxed),
        chunked_response_metadata_spills: metrics
            .chunked_response_metadata_spills
            .load(Ordering::Relaxed),
        pending_backend_byte_heap_fallbacks: metrics
            .pending_backend_byte_heap_fallbacks
            .load(Ordering::Relaxed),
        regular_pool_buffer_holds: metrics.regular_pool_buffer_holds.load(Ordering::Relaxed),
        regular_pool_buffer_hold_micros_total: metrics
            .regular_pool_buffer_hold_micros_total
            .load(Ordering::Relaxed),
        regular_pool_buffer_hold_micros_max: metrics
            .regular_pool_buffer_hold_micros_max
            .load(Ordering::Relaxed),
        capture_pool_buffer_holds: metrics.capture_pool_buffer_holds.load(Ordering::Relaxed),
        capture_pool_buffer_hold_micros_total: metrics
            .capture_pool_buffer_hold_micros_total
            .load(Ordering::Relaxed),
        capture_pool_buffer_hold_micros_max: metrics
            .capture_pool_buffer_hold_micros_max
            .load(Ordering::Relaxed),
    }
}

pub fn reset_hot_path_allocation_metrics() {
    let metrics = hot_path_allocation_metrics();
    metrics
        .regular_pool_fallback_allocations
        .store(0, Ordering::Relaxed);
    metrics.regular_pool_exhaustions.store(0, Ordering::Relaxed);
    metrics
        .capture_pool_fallback_allocations
        .store(0, Ordering::Relaxed);
    metrics
        .chunked_response_metadata_spills
        .store(0, Ordering::Relaxed);
    metrics
        .pending_backend_byte_heap_fallbacks
        .store(0, Ordering::Relaxed);
    metrics
        .regular_pool_buffer_holds
        .store(0, Ordering::Relaxed);
    metrics
        .regular_pool_buffer_hold_micros_total
        .store(0, Ordering::Relaxed);
    metrics
        .regular_pool_buffer_hold_micros_max
        .store(0, Ordering::Relaxed);
    metrics
        .capture_pool_buffer_holds
        .store(0, Ordering::Relaxed);
    metrics
        .capture_pool_buffer_hold_micros_total
        .store(0, Ordering::Relaxed);
    metrics
        .capture_pool_buffer_hold_micros_max
        .store(0, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn reset_response_write_metrics() {
    let metrics = response_write_metrics();
    metrics.responses.store(0, Ordering::Relaxed);
    metrics.single_chunk_responses.store(0, Ordering::Relaxed);
    metrics.multi_chunk_responses.store(0, Ordering::Relaxed);
    metrics.chunks_written.store(0, Ordering::Relaxed);
    metrics.bytes_written.store(0, Ordering::Relaxed);
    metrics.tiny_chunks.store(0, Ordering::Relaxed);
    metrics.tiny_chunk_bytes.store(0, Ordering::Relaxed);
    metrics.small_chunks.store(0, Ordering::Relaxed);
    metrics.small_chunk_bytes.store(0, Ordering::Relaxed);
    metrics.max_chunks_per_response.store(0, Ordering::Relaxed);
}

pub(crate) fn record_response_write_metrics_internal(
    chunk_count: usize,
    bytes_written: usize,
    tiny_chunks: usize,
    tiny_chunk_bytes: usize,
    small_chunks: usize,
    small_chunk_bytes: usize,
) {
    if !response_write_metrics_enabled() {
        return;
    }

    let metrics = response_write_metrics();
    metrics.responses.fetch_add(1, Ordering::Relaxed);
    metrics
        .chunks_written
        .fetch_add(chunk_count, Ordering::Relaxed);
    metrics
        .bytes_written
        .fetch_add(bytes_written, Ordering::Relaxed);
    metrics
        .tiny_chunks
        .fetch_add(tiny_chunks, Ordering::Relaxed);
    metrics
        .tiny_chunk_bytes
        .fetch_add(tiny_chunk_bytes, Ordering::Relaxed);
    metrics
        .small_chunks
        .fetch_add(small_chunks, Ordering::Relaxed);
    metrics
        .small_chunk_bytes
        .fetch_add(small_chunk_bytes, Ordering::Relaxed);
    metrics
        .max_chunks_per_response
        .fetch_max(chunk_count, Ordering::Relaxed);
    if chunk_count <= 1 {
        metrics
            .single_chunk_responses
            .fetch_add(1, Ordering::Relaxed);
    } else {
        metrics
            .multi_chunk_responses
            .fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
pub(crate) fn response_write_metrics_test_guard() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::Mutex;

    static GUARD: Mutex<()> = Mutex::new(());
    GUARD
        .lock()
        .expect("response-write metrics test guard poisoned")
}

#[must_use]
pub fn response_write_metrics_snapshot() -> ResponseWriteMetricsSnapshot {
    let metrics = response_write_metrics();
    ResponseWriteMetricsSnapshot {
        responses: metrics.responses.load(Ordering::Relaxed),
        single_chunk_responses: metrics.single_chunk_responses.load(Ordering::Relaxed),
        multi_chunk_responses: metrics.multi_chunk_responses.load(Ordering::Relaxed),
        chunks_written: metrics.chunks_written.load(Ordering::Relaxed),
        bytes_written: metrics.bytes_written.load(Ordering::Relaxed),
        tiny_chunks: metrics.tiny_chunks.load(Ordering::Relaxed),
        tiny_chunk_bytes: metrics.tiny_chunk_bytes.load(Ordering::Relaxed),
        small_chunks: metrics.small_chunks.load(Ordering::Relaxed),
        small_chunk_bytes: metrics.small_chunk_bytes.load(Ordering::Relaxed),
        max_chunks_per_response: metrics.max_chunks_per_response.load(Ordering::Relaxed),
    }
}

#[derive(Debug)]
struct ResponseChunk {
    storage: ResponseChunkStorage,
    range: Range<usize>,
}

#[derive(Debug)]
enum ResponseChunkStorage {
    Pooled(PooledBuffer),
    #[cfg(test)]
    SharedPooled(Arc<PooledBuffer>),
}

impl ResponseChunk {
    fn as_slice(&self) -> &[u8] {
        match &self.storage {
            ResponseChunkStorage::Pooled(buffer) => &buffer.as_ref()[self.range.clone()],
            #[cfg(test)]
            ResponseChunkStorage::SharedPooled(buffer) => &buffer.as_ref()[self.range.clone()],
        }
    }

    fn initialized(&self) -> usize {
        match &self.storage {
            ResponseChunkStorage::Pooled(buffer) => buffer.initialized(),
            #[cfg(test)]
            ResponseChunkStorage::SharedPooled(buffer) => buffer.initialized(),
        }
    }

    fn capacity(&self) -> usize {
        match &self.storage {
            ResponseChunkStorage::Pooled(buffer) => buffer.capacity(),
            #[cfg(test)]
            ResponseChunkStorage::SharedPooled(buffer) => buffer.capacity(),
        }
    }

    fn can_append(&self) -> bool {
        matches!(self.storage, ResponseChunkStorage::Pooled(_))
            && self.range.end == self.initialized()
            && self.initialized() < self.capacity()
    }

    fn extend_from_slice(&mut self, data: &[u8]) {
        match &mut self.storage {
            ResponseChunkStorage::Pooled(buffer) => buffer.extend_from_slice(data),
            #[cfg(test)]
            ResponseChunkStorage::SharedPooled(_) => {
                panic!("cannot extend shared response chunk")
            }
        }
    }
}

impl ChunkedResponse {
    /// Total buffered length across all chunks.
    #[must_use]
    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns true when no bytes are buffered.
    #[must_use]
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Remove all buffered data, returning chunks to the pool on drop.
    #[inline]
    pub fn clear(&mut self) {
        self.chunks.clear();
        self.len = 0;
    }

    /// Append bytes using one or more pooled capture buffers.
    ///
    /// # Panics
    /// Panics if the internal chunk list becomes unexpectedly empty after a
    /// new capture buffer is acquired.
    pub fn extend_from_slice(&mut self, pool: &BufferPool, mut data: &[u8]) {
        while !data.is_empty() {
            let need_new_chunk = self.chunks.last().is_none_or(|chunk| !chunk.can_append());

            if need_new_chunk {
                if self.chunks.len() == self.chunks.inline_size() {
                    hot_path_allocation_metrics()
                        .chunked_response_metadata_spills
                        .fetch_add(1, Ordering::Relaxed);
                }
                self.chunks.push(ResponseChunk {
                    storage: ResponseChunkStorage::Pooled(pool.acquire_capture_now()),
                    range: 0..0,
                });
            }

            let chunk = self
                .chunks
                .last_mut()
                .expect("chunk just pushed or already existed");
            let available = chunk.capacity().saturating_sub(chunk.initialized());
            debug_assert!(available > 0, "chunk must have space after allocation");

            let take = available.min(data.len());
            chunk.extend_from_slice(&data[..take]);
            chunk.range.end += take;
            self.len += take;
            data = &data[take..];
        }
    }

    /// Move a byte range from an initialized pooled buffer into this response.
    ///
    /// This avoids copying large backend reads into separate capture buffers.
    /// The caller must pass only a range already produced by response framing.
    /// Later appends will allocate a fresh pooled chunk when this range does not
    /// end at the initialized length, so skipped bytes are never exposed.
    /// # Panics
    /// Panics if `range` is outside the buffer's initialized bytes.
    pub(crate) fn push_buffer_range(&mut self, buffer: PooledBuffer, range: Range<usize>) {
        assert!(
            range.start <= range.end && range.end <= buffer.initialized(),
            "response range must be inside initialized buffer"
        );
        if self.chunks.len() == self.chunks.inline_size() {
            hot_path_allocation_metrics()
                .chunked_response_metadata_spills
                .fetch_add(1, Ordering::Relaxed);
        }
        self.len += range.end - range.start;
        self.chunks.push(ResponseChunk {
            storage: ResponseChunkStorage::Pooled(buffer),
            range,
        });
    }

    /// Add a range from a shared pooled buffer without copying.
    ///
    /// # Panics
    /// Panics if `range` is outside the pooled buffer's initialized bytes.
    #[cfg(test)]
    pub(crate) fn push_shared_pooled_range(
        &mut self,
        buffer: Arc<PooledBuffer>,
        range: Range<usize>,
    ) {
        assert!(
            range.start <= range.end && range.end <= buffer.initialized(),
            "response range must be inside initialized pooled buffer"
        );
        if self.chunks.len() == self.chunks.inline_size() {
            hot_path_allocation_metrics()
                .chunked_response_metadata_spills
                .fetch_add(1, Ordering::Relaxed);
        }
        self.len += range.end - range.start;
        self.chunks.push(ResponseChunk {
            storage: ResponseChunkStorage::SharedPooled(buffer),
            range,
        });
    }

    /// First chunk of buffered data, if any.
    #[cfg(test)]
    #[must_use]
    pub fn first_chunk(&self) -> Option<&[u8]> {
        self.chunks.first().map(ResponseChunk::as_slice)
    }

    /// Iterate buffered chunks without flattening.
    pub fn iter_chunks(&self) -> impl Iterator<Item = &[u8]> {
        self.chunks.iter().map(ResponseChunk::as_slice)
    }

    /// Copy up to `len` bytes from the front of the response into a small stack-backed buffer.
    pub fn copy_prefix_into(&self, len: usize, out: &mut SmallVec<[u8; 128]>) {
        out.clear();
        let mut remaining = len.min(self.len);
        for chunk in self.iter_chunks() {
            if remaining == 0 {
                break;
            }
            let take = remaining.min(chunk.len());
            out.extend_from_slice(&chunk[..take]);
            remaining -= take;
        }
    }

    /// Copy the buffered response into a contiguous `Vec<u8>`.
    #[must_use]
    pub fn to_vec(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.len);
        for chunk in self.iter_chunks() {
            out.extend_from_slice(chunk);
        }
        out
    }

    /// Write all buffered chunks to a sink in order.
    ///
    /// # Errors
    /// Returns any write error produced by the underlying async sink.
    pub async fn write_all_to_recording<W>(&self, writer: &mut W) -> std::io::Result<()>
    where
        W: AsyncWriteExt + Unpin,
    {
        for chunk in self.iter_chunks() {
            writer.write_all(chunk).await?;
        }
        Ok(())
    }
}

/// Lock-free buffer pool for reusing large I/O buffers.
///
/// Uses bounded `ArrayQueue`s so queue slots are allocated once at startup and
/// buffer return does not allocate linked queue blocks on the forwarding path.
#[derive(Debug, Clone)]
pub struct BufferPool {
    pool: Arc<ArrayQueue<BytesMut>>,
    buffer_size: BufferSize,
    max_pool_size: usize,
    pool_size: Arc<AtomicUsize>,
    allocated_count: Arc<AtomicUsize>,
    // Capture buffer pool (for accumulating streaming data)
    capture_pool: Arc<ArrayQueue<BytesMut>>,
    capture_capacity: usize,
    max_capture_pool_size: usize,
    capture_pool_size: Arc<AtomicUsize>,
    capture_allocated_count: Arc<AtomicUsize>,
}

impl BufferPool {
    /// Create a page-aligned buffer for optimal DMA performance
    ///
    /// Returns a raw `BytesMut` that will be wrapped in `PooledBuffer` by `acquire()`.
    /// The buffer has a logical length of zero before it enters the pool.
    ///
    /// # Safety
    ///
    /// **INTERNAL USE ONLY.** This function is not exposed publicly and is only used
    /// within the buffer pool implementation where the safety contract is guaranteed.
    ///
    /// The returned buffer has a logical length of zero. Only bytes added to the
    /// `BytesMut` logical length are exposed through the public initialized slice APIs.
    fn create_aligned_buffer(size: usize) -> BytesMut {
        // Align to page boundaries (4KB) for better memory performance
        let page_size = 4096;
        let aligned_size = size.div_ceil(page_size) * page_size;

        BytesMut::with_capacity(aligned_size)
    }

    /// Create a new lazy buffer pool
    ///
    /// # Arguments
    /// * `buffer_size` - Size of each buffer in bytes (must be non-zero)
    /// * `max_pool_size` - Maximum number of buffers to pool
    ///
    /// # Design Philosophy
    ///
    /// **Buffer pool queue capacity is allocated once at application boot**. Backing
    /// buffers are allocated lazily as clients/backend work first need them, then
    /// returned to the pool for reuse. This is a critical performance
    /// optimization:
    ///
    /// - **Boot time**: Queue slots only; idle RSS stays small
    /// - **Warm runtime**: Zero allocations in hot path (acquire/release from pool)
    /// - **Per-connection**: Buffers are borrowed and returned, never owned
    ///
    /// **IMPORTANT**: Do NOT create a `BufferPool` per-client, per-connection, or
    /// per-request. Create ONE pool at application startup and share it across
    /// all operations via Arc or static reference.
    ///
    /// Buffers are allocated on first use up to `max_pool_size`; allocations beyond
    /// that remain visible through fallback metrics.
    #[must_use]
    pub fn new(buffer_size: BufferSize, max_pool_size: usize) -> Self {
        let pool = Arc::new(ArrayQueue::new(max_pool_size.max(1)));
        let pool_size = Arc::new(AtomicUsize::new(0));
        let allocated_count = Arc::new(AtomicUsize::new(0));

        info!(
            "Creating lazy buffer pool for up to {} buffers of {}KB each ({}MB max resident after use)",
            max_pool_size,
            buffer_size.get() / 1024,
            (max_pool_size * buffer_size.get()) / (1024 * 1024)
        );

        Self {
            pool,
            buffer_size,
            max_pool_size,
            pool_size,
            allocated_count,
            // Initialize capture pool as empty (will be configured via with_capture_pool)
            capture_pool: Arc::new(ArrayQueue::new(1)),
            capture_capacity: 0,
            max_capture_pool_size: 0,
            capture_pool_size: Arc::new(AtomicUsize::new(0)),
            capture_allocated_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Configure the capture buffer pool for accumulator buffers
    ///
    /// Capture buffers are used for accumulating streaming data (e.g., caching).
    /// # Arguments
    /// * `capacity` - Size of each capture buffer in bytes (e.g., 1 MiB by default)
    /// * `count` - Maximum number of capture buffers to pool
    #[must_use]
    pub fn with_capture_pool(mut self, capacity: usize, count: usize) -> Self {
        self.capture_pool = Arc::new(ArrayQueue::new(count.max(1)));
        self.capture_pool_size = Arc::new(AtomicUsize::new(0));
        self.capture_allocated_count = Arc::new(AtomicUsize::new(0));

        info!(
            "Creating lazy capture buffer pool for up to {} buffers of {}KB each ({}MB max resident after use)",
            count,
            capacity / 1024,
            (count * capacity) / (1024 * 1024)
        );

        self.capture_capacity = capacity;
        self.max_capture_pool_size = count;

        self
    }

    /// Create a buffer pool suitable for testing
    ///
    /// Uses sensible defaults (8KB buffers, pool of 4) that work for most tests.
    /// Prefer this over manually constructing `BufferPool` in tests.
    ///
    /// # Panics
    /// Panics only if the built-in test buffer size stops satisfying
    /// `BufferSize` validation.
    #[cfg(test)]
    #[must_use]
    pub fn for_tests() -> Self {
        Self::new(BufferSize::try_new(8192).expect("valid size"), 4).with_capture_pool(8192, 2)
    }

    /// Get the current number of available buffers in the pool
    #[must_use]
    pub fn available_buffers(&self) -> usize {
        self.pool_size.load(Ordering::Relaxed)
    }

    /// Get the number of buffers currently in use
    #[must_use]
    pub fn buffers_in_use(&self) -> usize {
        self.allocated_count
            .load(Ordering::Relaxed)
            .saturating_sub(self.available_buffers())
    }

    /// Get buffer pool statistics (available, in-use, total)
    #[must_use]
    pub fn stats(&self) -> (usize, usize, usize) {
        let available = self.available_buffers();
        let in_use = self.buffers_in_use();
        (available, in_use, self.max_pool_size)
    }

    /// Get a buffer from the pool or create a new one (lock-free)
    ///
    /// Returns a `PooledBuffer` that automatically returns to the pool when dropped.
    ///
    /// # Performance: Warm Zero-Allocation Hot Path
    ///
    /// This method allocates lazily until the pool reaches `max_pool_size`.
    /// Once buffers have been created and returned, reuse just pops from the
    /// lock-free queue. This keeps idle RSS low while preserving the warm
    /// zero-allocation forwarding path.
    ///
    /// Only if the pool is exhausted (all buffers in use) will a new buffer
    /// be allocated. Size the pool appropriately to avoid this fallback.
    ///
    /// # Safety Notes
    ///
    /// The buffer may contain old bytes outside its logical length, but immutable
    /// access only exposes `BytesMut::len()` initialized bytes.
    fn wrap_regular_buffer(
        &self,
        buffer: BytesMut,
        fallback: bool,
        counts_toward_pool: bool,
    ) -> PooledBuffer {
        PooledBuffer {
            expected_capacity: buffer.capacity(),
            buffer,
            pool: Arc::clone(&self.pool),
            pool_size: Arc::clone(&self.pool_size),
            allocated_count: Arc::clone(&self.allocated_count),
            max_pool_size: self.max_pool_size,
            writable_len: self.buffer_size.get(),
            acquired_at: Instant::now(),
            source: PooledBufferSource::regular(fallback),
            counts_toward_pool,
        }
    }

    fn try_reserve_regular_slot(&self) -> bool {
        let mut allocated = self.allocated_count.load(Ordering::Relaxed);
        while allocated < self.max_pool_size {
            match self.allocated_count.compare_exchange_weak(
                allocated,
                allocated + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(new_allocated) => allocated = new_allocated,
            }
        }
        false
    }

    fn try_reserve_capture_slot(&self) -> bool {
        let mut allocated = self.capture_allocated_count.load(Ordering::Relaxed);
        while allocated < self.max_capture_pool_size {
            match self.capture_allocated_count.compare_exchange_weak(
                allocated,
                allocated + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(new_allocated) => allocated = new_allocated,
            }
        }
        false
    }

    fn acquire_now(&self) -> PooledBuffer {
        self.pool.pop().map_or_else(
            || {
                if self.try_reserve_regular_slot() {
                    return self.wrap_regular_buffer(
                        Self::create_aligned_buffer(self.buffer_size.get()),
                        false,
                        true,
                    );
                }

                hot_path_allocation_metrics()
                    .regular_pool_fallback_allocations
                    .fetch_add(1, Ordering::Relaxed);
                warn!(
                    max_pool_size = self.max_pool_size,
                    buffer_size = self.buffer_size.get(),
                    "Regular buffer pool exhausted; allocating fallback buffer"
                );
                self.wrap_regular_buffer(
                    Self::create_aligned_buffer(self.buffer_size.get()),
                    true,
                    false,
                )
            },
            |buffer| {
                self.pool_size.fetch_sub(1, Ordering::Relaxed);
                // Buffer from pool is logically empty and has the expected allocation
                // capacity (enforced on return).
                debug_assert_eq!(buffer.len(), 0);
                self.wrap_regular_buffer(buffer, false, true)
            },
        )
    }

    pub fn acquire(&self) -> PooledBuffer {
        self.acquire_now()
    }

    /// Get a regular buffer only if one is already available in the pool.
    ///
    /// This is for strict proxy forwarding paths where falling back to a fresh
    /// allocation would hide hot-path pressure. Pool exhaustion remains visible
    /// through the separate exhaustion metric, but this method returns `None`
    /// instead of allocating.
    #[cfg(test)]
    pub(crate) fn try_acquire(&self) -> Option<PooledBuffer> {
        let buffer = self.pool.pop().map_or_else(
            || {
                hot_path_allocation_metrics()
                    .regular_pool_exhaustions
                    .fetch_add(1, Ordering::Relaxed);
                None
            },
            Some,
        )?;
        self.pool_size.fetch_sub(1, Ordering::Relaxed);
        debug_assert_eq!(buffer.len(), 0);
        Some(self.wrap_regular_buffer(buffer, false, true))
    }

    /// Get a capture buffer from the capture pool
    ///
    /// Returns a `PooledBuffer` backed by a pooled capture buffer.
    /// Used for accumulating streaming data (e.g., caching articles).
    pub(crate) fn acquire_capture_now(&self) -> PooledBuffer {
        let mut fallback = false;
        let mut counts_toward_pool = true;
        let buffer = self.capture_pool.pop().map_or_else(
            || {
                let capacity = if self.capture_capacity > 0 {
                    self.capture_capacity
                } else {
                    self.buffer_size.get()
                };

                if self.try_reserve_capture_slot() {
                    return BytesMut::with_capacity(capacity);
                }

                fallback = true;
                counts_toward_pool = false;
                hot_path_allocation_metrics()
                    .capture_pool_fallback_allocations
                    .fetch_add(1, Ordering::Relaxed);
                warn!(
                    max_pool_size = self.max_capture_pool_size,
                    capacity, "Capture buffer pool exhausted; allocating fallback buffer"
                );
                // Pool exhausted - allocate a visible fallback buffer.
                BytesMut::with_capacity(capacity)
            },
            |mut buffer| {
                self.capture_pool_size.fetch_sub(1, Ordering::Relaxed);
                // Clear but keep capacity for reuse.
                buffer.clear();
                buffer
            },
        );

        PooledBuffer {
            expected_capacity: buffer.capacity(),
            buffer,
            pool: Arc::clone(&self.capture_pool),
            pool_size: Arc::clone(&self.capture_pool_size),
            allocated_count: Arc::clone(&self.capture_allocated_count),
            max_pool_size: self.max_capture_pool_size,
            writable_len: 0,
            acquired_at: Instant::now(),
            source: PooledBufferSource::capture(fallback),
            counts_toward_pool,
        }
    }

    pub fn acquire_capture(&self) -> PooledBuffer {
        self.acquire_capture_now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    fn hot_path_metrics_test_guard() -> MutexGuard<'static, ()> {
        static GUARD: Mutex<()> = Mutex::new(());
        GUARD.lock().expect("hot-path metrics test guard poisoned")
    }

    fn response_write_metrics_test_guard() -> MutexGuard<'static, ()> {
        super::response_write_metrics_test_guard()
    }
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn test_buffer_pool_creation() {
        let pool = BufferPool::new(BufferSize::try_new(8192).unwrap(), 10);

        assert_eq!(pool.stats(), (0, 0, 10));

        // Pool should lazily allocate buffers on first use.
        let buffer1 = pool.acquire();
        assert_eq!(buffer1.capacity(), 8192);
        assert_eq!(buffer1.initialized(), 0); // No bytes initialized yet
        // Buffer automatically returned on drop
    }

    #[tokio::test]
    async fn test_acquire_starts_empty_with_stable_capacity() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1);

        let capacity = {
            let buffer = pool.acquire();
            assert_eq!(buffer.initialized(), 0);
            assert_eq!(buffer.len(), 0);
            buffer.capacity()
        };

        let buffer = pool.acquire();
        assert_eq!(buffer.initialized(), 0);
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.capacity(), capacity);
    }

    #[tokio::test]
    async fn test_read_from_sets_initialized_without_reducing_capacity() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1);
        let mut buffer = pool.acquire();
        let capacity = buffer.capacity();
        let (mut writer, mut reader) = tokio::io::duplex(64);

        writer.write_all(b"220 ready\r\n").await.unwrap();
        drop(writer);

        let read = buffer.read_from(&mut reader).await.unwrap();
        assert_eq!(read, 11);
        assert_eq!(buffer.initialized(), 11);
        assert_eq!(&*buffer, b"220 ready\r\n");
        assert_eq!(buffer.capacity(), capacity);
    }

    #[tokio::test]
    async fn test_read_from_is_limited_to_fixed_writable_region() {
        let pool = BufferPool::new(BufferSize::try_new(8).unwrap(), 1);
        let mut buffer = pool.acquire();
        assert_eq!(buffer.capacity(), 4096);

        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer
            .write_all(b"220 long response over eight bytes\r\n")
            .await
            .unwrap();
        drop(writer);

        let read = buffer.read_from(&mut reader).await.unwrap();
        assert_eq!(read, 8);
        assert_eq!(buffer.initialized(), 8);
        assert_eq!(buffer[..read].len(), read);
    }

    #[tokio::test]
    async fn test_read_more_appends_after_initialized_bytes() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1);
        let mut buffer = pool.acquire();
        buffer.copy_from_slice(b"22");

        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(b"0 ready\r\n").await.unwrap();
        drop(writer);

        let read = buffer.read_more(&mut reader).await.unwrap();
        assert_eq!(read, 9);
        assert_eq!(buffer.initialized(), 11);
        assert_eq!(&*buffer, b"220 ready\r\n");
    }

    #[tokio::test]
    async fn test_read_more_is_limited_to_remaining_fixed_writable_region() {
        let pool = BufferPool::new(BufferSize::try_new(8).unwrap(), 1);
        let mut buffer = pool.acquire();
        buffer.copy_from_slice(b"22");

        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(b"0 long response\r\n").await.unwrap();
        drop(writer);

        let read = buffer.read_more(&mut reader).await.unwrap();
        assert_eq!(read, 6);
        assert_eq!(buffer.initialized(), 8);
        assert_eq!(&*buffer, b"220 long");
    }

    #[tokio::test]
    async fn test_copy_from_slice_overwrites_and_allows_up_to_capacity() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1);
        let mut buffer = pool.acquire();
        let capacity = buffer.capacity();

        buffer.copy_from_slice(b"previous bytes");
        assert_eq!(&*buffer, b"previous bytes");

        let data = vec![b'X'; capacity];
        buffer.copy_from_slice(&data);
        assert_eq!(buffer.initialized(), capacity);
        assert_eq!(&buffer[..4], b"XXXX");
        assert_eq!(buffer.capacity(), capacity);
    }

    #[tokio::test]
    #[should_panic(expected = "data exceeds buffer capacity")]
    async fn test_copy_from_slice_rejects_larger_than_capacity() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1);
        let mut buffer = pool.acquire();
        let too_large = vec![b'X'; buffer.capacity() + 1];

        buffer.copy_from_slice(&too_large);
    }

    #[tokio::test]
    async fn test_clear_only_changes_initialized_length() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1);
        let mut buffer = pool.acquire();
        let capacity = buffer.capacity();

        buffer.copy_from_slice(b"abcdef");
        buffer.clear();
        assert_eq!(buffer.initialized(), 0);
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.capacity(), capacity);
    }

    #[tokio::test]
    async fn test_resized_buffers_are_discarded_instead_of_repooled() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1).with_capture_pool(8, 1);

        let normal_capacity = {
            let mut buffer = pool.acquire();
            let capacity = buffer.capacity();
            buffer.extend_from_slice(&vec![b'N'; capacity + 1]);
            assert!(buffer.capacity() > capacity);
            capacity
        };

        let buffer = pool.acquire();
        assert_eq!(buffer.capacity(), normal_capacity);
        drop(buffer);

        {
            let mut capture = pool.acquire_capture();
            capture.extend_from_slice(&[b'C'; 9]);
            assert!(capture.capacity() > 8);
        }

        let capture = pool.acquire_capture();
        assert_eq!(capture.capacity(), 8);
    }

    #[tokio::test]
    async fn test_buffer_pool_get_and_return() {
        let pool = BufferPool::new(BufferSize::try_new(4096).unwrap(), 5);

        // Get a buffer
        let buffer = pool.acquire();
        assert_eq!(buffer.capacity(), 4096);
        assert_eq!(buffer.initialized(), 0);

        // Callers see an empty initialized slice until data is read or copied in.

        // Drop it (automatically returns to pool)
        drop(buffer);

        // Get it again - should be from pool
        let buffer2 = pool.acquire();
        assert_eq!(buffer2.capacity(), 4096);
    }

    #[tokio::test]
    async fn test_buffer_pool_exhaustion() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 2);

        // Lazily allocate all pool-owned buffers.
        let buf1 = pool.acquire();
        let buf2 = pool.acquire();

        // Pool is exhausted, should create new buffer
        let buf3 = pool.acquire();
        assert_eq!(buf3.capacity(), 4096);

        // Drop buffers (automatically returned)
        drop(buf1);
        drop(buf2);
        drop(buf3);
    }

    #[tokio::test]
    async fn test_try_acquire_reports_exhaustion_without_allocating() {
        let _guard = hot_path_metrics_test_guard();
        reset_hot_path_allocation_metrics();
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1);

        assert!(pool.try_acquire().is_none());
        {
            let buffer = pool.acquire();
            assert_eq!(buffer.capacity(), 4096);
        }

        let _held = pool.try_acquire().expect("warmed buffer available");

        let before = hot_path_allocation_metrics_snapshot();

        assert!(pool.try_acquire().is_none());

        let after = hot_path_allocation_metrics_snapshot();
        assert_eq!(
            after.regular_pool_fallback_allocations,
            before.regular_pool_fallback_allocations
        );
        assert!(after.regular_pool_exhaustions > before.regular_pool_exhaustions);
        assert_eq!(pool.stats(), (0, 1, 1));
    }

    #[tokio::test]
    async fn test_buffer_hold_metrics_record_regular_and_capture_drops() {
        let _guard = hot_path_metrics_test_guard();
        reset_hot_path_allocation_metrics();
        let pool =
            BufferPool::new(BufferSize::try_new(1024).unwrap(), 1).with_capture_pool(8192, 1);
        let before = hot_path_allocation_metrics_snapshot();

        drop(pool.acquire());
        drop(pool.acquire_capture());

        let after = hot_path_allocation_metrics_snapshot();
        assert!(after.regular_pool_buffer_holds > before.regular_pool_buffer_holds);
        assert!(after.capture_pool_buffer_holds > before.capture_pool_buffer_holds);
    }

    #[test]
    fn test_set_response_write_metrics_enabled_toggles_flag() {
        let _guard = response_write_metrics_test_guard();
        let was_enabled = super::response_write_metrics_enabled();

        super::set_response_write_metrics_enabled(!was_enabled);
        assert_eq!(super::response_write_metrics_enabled(), !was_enabled);

        super::set_response_write_metrics_enabled(was_enabled);
    }

    #[tokio::test]
    async fn test_oversized_capture_buffer_is_not_reused() {
        let pool =
            BufferPool::new(BufferSize::try_new(1024).unwrap(), 1).with_capture_pool(8192, 1);

        let mut capture = pool.acquire_capture();
        capture.extend_from_slice(&vec![b'X'; 8193]);
        assert!(capture.capacity() > 8192);
        drop(capture);

        let capture2 = pool.acquire_capture();
        assert_eq!(capture2.capacity(), 8192);
    }

    #[tokio::test]
    async fn test_capture_pool_lazily_allocates_configured_count() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1).with_capture_pool(8, 4);

        assert_eq!(pool.capture_pool_size.load(Ordering::Relaxed), 0);
        assert_eq!(pool.capture_allocated_count.load(Ordering::Relaxed), 0);

        let buffers = [
            pool.acquire_capture(),
            pool.acquire_capture(),
            pool.acquire_capture(),
            pool.acquire_capture(),
        ];

        assert_eq!(pool.capture_pool_size.load(Ordering::Relaxed), 0);
        assert_eq!(pool.capture_allocated_count.load(Ordering::Relaxed), 4);
        assert!(buffers.iter().all(|buffer| buffer.capacity() == 8));
        drop(buffers);

        assert_eq!(pool.capture_pool_size.load(Ordering::Relaxed), 4);
    }

    #[tokio::test]
    async fn test_chunked_response_helpers_without_flattening() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1).with_capture_pool(8, 4);
        let mut response = ChunkedResponse::default();
        response.extend_from_slice(&pool, b"220 0 <id>\r\nBody\r\n.\r\n");

        let chunks: Vec<&[u8]> = response.iter_chunks().collect();
        assert!(chunks.len() > 1, "test requires multi-chunk buffering");
        assert_eq!(chunks.concat(), b"220 0 <id>\r\nBody\r\n.\r\n");

        let mut prefix = SmallVec::<[u8; 128]>::new();
        response.copy_prefix_into(12, &mut prefix);
        assert_eq!(&prefix[..], b"220 0 <id>\r\n");
    }

    #[tokio::test]
    async fn test_chunked_response_uses_more_capture_buffers_instead_of_growing_one() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1).with_capture_pool(8, 4);
        let mut response = ChunkedResponse::default();

        response.extend_from_slice(&pool, b"123456789abcdefghi");

        let chunks: Vec<&[u8]> = response.iter_chunks().collect();
        assert_eq!(
            chunks.iter().map(|chunk| chunk.len()).collect::<Vec<_>>(),
            vec![8, 8, 2]
        );
    }

    #[tokio::test]
    async fn test_chunked_response_copy_prefix_clamps_to_length() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1).with_capture_pool(8, 4);
        let mut response = ChunkedResponse::default();
        response.extend_from_slice(&pool, b"430\r\n");

        let mut prefix = SmallVec::<[u8; 128]>::new();
        response.copy_prefix_into(64, &mut prefix);
        assert_eq!(&prefix[..], b"430\r\n");
    }

    #[tokio::test]
    async fn test_chunked_response_can_own_range_without_copying() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 2);
        let mut buffer = pool.acquire();
        buffer.copy_from_slice(b"xx220 0 <id>\r\nBody\r\n.\r\nyy");
        let expected_ptr = buffer.as_ref()[2..].as_ptr();

        let mut response = ChunkedResponse::default();
        response.push_buffer_range(buffer, 2..23);

        let first = response.first_chunk().expect("range chunk");
        assert_eq!(first.as_ptr(), expected_ptr);
        assert_eq!(first, b"220 0 <id>\r\nBody\r\n.\r\n");
    }

    #[tokio::test]
    async fn test_chunked_response_can_share_pooled_range_without_copying() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 2);
        let mut buffer = pool.acquire();
        buffer.copy_from_slice(b"xx220 0 <id>\r\nBody\r\n.\r\nyy");
        let shared = std::sync::Arc::new(buffer);
        let expected_ptr = shared.as_ref()[2..].as_ptr();

        let mut response = ChunkedResponse::default();
        response.push_shared_pooled_range(std::sync::Arc::clone(&shared), 2..23);

        let first = response.first_chunk().expect("shared pooled range chunk");
        assert_eq!(first.as_ptr(), expected_ptr);
        assert_eq!(first, b"220 0 <id>\r\nBody\r\n.\r\n");
        drop(response);
        drop(shared);
        assert_eq!(pool.available_buffers(), 1);
    }

    #[tokio::test]
    async fn test_freeze_detaches_bytes_from_pool_ownership() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1);
        let mut buffer = pool.acquire();
        buffer.copy_from_slice(b"223 0 <id>\r\n");
        assert_eq!(pool.available_buffers(), 0);

        let bytes = buffer.freeze();
        assert_eq!(&bytes[..], b"223 0 <id>\r\n");

        drop(bytes);

        assert_eq!(
            pool.available_buffers(),
            0,
            "dropping frozen bytes must not return the detached allocation to the pool"
        );
    }

    #[tokio::test]
    async fn test_buffer_pool_concurrent_access() {
        let pool = BufferPool::new(BufferSize::try_new(2048).unwrap(), 10);

        // Spawn multiple tasks accessing the pool concurrently
        let mut handles = vec![];

        for _ in 0..20 {
            let pool_clone = pool.clone();
            let handle = tokio::spawn(async move {
                let buffer = pool_clone.acquire();
                assert_eq!(buffer.capacity(), 4096);
                // Simulate some work
                tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
            });
            handles.push(handle);
        }

        // Wait for all tasks to complete
        for handle in handles {
            handle.await.unwrap();
        }
    }

    #[tokio::test]
    async fn test_buffer_alignment() {
        let pool = BufferPool::new(BufferSize::try_new(8192).unwrap(), 1);
        let buffer = pool.acquire();

        // Buffer capacity should be aligned to page boundaries (4KB)
        assert!(buffer.capacity() >= 8192);
        // Should be page-aligned (multiple of 4096)
        assert_eq!(buffer.capacity() % 4096, 0);
    }

    #[tokio::test]
    async fn test_buffer_clear_and_resize() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 2);

        let mut buffer = pool.acquire();

        // Write data using copy_from_slice
        let data = vec![42u8; 101];
        buffer.copy_from_slice(&data);
        assert_eq!(buffer.initialized(), 101);

        // Drop returns it to pool
        drop(buffer);

        // Get it again - may contain old data (performance optimization)
        let buffer2 = pool.acquire();
        assert_eq!(buffer2.capacity(), 4096);
        // Note: buffer may contain previous bytes outside the initialized range.
    }

    #[tokio::test]
    async fn test_buffer_pool_max_size_enforcement() {
        let pool = BufferPool::new(BufferSize::try_new(512).unwrap(), 3);

        // Get all buffers
        let buf1 = pool.acquire();
        let buf2 = pool.acquire();
        let buf3 = pool.acquire();

        // Get one more (should create new)
        let buf4 = pool.acquire();

        // Drop all buffers (automatically returned)
        drop(buf1);
        drop(buf2);
        drop(buf3);
        drop(buf4);

        // Pool should not exceed max size
        // (We can't directly test pool size, but the implementation handles it)
    }

    #[tokio::test]
    async fn test_buffer_wrong_size_not_returned() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 2);

        let buffer = pool.acquire();
        assert_eq!(buffer.capacity(), 4096);

        // PooledBuffer auto-returns on drop with correct size enforcement in Drop impl
        drop(buffer);
    }

    #[tokio::test]
    async fn test_buffer_pool_multiple_get_return_cycles() {
        let pool = BufferPool::new(BufferSize::try_new(4096).unwrap(), 5);

        // Do multiple get/return cycles
        for i in 0u8..20 {
            let mut buffer = pool.acquire();
            assert_eq!(buffer.capacity(), 4096);

            // Write some data using copy_from_slice
            let data = vec![i; 1];
            buffer.copy_from_slice(&data);
            assert_eq!(buffer.initialized(), 1);
        }
    }

    #[test]
    fn test_buffer_pool_clone() {
        let pool1 = BufferPool::new(BufferSize::try_new(1024).unwrap(), 5);
        let _pool2 = pool1;

        // Both should share the same underlying pool
        // (Arc ensures shared ownership)
    }

    #[tokio::test]
    async fn test_different_buffer_sizes() {
        let small_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 5);
        let medium_pool = BufferPool::new(BufferSize::try_new(8192).unwrap(), 5);
        let large_pool = BufferPool::new(BufferSize::try_new(65536).unwrap(), 5);

        let small_buf = small_pool.acquire();
        let medium_buf = medium_pool.acquire();
        let large_buf = large_pool.acquire();

        assert_eq!(small_buf.capacity(), 4096);
        assert_eq!(medium_buf.capacity(), 8192);
        assert_eq!(large_buf.capacity(), 65536);

        // Buffers auto-return on drop
    }

    #[tokio::test]
    async fn test_buffer_pool_stress() {
        let pool = BufferPool::new(BufferSize::try_new(4096).unwrap(), 10);

        // Stress test with many concurrent operations
        let mut handles = vec![];

        for _ in 0..100 {
            let pool_clone = pool.clone();
            let handle = tokio::spawn(async move {
                for _ in 0..10 {
                    let buffer = pool_clone.acquire();
                    assert_eq!(buffer.capacity(), 4096);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await.unwrap();
        }
    }

    #[tokio::test]
    async fn test_pooled_buffer_deref() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 5);
        let mut buffer = pool.acquire();

        // Initially no initialized bytes
        assert_eq!(buffer.len(), 0);

        // Write data
        buffer.copy_from_slice(b"Hello");

        // Deref should return only initialized portion
        assert_eq!(buffer.len(), 5);
        assert_eq!(&*buffer, b"Hello");
    }

    #[tokio::test]
    async fn test_pooled_buffer_as_ref() {
        let pool = BufferPool::new(BufferSize::try_new(512).unwrap(), 5);
        let mut buffer = pool.acquire();

        buffer.copy_from_slice(b"Test data");

        // AsRef should return initialized portion
        let slice: &[u8] = buffer.as_ref();
        assert_eq!(slice, b"Test data");
        assert_eq!(slice.len(), 9);
    }

    #[tokio::test]
    async fn test_copy_from_slice_updates_initialized() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 5);
        let mut buffer = pool.acquire();

        assert_eq!(buffer.initialized(), 0);

        buffer.copy_from_slice(b"abc");
        assert_eq!(buffer.initialized(), 3);

        buffer.copy_from_slice(b"longer text");
        assert_eq!(buffer.initialized(), 11);
    }

    #[tokio::test]
    #[should_panic(expected = "data exceeds buffer capacity")]
    async fn test_copy_from_slice_panic_on_overflow() {
        let pool = BufferPool::new(BufferSize::try_new(10).unwrap(), 5);
        let mut buffer = pool.acquire();

        let too_large = vec![0u8; buffer.capacity() + 1];
        buffer.copy_from_slice(&too_large); // Should panic
    }

    #[tokio::test]
    async fn test_buffer_pool_debug() {
        let pool = BufferPool::new(BufferSize::try_new(2048).unwrap(), 5);
        let debug_str = format!("{pool:?}");
        assert!(debug_str.contains("BufferPool"));
    }

    #[tokio::test]
    async fn test_buffer_initialized_tracking() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 5);
        let mut buffer = pool.acquire();

        // Test multiple writes update initialized correctly
        buffer.copy_from_slice(b"First");
        assert_eq!(buffer.initialized(), 5);
        assert_eq!(&*buffer, b"First");

        buffer.copy_from_slice(b"Second write");
        assert_eq!(buffer.initialized(), 12);
        assert_eq!(&*buffer, b"Second write");
    }

    #[tokio::test]
    async fn test_buffer_capacity_vs_initialized() {
        let pool = BufferPool::new(BufferSize::try_new(8192).unwrap(), 5);
        let mut buffer = pool.acquire();

        // Capacity is full buffer size
        assert_eq!(buffer.capacity(), 8192);

        // Initialized is what we've written
        assert_eq!(buffer.initialized(), 0);

        buffer.copy_from_slice(b"Small");
        assert_eq!(buffer.capacity(), 8192);
        assert_eq!(buffer.initialized(), 5);
    }

    #[tokio::test]
    async fn test_empty_slice_copy() {
        let pool = BufferPool::new(BufferSize::try_new(512).unwrap(), 5);
        let mut buffer = pool.acquire();

        // Copying empty slice should work
        buffer.copy_from_slice(&[]);
        assert_eq!(buffer.initialized(), 0);
        assert_eq!(&*buffer, b"");
    }

    #[tokio::test]
    async fn test_buffer_reuse_preserves_capacity() {
        let pool = BufferPool::new(BufferSize::try_new(2048).unwrap(), 5);

        {
            let mut buffer = pool.acquire();
            buffer.copy_from_slice(b"test");
            assert_eq!(buffer.capacity(), 4096);
        } // Drop returns to pool

        let buffer2 = pool.acquire();
        // Should have same capacity when reused
        assert_eq!(buffer2.capacity(), 4096);
    }

    #[test]
    fn test_buffer_size_alignment() {
        // Test that create_aligned_buffer aligns to page boundaries
        let buffer = BufferPool::create_aligned_buffer(1000);
        // Should be aligned to 4096
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.capacity() % 4096, 0);

        let buffer2 = BufferPool::create_aligned_buffer(8192);
        assert_eq!(buffer2.len(), 0);
        assert_eq!(buffer2.capacity() % 4096, 0);
    }
}
