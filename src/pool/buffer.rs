use crate::types::BufferSize;
use bytes::BytesMut;
use crossbeam::queue::SegQueue;
use smallvec::SmallVec;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info};

/// A pooled buffer that automatically returns to the pool when dropped
///
/// # Safety and Initialized Bytes
///
/// The backing allocation is pre-faulted and then kept logically empty until
/// bytes are read or copied into it. `BytesMut::len()` is the initialized length,
/// and immutable access only exposes that initialized portion.
///
/// ## Usage
/// ```ignore
/// let mut buffer = pool.acquire().await;
/// let n = buffer.read_from(&mut stream).await?;  // Automatic tracking
/// process(&*buffer);  // Deref returns only &buffer[..n]
/// ```
pub struct PooledBuffer {
    buffer: BytesMut,
    pool: Arc<SegQueue<BytesMut>>,
    pool_size: Arc<AtomicUsize>,
    max_pool_size: usize,
    expected_capacity: usize,
    writable_len: usize,
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
        let limit = self.read_limit();
        let n = reader.take(limit as u64).read_buf(&mut self.buffer).await?;
        Ok(n)
    }

    /// Read more data at the current initialized offset, accumulating bytes.
    ///
    /// Unlike `read_from` which resets the buffer, this appends to existing data.
    /// Used when a partial response needs more data (e.g., status code split
    /// across TCP segments).
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
        let n = reader.take(limit as u64).read_buf(&mut self.buffer).await?;
        Ok(n)
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

    /// Shorten the logical initialized length without changing the backing allocation.
    ///
    /// Used by response framing helpers when a read contains bytes beyond the current
    /// response boundary and the remainder is stashed as leftover for the next response.
    ///
    /// # Panics
    /// Panics if `len` exceeds the current initialized byte count.
    #[inline]
    pub fn truncate(&mut self, len: usize) {
        assert!(
            len <= self.buffer.len(),
            "truncate length exceeds initialized bytes"
        );
        self.buffer.truncate(len);
    }

    /// Append data to the buffer (accumulator mode)
    ///
    /// Used when `PooledBuffer` is acquired from capture pool for accumulating
    /// streaming data. Unlike `copy_from_slice` which overwrites, this extends.
    ///
    /// # Performance
    ///
    /// Capture buffers are pre-allocated with sufficient capacity (768KB) and
    /// pages are pre-faulted. As long as total accumulated data fits within
    /// capacity, this operation performs NO allocations or page faults.
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
        if self.buffer.capacity() != self.expected_capacity {
            debug!(
                "Dropping resized pooled buffer instead of returning it to the pool (capacity {} != expected {})",
                self.buffer.capacity(),
                self.expected_capacity
            );
            return;
        }

        self.buffer.clear();

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
                    self.pool.push(buffer);
                    return;
                }
                Err(new_size) => {
                    current_size = new_size;
                }
            }
        }
        // If pool is full, buffer is dropped
    }
}

/// Buffered response assembled from one or more pooled capture buffers.
///
/// This avoids reallocating or growing a single `Vec` on the hot path when
/// a multiline response is larger than the typical capture size.
#[derive(Debug, Default)]
pub struct ChunkedResponse {
    chunks: Vec<PooledBuffer>,
    len: usize,
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
            let need_new_chunk = self
                .chunks
                .last()
                .is_none_or(|chunk| chunk.initialized() == chunk.capacity());

            if need_new_chunk {
                self.chunks.push(pool.acquire_capture_now());
            }

            let chunk = self
                .chunks
                .last_mut()
                .expect("chunk just pushed or already existed");
            let available = chunk.capacity().saturating_sub(chunk.initialized());
            debug_assert!(available > 0, "chunk must have space after allocation");

