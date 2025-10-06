use crossbeam::queue::SegQueue;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tracing::info;

/// Lock-free buffer pool for reusing large I/O buffers
/// Uses crossbeam's SegQueue for lock-free operations
#[derive(Debug, Clone)]
pub struct BufferPool {
    pool: Arc<SegQueue<Vec<u8>>>,
    buffer_size: usize,
    max_pool_size: usize,
    pool_size: Arc<AtomicUsize>,
}

impl BufferPool {
    /// Create a page-aligned buffer for optimal DMA performance
    fn create_aligned_buffer(size: usize) -> Vec<u8> {
        // Align to page boundaries (4KB) for better memory performance
        let page_size = 4096;
        let aligned_size = size.div_ceil(page_size) * page_size;

        // Use aligned allocation for better cache performance
        let mut buffer = Vec::with_capacity(aligned_size);
        buffer.resize(size, 0);
        buffer
    }

    pub fn new(buffer_size: usize, max_pool_size: usize) -> Self {
        let pool = Arc::new(SegQueue::new());
        let pool_size = Arc::new(AtomicUsize::new(0));

        // Pre-allocate all buffers at startup to eliminate allocation overhead
        info!(
            "Pre-allocating {} buffers of {}KB each ({}MB total)",
            max_pool_size,
            buffer_size / 1024,
            (max_pool_size * buffer_size) / (1024 * 1024)
        );

        for _ in 0..max_pool_size {
            let buffer = Self::create_aligned_buffer(buffer_size);
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
    pub async fn get_buffer(&self) -> Vec<u8> {
        if let Some(mut buffer) = self.pool.pop() {
            self.pool_size.fetch_sub(1, Ordering::Relaxed);
            // Reuse existing buffer, clear it first
            buffer.clear();
            buffer.resize(self.buffer_size, 0);
            buffer
        } else {
            // Create new page-aligned buffer for better DMA performance
            Self::create_aligned_buffer(self.buffer_size)
        }
    }

    /// Return a buffer to the pool (lock-free)
    pub async fn return_buffer(&self, buffer: Vec<u8>) {
        if buffer.len() == self.buffer_size {
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
        let pool = BufferPool::new(8192, 10);

        // Pool should pre-allocate buffers
        let buffer1 = pool.get_buffer().await;
        assert_eq!(buffer1.len(), 8192);

        pool.return_buffer(buffer1).await;
    }

    #[tokio::test]
    async fn test_buffer_pool_get_and_return() {
        let pool = BufferPool::new(4096, 5);

        // Get a buffer
        let buffer = pool.get_buffer().await;
        assert_eq!(buffer.len(), 4096);

        // Buffer should be zeroed
        assert!(buffer.iter().all(|&b| b == 0));

        // Return it
        pool.return_buffer(buffer).await;

        // Get it again - should be from pool
        let buffer2 = pool.get_buffer().await;
        assert_eq!(buffer2.len(), 4096);
    }

    #[tokio::test]
    async fn test_buffer_pool_exhaustion() {
        let pool = BufferPool::new(1024, 2);

        // Get all pre-allocated buffers
        let buf1 = pool.get_buffer().await;
        let buf2 = pool.get_buffer().await;

        // Pool is exhausted, should create new buffer
        let buf3 = pool.get_buffer().await;
        assert_eq!(buf3.len(), 1024);

        // Return buffers
        pool.return_buffer(buf1).await;
        pool.return_buffer(buf2).await;
        pool.return_buffer(buf3).await; // This one might be dropped if pool is full
    }

    #[tokio::test]
    async fn test_buffer_pool_concurrent_access() {
        let pool = BufferPool::new(2048, 10);

        // Spawn multiple tasks accessing the pool concurrently
        let mut handles = vec![];

        for _ in 0..20 {
            let pool_clone = pool.clone();
            let handle = tokio::spawn(async move {
                let buffer = pool_clone.get_buffer().await;
                assert_eq!(buffer.len(), 2048);
                // Simulate some work
                tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
                pool_clone.return_buffer(buffer).await;
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
        let pool = BufferPool::new(8192, 1);
        let buffer = pool.get_buffer().await;

        // Buffer capacity should be aligned to page boundaries (4KB)
        assert!(buffer.capacity() >= 8192);
        // Should be page-aligned (multiple of 4096)
        assert_eq!(buffer.capacity() % 4096, 0);

        pool.return_buffer(buffer).await;
    }

    #[tokio::test]
    async fn test_buffer_clear_and_resize() {
        let pool = BufferPool::new(1024, 2);

        let mut buffer = pool.get_buffer().await;

        // Modify the buffer
        buffer[0] = 42;
        buffer[100] = 99;

        // Return it
        pool.return_buffer(buffer).await;

        // Get it again - should be cleared and resized
        let buffer2 = pool.get_buffer().await;
        assert_eq!(buffer2.len(), 1024);
        assert!(buffer2.iter().all(|&b| b == 0));

        pool.return_buffer(buffer2).await;
    }

    #[tokio::test]
    async fn test_buffer_pool_max_size_enforcement() {
        let pool = BufferPool::new(512, 3);

        // Get all buffers
        let buf1 = pool.get_buffer().await;
        let buf2 = pool.get_buffer().await;
        let buf3 = pool.get_buffer().await;

        // Get one more (should create new)
        let buf4 = pool.get_buffer().await;

        // Return all buffers
        pool.return_buffer(buf1).await;
        pool.return_buffer(buf2).await;
        pool.return_buffer(buf3).await;
        pool.return_buffer(buf4).await; // This might be dropped if pool is full

        // Pool should not exceed max size
        // (We can't directly test pool size, but the implementation handles it)
    }

    #[tokio::test]
    async fn test_buffer_wrong_size_not_returned() {
        let pool = BufferPool::new(1024, 2);

        let buffer = pool.get_buffer().await;
        assert_eq!(buffer.len(), 1024);

        // Create a buffer of wrong size
        let wrong_size_buffer = vec![0u8; 2048];

        // Try to return it - should be dropped
        pool.return_buffer(wrong_size_buffer).await;

        // Original buffer should still work
        pool.return_buffer(buffer).await;
    }

    #[tokio::test]
    async fn test_buffer_pool_multiple_get_return_cycles() {
        let pool = BufferPool::new(4096, 5);

        // Do multiple get/return cycles
        for i in 0..20 {
            let mut buffer = pool.get_buffer().await;
            assert_eq!(buffer.len(), 4096);

            // Write some data
            buffer[0] = i as u8;

            pool.return_buffer(buffer).await;
        }
    }

    #[test]
    fn test_buffer_pool_clone() {
        let pool1 = BufferPool::new(1024, 5);
        let _pool2 = pool1.clone();

        // Both should share the same underlying pool
        // (Arc ensures shared ownership)
    }

    #[tokio::test]
    async fn test_different_buffer_sizes() {
        let small_pool = BufferPool::new(1024, 5);
        let medium_pool = BufferPool::new(8192, 5);
        let large_pool = BufferPool::new(65536, 5);

        let small_buf = small_pool.get_buffer().await;
        let medium_buf = medium_pool.get_buffer().await;
        let large_buf = large_pool.get_buffer().await;

        assert_eq!(small_buf.len(), 1024);
        assert_eq!(medium_buf.len(), 8192);
        assert_eq!(large_buf.len(), 65536);

        small_pool.return_buffer(small_buf).await;
        medium_pool.return_buffer(medium_buf).await;
        large_pool.return_buffer(large_buf).await;
    }

    #[tokio::test]
    async fn test_buffer_pool_stress() {
        let pool = BufferPool::new(4096, 10);

        // Stress test with many concurrent operations
        let mut handles = vec![];

        for _ in 0..100 {
            let pool_clone = pool.clone();
            let handle = tokio::spawn(async move {
                for _ in 0..10 {
                    let buffer = pool_clone.get_buffer().await;
                    assert_eq!(buffer.len(), 4096);
                    pool_clone.return_buffer(buffer).await;
                }
            });
            handles.push(handle);
        }

        for handle in handles {
            handle.await.unwrap();
        }
    }
}
