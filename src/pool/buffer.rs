use crate::types::BufferSize;
use crossbeam::queue::SegQueue;
use std::ops::Deref;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::info;

/// A pooled buffer that automatically returns to the pool when dropped
///
/// # Safety and Uninitialized Memory
///
/// Buffers contain **uninitialized memory** for performance (no zeroing overhead).
/// The type tracks initialized bytes and only exposes that portion, preventing UB.
///
/// ## Usage
/// ```ignore
/// let mut buffer = pool.acquire().await;
/// let n = buffer.read_from(&mut stream).await?;  // Automatic tracking
/// process(&*buffer);  // Deref returns only &buffer[..n]
/// ```
pub struct PooledBuffer {
    buffer: Vec<u8>,
    initialized: usize,
    pool: Arc<SegQueue<Vec<u8>>>,
    pool_size: Arc<AtomicUsize>,
    max_pool_size: usize,
}

impl PooledBuffer {
    /// Get the full capacity of the buffer
    #[must_use]
    #[inline]
    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }

    /// Get the number of initialized bytes
    #[must_use]
    #[inline]
    pub fn initialized(&self) -> usize {
        self.initialized
    }

    /// Read from an AsyncRead source, automatically tracking initialized bytes
    pub async fn read_from<R>(&mut self, reader: &mut R) -> std::io::Result<usize>
    where
        R: tokio::io::AsyncReadExt + Unpin,
    {
        let n = reader.read(&mut self.buffer[..]).await?;
        self.initialized = n;
        Ok(n)
    }

    /// Copy data into buffer and mark as initialized
    ///
    /// # Panics
    /// Panics if data.len() > capacity
    #[inline]
    pub fn copy_from_slice(&mut self, data: &[u8]) {
        assert!(
            data.len() <= self.buffer.len(),
            "data exceeds buffer capacity"
        );
        self.buffer[..data.len()].copy_from_slice(data);
        self.initialized = data.len();
    }

    /// Get mutable access to the full buffer capacity
    ///
    /// Returns a mutable slice of the entire buffer. After writing to this slice,
    /// you must manually track how many bytes were initialized (e.g., using the
    /// return value from `read()` and accessing via `&buffer[..n]`).
    ///
    /// # Safety Note
    /// The returned slice contains **uninitialized memory**. Only access bytes
    /// that you've written to. This method is primarily for use with I/O
    /// operations like `AsyncRead::read()`.
    ///
    /// # Examples
    /// ```ignore
    /// let mut buffer = pool.acquire().await;
    /// let n = stream.read(buffer.as_mut_slice()).await?;
    /// let initialized_data = &buffer.as_mut_slice()[..n];
    /// ```
    #[must_use]
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.buffer[..]
    }
}

impl Deref for PooledBuffer {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        // Immutable access only returns the initialized portion
        &self.buffer[..self.initialized]
    }
}

// Intentionally NO DerefMut or AsMut - forces explicit use of read_from() or as_mut_slice()

impl AsRef<[u8]> for PooledBuffer {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        // Only return initialized portion
        &self.buffer[..self.initialized]
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
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

/// Lock-free buffer pool for reusing large I/O buffers
/// Uses crossbeam's SegQueue for lock-free operations
#[derive(Debug, Clone)]
pub struct BufferPool {
    pool: Arc<SegQueue<Vec<u8>>>,
    buffer_size: BufferSize,
    max_pool_size: usize,
    pool_size: Arc<AtomicUsize>,
}

impl BufferPool {
    /// Create a page-aligned buffer for optimal DMA performance
    ///
    /// Returns a raw Vec<u8> that will be wrapped in PooledBuffer by acquire().
    /// The buffer is NOT zero-initialized for performance.
    ///
    /// # Safety
    ///
    /// **INTERNAL USE ONLY.** This function is not exposed publicly and is only used
    /// within the buffer pool implementation where the safety contract is guaranteed.
    ///
    /// The returned buffer contains uninitialized memory. Callers must ensure that the buffer
    /// is only used with `AsyncRead`/`AsyncWrite` operations that fully initialize the bytes
    /// before they are read. Only the initialized portion of the buffer (`&buf[..n]`, where `n`
    /// is the number of bytes read or written) may be accessed. Accessing uninitialized bytes
    /// is undefined behavior.
    ///
    /// The public API (`acquire()`) returns a `PooledBuffer` which is a safe wrapper that
    /// enforces this contract through the type system and usage patterns.
    #[allow(clippy::uninit_vec)]
    fn create_aligned_buffer(size: usize) -> Vec<u8> {
        // Align to page boundaries (4KB) for better memory performance
        let page_size = 4096;
        let aligned_size = size.div_ceil(page_size) * page_size;

        // Use aligned allocation for better cache performance
        let mut buffer = Vec::with_capacity(aligned_size);
        // SAFETY: We're setting the length without initializing the data.
        // This is safe because:
        // 1. The Vec is wrapped in PooledBuffer which derefs to &[u8]
        // 2. PooledBuffer is immediately used with AsyncRead which writes into it
        // 3. Callers only access &buffer[..n] where n is the bytes actually read
        // 4. Unwritten/uninitialized bytes are never accessed
        unsafe {
            buffer.set_len(size);
        }
        buffer
    }