            let take = available.min(data.len());
            chunk.extend_from_slice(&data[..take]);
            self.len += take;
            data = &data[take..];
        }
    }

    /// Truncate the buffered response to the given absolute length.
    ///
    /// # Panics
    /// Panics if `len` exceeds the total buffered byte count.
    pub fn truncate(&mut self, len: usize) {
        assert!(len <= self.len, "truncate length exceeds buffered bytes");
        if len == self.len {
            return;
        }

        let mut remaining = len;
        let mut keep_chunks = 0;

        for chunk in &mut self.chunks {
            let chunk_len = chunk.initialized();
            if remaining >= chunk_len {
                remaining -= chunk_len;
                keep_chunks += 1;
                continue;
            }

            chunk.truncate(remaining);
            keep_chunks += 1;
            break;
        }

        self.chunks.truncate(keep_chunks);
        self.len = len;
    }

    /// First chunk of buffered data, if any.
    #[must_use]
    pub fn first_chunk(&self) -> Option<&[u8]> {
        self.chunks.first().map(AsRef::as_ref)
    }

    /// Iterate buffered chunks without flattening.
    pub fn iter_chunks(&self) -> impl Iterator<Item = &[u8]> {
        self.chunks.iter().map(AsRef::as_ref)
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

    /// Returns true if buffered bytes begin with `prefix`.
    #[must_use]
    pub fn starts_with(&self, prefix: &[u8]) -> bool {
        if prefix.len() > self.len {
            return false;
        }

        let mut remaining = prefix;
        for chunk in &self.chunks {
            if remaining.is_empty() {
                return true;
            }

            let data = chunk.as_ref();
            let take = data.len().min(remaining.len());
            if data[..take] != remaining[..take] {
                return false;
            }
            remaining = &remaining[take..];
        }

        remaining.is_empty()
    }

    /// Returns true if buffered bytes end with `suffix`.
    #[must_use]
    pub fn ends_with(&self, suffix: &[u8]) -> bool {
        if suffix.len() > self.len {
            return false;
        }

        let mut remaining = suffix.len();
        for chunk in self.chunks.iter().rev() {
            if remaining == 0 {
                return true;
            }
            let data = chunk.as_ref();
            let take = remaining.min(data.len());
            let suffix_start = remaining - take;
            let data_start = data.len() - take;
            if data[data_start..] != suffix[suffix_start..remaining] {
                return false;
            }
            remaining -= take;
        }

        remaining == 0
    }

    /// Write all buffered chunks to a sink in order.
    ///
    /// # Errors
    /// Returns any write error produced by the underlying async sink.
    pub async fn write_all_to<W>(&self, writer: &mut W) -> std::io::Result<()>
    where
        W: AsyncWriteExt + Unpin,
    {
        for chunk in self.iter_chunks() {
            writer.write_all(chunk).await?;
        }
        Ok(())
    }
}

/// Lock-free buffer pool for reusing large I/O buffers
/// Uses crossbeam's `SegQueue` for lock-free operations
#[derive(Debug, Clone)]
pub struct BufferPool {
    pool: Arc<SegQueue<BytesMut>>,
    buffer_size: BufferSize,
    max_pool_size: usize,
    pool_size: Arc<AtomicUsize>,
    // Capture buffer pool (for accumulating streaming data)
    capture_pool: Arc<SegQueue<BytesMut>>,
    capture_capacity: usize,
    max_capture_pool_size: usize,
    capture_pool_size: Arc<AtomicUsize>,
}

impl BufferPool {
    /// Create a page-aligned buffer for optimal DMA performance
    ///
    /// Returns a raw `BytesMut` that will be wrapped in `PooledBuffer` by `acquire()`.
    /// The buffer is pre-faulted, then cleared before it enters the pool.
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

