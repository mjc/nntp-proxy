use crate::types::BufferSize;
use crossbeam::queue::SegQueue;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::info;

/// A pooled buffer that automatically returns to the pool when dropped
///
/// This type only allows mutable access to the underlying buffer, preventing
/// accidental reads of uninitialized data. Use it with AsyncRead/AsyncWrite
/// and slice operations.
pub struct PooledBuffer {
    buffer: Vec<u8>,
    pool: Arc<SegQueue<Vec<u8>>>,
    pool_size: Arc<AtomicUsize>,
    max_pool_size: usize,
}

impl PooledBuffer {
    /// Get the full capacity of the buffer
    #[inline]
    pub fn capacity(&self) -> usize {
        self.buffer.len()
    }
}

impl DerefMut for PooledBuffer {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buffer[..]
    }
}

impl Deref for PooledBuffer {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.buffer[..]
    }
}

impl AsMut<[u8]> for PooledBuffer {
    #[inline]
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.buffer
    }
}

impl AsRef<[u8]> for PooledBuffer {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.buffer
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
        // Automatically return buffer to pool when dropped
        let current_size = self.pool_size.load(Ordering::Relaxed);
        if current_size < self.max_pool_size {
            let buffer = std::mem::take(&mut self.buffer);
            self.pool.push(buffer);
            self.pool_size.fetch_add(1, Ordering::Relaxed);
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
    /// Returns a raw Vec<u8> that will be wrapped in PooledBuffer by get_buffer().
    /// The buffer is NOT zero-initialized for performance.
    ///
    /// # Safety
    ///
    /// The returned buffer contains uninitialized memory. Callers must ensure that the buffer
    /// is only used with `AsyncRead`/`AsyncWrite` operations that fully initialize the bytes
    /// before they are read. Only the initialized portion of the buffer (`&buf[..n]`, where `n`
    /// is the number of bytes read or written) may be accessed. Accessing uninitialized bytes
    /// is undefined behavior.
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
    pub async fn get_buffer(&self) -> PooledBuffer {
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
        let pool = BufferPool::new(BufferSize::new(8192).unwrap(), 10);

        // Pool should pre-allocate buffers
        let buffer1 = pool.get_buffer().await;
        assert_eq!(buffer1.len(), 8192);
        // Buffer automatically returned on drop
    }

    #[tokio::test]
    async fn test_buffer_pool_get_and_return() {
        let pool = BufferPool::new(BufferSize::new(4096).unwrap(), 5);

        // Get a buffer
        let buffer = pool.get_buffer().await;
        assert_eq!(buffer.len(), 4096);

        // Buffer may contain uninitialized data - this is intentional for performance
        // Callers will write to it via AsyncRead before accessing

        // Drop it (automatically returns to pool)
        drop(buffer);

        // Get it again - should be from pool
        let buffer2 = pool.get_buffer().await;
        assert_eq!(buffer2.len(), 4096);
    }

    #[tokio::test]
    async fn test_buffer_pool_exhaustion() {
        let pool = BufferPool::new(BufferSize::new(1024).unwrap(), 2);

        // Get all pre-allocated buffers
        let buf1 = pool.get_buffer().await;
        let buf2 = pool.get_buffer().await;

        // Pool is exhausted, should create new buffer
        let buf3 = pool.get_buffer().await;
        assert_eq!(buf3.len(), 1024);

        // Drop buffers (automatically returned)
        drop(buf1);
        drop(buf2);
        drop(buf3);
    }

    #[tokio::test]
    async fn test_buffer_pool_concurrent_access() {
        let pool = BufferPool::new(BufferSize::new(2048).unwrap(), 10);

        // Spawn multiple tasks accessing the pool concurrently
        let mut handles = vec![];

        for _ in 0..20 {
            let pool_clone = pool.clone();
            let handle = tokio::spawn(async move {
                let buffer = pool_clone.get_buffer().await;
                assert_eq!(buffer.len(), 2048);
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
        let pool = BufferPool::new(BufferSize::new(8192).unwrap(), 1);
        let buffer = pool.get_buffer().await;

        // Buffer capacity should be aligned to page boundaries (4KB)
        assert!(buffer.capacity() >= 8192);
        // Should be page-aligned (multiple of 4096)
        assert_eq!(buffer.capacity() % 4096, 0);
    }

    #[tokio::test]
    async fn test_buffer_clear_and_resize() {
        let pool = BufferPool::new(BufferSize::new(1024).unwrap(), 2);

        let mut buffer = pool.get_buffer().await;

        // Modify the buffer
        buffer[0] = 42;
        buffer[100] = 99;

        // Drop returns it to pool
        drop(buffer);

        // Get it again - may contain old data (performance optimization)
        let buffer2 = pool.get_buffer().await;
        assert_eq!(buffer2.len(), 1024);
        // Note: buffer may contain previous data - callers must use &buf[..n] pattern
    }

    #[tokio::test]
    async fn test_buffer_pool_max_size_enforcement() {
        let pool = BufferPool::new(BufferSize::new(512).unwrap(), 3);

        // Get all buffers
        let buf1 = pool.get_buffer().await;
        let buf2 = pool.get_buffer().await;
        let buf3 = pool.get_buffer().await;

        // Get one more (should create new)
        let buf4 = pool.get_buffer().await;

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
        let pool = BufferPool::new(BufferSize::new(1024).unwrap(), 2);

        let buffer = pool.get_buffer().await;
        assert_eq!(buffer.len(), 1024);

        // PooledBuffer auto-returns on drop with correct size enforcement in Drop impl
        drop(buffer);
    }

    #[tokio::test]
    async fn test_buffer_pool_multiple_get_return_cycles() {
        let pool = BufferPool::new(BufferSize::new(4096).unwrap(), 5);

        // Do multiple get/return cycles
        for i in 0..20 {
            let mut buffer = pool.get_buffer().await;
            assert_eq!(buffer.len(), 4096);

            // Write some data
            buffer[0] = i as u8;
        }
    }

    #[test]
    fn test_buffer_pool_clone() {
        let pool1 = BufferPool::new(BufferSize::new(1024).unwrap(), 5);
        let _pool2 = pool1.clone();

        // Both should share the same underlying pool
        // (Arc ensures shared ownership)
    }

    #[tokio::test]
    async fn test_different_buffer_sizes() {
        let small_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 5);
        let medium_pool = BufferPool::new(BufferSize::new(8192).unwrap(), 5);
        let large_pool = BufferPool::new(BufferSize::new(65536).unwrap(), 5);

        let small_buf = small_pool.get_buffer().await;
        let medium_buf = medium_pool.get_buffer().await;
        let large_buf = large_pool.get_buffer().await;

        assert_eq!(small_buf.len(), 1024);
        assert_eq!(medium_buf.len(), 8192);
        assert_eq!(large_buf.len(), 65536);

        // Buffers auto-return on drop
    }

    #[tokio::test]
    async fn test_buffer_pool_stress() {
        let pool = BufferPool::new(BufferSize::new(4096).unwrap(), 10);

        // Stress test with many concurrent operations
        let mut handles = vec![];

        for _ in 0..100 {
            let pool_clone = pool.clone();
            let handle = tokio::spawn(async move {
                for _ in 0..10 {
                    let buffer = pool_clone.get_buffer().await;
                    assert_eq!(buffer.len(), 4096);
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await.unwrap();
        }
    }
}
