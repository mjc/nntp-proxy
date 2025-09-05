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