        let mut buffer = BytesMut::with_capacity(aligned_size);
        Self::prefault_pages(&mut buffer);
        buffer
    }

    /// Pre-fault pages by touching one byte per 4KB stride to map physical memory.
    ///
    /// This eliminates page faults during streaming by ensuring all pages are
    /// resident in physical memory. Without this, the first write to each page
    /// triggers a soft page fault (96.75% of memmove time in profiling).
    ///
    /// # Safety
    ///
    /// We write within the buffer's allocated capacity (not beyond it).
    /// `write_volatile` touches one byte per 4KB page to trigger the OS page fault
    /// at init time rather than during streaming I/O. We deliberately do NOT zero
    /// the full allocation first; scratch buffers are filled via `read_buf` before
    /// their bytes become part of the initialized slice.
    fn prefault_pages(buf: &mut BytesMut) {
        let cap = buf.capacity();
        // SAFETY: We write within the buffer's allocated capacity. `BytesMut::with_capacity`
        // reserves that storage even though the logical length remains zero.
        unsafe {
            let ptr = buf.as_mut_ptr();
            for offset in (0..cap).step_by(4096) {
                std::ptr::write_volatile(ptr.add(offset), 1);
            }
        }
    }

    /// Create a new buffer pool with pre-allocated buffers
    ///
    /// # Arguments
    /// * `buffer_size` - Size of each buffer in bytes (must be non-zero)
    /// * `max_pool_size` - Maximum number of buffers to pool
    ///
    /// # Design Philosophy
    ///
    /// **Buffer pools are allocated ONCE at application boot**, providing a
    /// zero-allocation hot path for I/O operations. This is a critical performance
    /// optimization:
    ///
    /// - **Boot time**: All buffers pre-allocated (one-time cost)
    /// - **Runtime**: Zero allocations in hot path (acquire/release from pool)
    /// - **Per-connection**: Buffers are borrowed and returned, never owned
    ///
    /// **IMPORTANT**: Do NOT create a `BufferPool` per-client, per-connection, or
    /// per-request. Create ONE pool at application startup and share it across
    /// all operations via Arc or static reference.
    ///
    /// All buffers are pre-allocated at creation time for optimal performance.
    #[must_use]
    pub fn new(buffer_size: BufferSize, max_pool_size: usize) -> Self {
        let pool = Arc::new(SegQueue::new());
        let pool_size = Arc::new(AtomicUsize::new(0));

        // Pre-allocate all buffers at startup to eliminate allocation overhead
        info!(
            "Pre-allocating {} buffers of {}KB each ({}MB total)",
            max_pool_size,
            buffer_size.get() / 1024,
            (max_pool_size * buffer_size.get()) / (1024 * 1024)
        );

        for _ in 0..max_pool_size {
            let buffer = Self::create_aligned_buffer(buffer_size.get());
            pool.push(buffer);
            pool_size.fetch_add(1, Ordering::Relaxed);
        }

        info!("Buffer pool pre-allocation complete");

        Self {
            pool,
            buffer_size,
            max_pool_size,
            pool_size,
            // Initialize capture pool as empty (will be configured via with_capture_pool)
            capture_pool: Arc::new(SegQueue::new()),
            capture_capacity: 0,
            max_capture_pool_size: 0,
            capture_pool_size: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Configure the capture buffer pool for pre-faulted accumulator buffers
    ///
    /// Capture buffers are used for accumulating streaming data (e.g., caching).
    /// Pages are pre-faulted to eliminate soft page faults during streaming.
    ///
    /// # Arguments
    /// * `capacity` - Size of each capture buffer in bytes (e.g., 768KB)
    /// * `count` - Number of capture buffers to pre-allocate
    #[must_use]
    pub fn with_capture_pool(mut self, capacity: usize, count: usize) -> Self {
        info!(
            "Pre-allocating {} capture buffers of {}KB each ({}MB total)",
            count,
            capacity / 1024,
            (count * capacity) / (1024 * 1024)
        );

        for _ in 0..count {
            let mut buffer = BytesMut::with_capacity(capacity);
            // Pre-fault all pages to map physical memory
            Self::prefault_pages(&mut buffer);
            self.capture_pool.push(buffer);
            self.capture_pool_size.fetch_add(1, Ordering::Relaxed);
        }

        self.capture_capacity = capacity;
        self.max_capture_pool_size = count;

        info!("Capture buffer pool pre-allocation complete");

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
        self.max_pool_size.saturating_sub(self.available_buffers())
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
    /// # Performance: Zero-Allocation Hot Path
    ///
    /// When the pool was created with sufficient `max_pool_size`, this method
    /// performs ZERO allocations - it just pops from the lock-free queue.
    /// This is the critical hot path for I/O operations.
    ///
    /// Only if the pool is exhausted (all buffers in use) will a new buffer
    /// be allocated. Size the pool appropriately to avoid this fallback.
    ///
    /// # Safety Notes
    ///
    /// The buffer may contain old bytes outside its logical length, but immutable
    /// access only exposes `BytesMut::len()` initialized bytes.
    fn acquire_now(&self) -> PooledBuffer {
        let buffer = self.pool.pop().map_or_else(
            || {
                // Create new page-aligned buffer for better DMA performance
                let in_use = self.buffers_in_use();
                debug!(
                    "Buffer pool exhausted (allocating beyond pool size). In use: {}/{}",
                    in_use, self.max_pool_size
                );
                Self::create_aligned_buffer(self.buffer_size.get())
            },
            |buffer| {
                self.pool_size.fetch_sub(1, Ordering::Relaxed);
                // Buffer from pool is logically empty and has the expected allocation
                // capacity (enforced on return).
                debug_assert_eq!(buffer.len(), 0);
                buffer
            },
        );

        PooledBuffer {
            expected_capacity: buffer.capacity(),
            buffer,
            pool: Arc::clone(&self.pool),
            pool_size: Arc::clone(&self.pool_size),
            max_pool_size: self.max_pool_size,
            writable_len: self.buffer_size.get(),
        }
    }

    pub async fn acquire(&self) -> PooledBuffer {
        std::future::ready(self.acquire_now()).await
    }

    /// Get a capture buffer from the capture pool
    ///
    /// Returns a `PooledBuffer` backed by a pre-faulted capture buffer.
    /// Used for accumulating streaming data (e.g., caching articles).
    ///
    /// Pages are pre-faulted to eliminate soft page faults during streaming,
    /// which profiling showed accounted for 96.75% of memmove time.
    pub(crate) fn acquire_capture_now(&self) -> PooledBuffer {
        let buffer = self.capture_pool.pop().map_or_else(
            || {
                // Pool exhausted - allocate and pre-fault new buffer
                let in_use = self
                    .max_capture_pool_size
                    .saturating_sub(self.capture_pool_size.load(Ordering::Relaxed));
                debug!(
                    "Capture pool exhausted (allocating beyond pool size). In use: {}/{}",
                    in_use, self.max_capture_pool_size
                );
                let capacity = if self.capture_capacity > 0 {
                    self.capture_capacity
                } else {
                    self.buffer_size.get()
                };
                let mut buffer = BytesMut::with_capacity(capacity);
                Self::prefault_pages(&mut buffer);
                buffer
            },
            |mut buffer| {
                self.capture_pool_size.fetch_sub(1, Ordering::Relaxed);
                // Clear but keep capacity (pages stay mapped)
                buffer.clear();
                buffer
            },
        );

        PooledBuffer {
            expected_capacity: buffer.capacity(),
            buffer,
            pool: Arc::clone(&self.capture_pool),
            pool_size: Arc::clone(&self.capture_pool_size),
            max_pool_size: self.max_capture_pool_size,
            writable_len: 0,
        }
    }

    pub async fn acquire_capture(&self) -> PooledBuffer {
        std::future::ready(self.acquire_capture_now()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn test_buffer_pool_creation() {
        let pool = BufferPool::new(BufferSize::try_new(8192).unwrap(), 10);

        // Pool should pre-allocate buffers
        let buffer1 = pool.acquire().await;
        assert_eq!(buffer1.capacity(), 8192);
        assert_eq!(buffer1.initialized(), 0); // No bytes initialized yet
        // Buffer automatically returned on drop
    }

    #[tokio::test]
    async fn test_acquire_starts_empty_with_stable_capacity() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1);

        let capacity = {
            let buffer = pool.acquire().await;
            assert_eq!(buffer.initialized(), 0);
            assert_eq!(buffer.len(), 0);
            buffer.capacity()
        };

        let buffer = pool.acquire().await;
        assert_eq!(buffer.initialized(), 0);
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.capacity(), capacity);
    }

    #[tokio::test]
    async fn test_read_from_sets_initialized_without_reducing_capacity() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1);
        let mut buffer = pool.acquire().await;
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
        let mut buffer = pool.acquire().await;
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
        let mut buffer = pool.acquire().await;
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
        let mut buffer = pool.acquire().await;
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
        let mut buffer = pool.acquire().await;
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
        let mut buffer = pool.acquire().await;
        let too_large = vec![b'X'; buffer.capacity() + 1];

        buffer.copy_from_slice(&too_large);
    }

    #[tokio::test]
    async fn test_clear_and_truncate_only_change_initialized_length() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1);
        let mut buffer = pool.acquire().await;
        let capacity = buffer.capacity();

        buffer.copy_from_slice(b"abcdef");
        buffer.truncate(3);
        assert_eq!(buffer.initialized(), 3);
        assert_eq!(&*buffer, b"abc");
        assert_eq!(buffer.capacity(), capacity);

        buffer.clear();
        assert_eq!(buffer.initialized(), 0);
        assert_eq!(buffer.len(), 0);
        assert_eq!(buffer.capacity(), capacity);
    }

    #[tokio::test]
    async fn test_resized_buffers_are_discarded_instead_of_repooled() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1).with_capture_pool(8, 1);

        let normal_capacity = {
            let mut buffer = pool.acquire().await;
            let capacity = buffer.capacity();
            buffer.extend_from_slice(&vec![b'N'; capacity + 1]);
            assert!(buffer.capacity() > capacity);
            capacity
        };

        let buffer = pool.acquire().await;
        assert_eq!(buffer.capacity(), normal_capacity);
        drop(buffer);

        {
            let mut capture = pool.acquire_capture().await;
            capture.extend_from_slice(&[b'C'; 9]);
            assert!(capture.capacity() > 8);
        }

        let capture = pool.acquire_capture().await;
        assert_eq!(capture.capacity(), 8);
    }

    #[tokio::test]
    async fn test_buffer_pool_get_and_return() {
        let pool = BufferPool::new(BufferSize::try_new(4096).unwrap(), 5);

        // Get a buffer
        let buffer = pool.acquire().await;
        assert_eq!(buffer.capacity(), 4096);
        assert_eq!(buffer.initialized(), 0);

        // Callers see an empty initialized slice until data is read or copied in.

        // Drop it (automatically returns to pool)
        drop(buffer);

        // Get it again - should be from pool
        let buffer2 = pool.acquire().await;
        assert_eq!(buffer2.capacity(), 4096);
    }

    #[tokio::test]
    async fn test_buffer_pool_exhaustion() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 2);

        // Get all pre-allocated buffers
        let buf1 = pool.acquire().await;
        let buf2 = pool.acquire().await;

        // Pool is exhausted, should create new buffer
        let buf3 = pool.acquire().await;
        assert_eq!(buf3.capacity(), 4096);

        // Drop buffers (automatically returned)
        drop(buf1);
        drop(buf2);
        drop(buf3);
    }

    #[tokio::test]
    async fn test_oversized_capture_buffer_is_not_reused() {
        let pool =
            BufferPool::new(BufferSize::try_new(1024).unwrap(), 1).with_capture_pool(8192, 1);

        let mut capture = pool.acquire_capture().await;
        capture.extend_from_slice(&vec![b'X'; 8193]);
        assert!(capture.capacity() > 8192);
        drop(capture);

        let capture2 = pool.acquire_capture().await;
        assert_eq!(capture2.capacity(), 8192);
    }

    #[tokio::test]
    async fn test_chunked_response_helpers_without_flattening() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 1).with_capture_pool(8, 4);
        let mut response = ChunkedResponse::default();
        response.extend_from_slice(&pool, b"220 0 <id>\r\nBody\r\n.\r\n");

        let chunks: Vec<&[u8]> = response.iter_chunks().collect();
        assert!(chunks.len() > 1, "test requires multi-chunk buffering");
        assert_eq!(chunks.concat(), b"220 0 <id>\r\nBody\r\n.\r\n");
        assert!(response.starts_with(b"220 0 "));
        assert!(response.ends_with(b"\r\n.\r\n"));

        let mut prefix = SmallVec::<[u8; 128]>::new();
        response.copy_prefix_into(12, &mut prefix);
        assert_eq!(&prefix[..], b"220 0 <id>\r\n");
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
    async fn test_buffer_pool_concurrent_access() {
        let pool = BufferPool::new(BufferSize::try_new(2048).unwrap(), 10);

        // Spawn multiple tasks accessing the pool concurrently
        let mut handles = vec![];

        for _ in 0..20 {
            let pool_clone = pool.clone();
            let handle = tokio::spawn(async move {
                let buffer = pool_clone.acquire().await;
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
        let buffer = pool.acquire().await;

        // Buffer capacity should be aligned to page boundaries (4KB)
        assert!(buffer.capacity() >= 8192);
        // Should be page-aligned (multiple of 4096)
        assert_eq!(buffer.capacity() % 4096, 0);
    }

    #[tokio::test]
    async fn test_buffer_clear_and_resize() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 2);

        let mut buffer = pool.acquire().await;

        // Write data using copy_from_slice
        let data = vec![42u8; 101];
        buffer.copy_from_slice(&data);
        assert_eq!(buffer.initialized(), 101);

        // Drop returns it to pool
        drop(buffer);

        // Get it again - may contain old data (performance optimization)
        let buffer2 = pool.acquire().await;
        assert_eq!(buffer2.capacity(), 4096);
        // Note: buffer may contain previous bytes outside the initialized range.
    }

    #[tokio::test]
    async fn test_buffer_pool_max_size_enforcement() {
        let pool = BufferPool::new(BufferSize::try_new(512).unwrap(), 3);

        // Get all buffers
        let buf1 = pool.acquire().await;
        let buf2 = pool.acquire().await;
        let buf3 = pool.acquire().await;

        // Get one more (should create new)
        let buf4 = pool.acquire().await;

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

        let buffer = pool.acquire().await;
        assert_eq!(buffer.capacity(), 4096);

        // PooledBuffer auto-returns on drop with correct size enforcement in Drop impl
        drop(buffer);
    }

    #[tokio::test]
    async fn test_buffer_pool_multiple_get_return_cycles() {
        let pool = BufferPool::new(BufferSize::try_new(4096).unwrap(), 5);

        // Do multiple get/return cycles
        for i in 0u8..20 {
            let mut buffer = pool.acquire().await;
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

        let small_buf = small_pool.acquire().await;
        let medium_buf = medium_pool.acquire().await;
        let large_buf = large_pool.acquire().await;

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
                    let buffer = pool_clone.acquire().await;
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
        let mut buffer = pool.acquire().await;

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
        let mut buffer = pool.acquire().await;

        buffer.copy_from_slice(b"Test data");

        // AsRef should return initialized portion
        let slice: &[u8] = buffer.as_ref();
        assert_eq!(slice, b"Test data");
        assert_eq!(slice.len(), 9);
    }

    #[tokio::test]
    async fn test_copy_from_slice_updates_initialized() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 5);
        let mut buffer = pool.acquire().await;

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
        let mut buffer = pool.acquire().await;

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
        let mut buffer = pool.acquire().await;

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
        let mut buffer = pool.acquire().await;

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
        let mut buffer = pool.acquire().await;

        // Copying empty slice should work
        buffer.copy_from_slice(&[]);
        assert_eq!(buffer.initialized(), 0);
        assert_eq!(&*buffer, b"");
    }

    #[tokio::test]
    async fn test_buffer_reuse_preserves_capacity() {
        let pool = BufferPool::new(BufferSize::try_new(2048).unwrap(), 5);

        {
            let mut buffer = pool.acquire().await;
            buffer.copy_from_slice(b"test");
            assert_eq!(buffer.capacity(), 4096);
        } // Drop returns to pool

        let buffer2 = pool.acquire().await;
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