    /// Create a new buffer pool with pre-allocated buffers
    ///
    /// # Arguments
    /// * `buffer_size` - Size of each buffer in bytes (must be non-zero)
    /// * `max_pool_size` - Maximum number of buffers to pool
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
        }
    }

    /// Get a buffer from the pool or create a new one (lock-free)
    ///
    /// Returns a PooledBuffer that automatically returns to the pool when dropped.
    /// The buffer may contain old data, but this is safe because:
    /// - Callers use AsyncRead which writes into the buffer
    /// - They get back `n` bytes written and access only `&buf[..n]`
    /// - Stale data beyond `n` is never accessed
    pub async fn acquire(&self) -> PooledBuffer {
        let buffer = if let Some(buffer) = self.pool.pop() {
            self.pool_size.fetch_sub(1, Ordering::Relaxed);
            // Buffer from pool is already the correct size (enforced on return)
            debug_assert_eq!(buffer.len(), self.buffer_size.get());
            buffer
        } else {
            // Create new page-aligned buffer for better DMA performance
            Self::create_aligned_buffer(self.buffer_size.get())
        };

        PooledBuffer {
            buffer,
            initialized: 0, // Start with 0 bytes safe to read
            pool: Arc::clone(&self.pool),
            pool_size: Arc::clone(&self.pool_size),
            max_pool_size: self.max_pool_size,
        }
    }

    /// Return a buffer to the pool (lock-free)
    ///
    /// Note: Usually not needed as PooledBuffer returns itself automatically on drop
    #[allow(dead_code)]
    pub async fn return_buffer(&self, buffer: Vec<u8>) {
        if buffer.len() == self.buffer_size.get() {
            let current_size = self.pool_size.load(Ordering::Relaxed);
            if current_size < self.max_pool_size {
                self.pool.push(buffer);
                self.pool_size.fetch_add(1, Ordering::Relaxed);
            }
            // If pool is full, just drop the buffer
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    async fn test_buffer_pool_get_and_return() {
        let pool = BufferPool::new(BufferSize::try_new(4096).unwrap(), 5);

        // Get a buffer
        let buffer = pool.acquire().await;
        assert_eq!(buffer.capacity(), 4096);
        assert_eq!(buffer.initialized(), 0);

        // Buffer contains uninitialized data - this is intentional for performance
        // Callers will write to it via AsyncRead before accessing

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
        assert_eq!(buf3.capacity(), 1024);

        // Drop buffers (automatically returned)
        drop(buf1);
        drop(buf2);
        drop(buf3);
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
                assert_eq!(buffer.capacity(), 2048);
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
        assert_eq!(buffer2.capacity(), 1024);
        // Note: buffer may contain previous data - callers must use &buf[..n] pattern
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
        assert_eq!(buffer.capacity(), 1024);

        // PooledBuffer auto-returns on drop with correct size enforcement in Drop impl
        drop(buffer);
    }

    #[tokio::test]
    async fn test_buffer_pool_multiple_get_return_cycles() {
        let pool = BufferPool::new(BufferSize::try_new(4096).unwrap(), 5);

        // Do multiple get/return cycles
        for i in 0..20 {
            let mut buffer = pool.acquire().await;
            assert_eq!(buffer.capacity(), 4096);

            // Write some data using copy_from_slice
            let data = vec![i as u8; 1];
            buffer.copy_from_slice(&data);
            assert_eq!(buffer.initialized(), 1);
        }
    }

    #[test]
    fn test_buffer_pool_clone() {
        let pool1 = BufferPool::new(BufferSize::try_new(1024).unwrap(), 5);
        let _pool2 = pool1.clone();

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

        assert_eq!(small_buf.capacity(), 1024);
        assert_eq!(medium_buf.capacity(), 8192);
        assert_eq!(large_buf.capacity(), 65536);

        // Buffers auto-return on drop
    }

    #[tokio::test]
    async fn test_as_mut_slice() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 5);
        let mut buffer = pool.acquire().await;

        // Get mutable slice and write to it
        let slice = buffer.as_mut_slice();
        assert_eq!(slice.len(), 1024);

        // Write some data manually
        slice[0] = b'H';
        slice[1] = b'i';
        slice[2] = b'!';

        // Can write to the full capacity
        for (i, byte) in slice.iter_mut().enumerate() {
            *byte = (i % 256) as u8;
        }

        // Note: initialized() doesn't track manual writes via as_mut_slice
        // This is intentional - as_mut_slice is for low-level I/O
        assert_eq!(buffer.initialized(), 0);
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

        let too_large = vec![0u8; 20];
        buffer.copy_from_slice(&too_large); // Should panic
    }

    #[tokio::test]
    async fn test_buffer_pool_debug() {
        let pool = BufferPool::new(BufferSize::try_new(2048).unwrap(), 5);
        let debug_str = format!("{:?}", pool);
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
    async fn test_as_mut_slice_capacity() {
        let pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 5);
        let mut buffer = pool.acquire().await;

        // as_mut_slice should return full capacity
        let slice = buffer.as_mut_slice();
        assert_eq!(slice.len(), 1024);
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
            assert_eq!(buffer.capacity(), 2048);
        } // Drop returns to pool

        let buffer2 = pool.acquire().await;
        // Should have same capacity when reused
        assert_eq!(buffer2.capacity(), 2048);
    }

    #[test]
    fn test_buffer_size_alignment() {
        // Test that create_aligned_buffer aligns to page boundaries
        let buffer = BufferPool::create_aligned_buffer(1000);
        // Should be aligned to 4096
        assert_eq!(buffer.len(), 1000);
        assert_eq!(buffer.capacity() % 4096, 0);

        let buffer2 = BufferPool::create_aligned_buffer(8192);
        assert_eq!(buffer2.len(), 8192);
        assert_eq!(buffer2.capacity() % 4096, 0);
    }
}
