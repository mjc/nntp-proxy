//! Per-backend request queue for pipeline multiplexing
//!
//! Provides a lock-free queue that allows multiple client sessions to enqueue
//! commands destined for a single backend. A pipeline worker dequeues batches
//! and executes them using write-write-read-read pipelining.
//!
//! # Backpressure
//!
//! The queue has a configurable max depth. When full, `try_enqueue` returns
//! `QueueFull` immediately rather than blocking the client session.

use crossbeam::queue::SegQueue;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::{Notify, oneshot};

use crate::types::BackendId;

/// Response sent back to a client session from the pipeline worker
#[derive(Debug)]
pub enum PipelineResponse {
    /// Command executed successfully; `data` is the complete response bytes
    Success {
        /// Complete response data (status line + multiline body if applicable)
        data: Vec<u8>,
        /// Parsed status code from the response
        status_code: u16,
        /// Which backend handled this request
        backend_id: BackendId,
    },
    /// Command failed (connection error, queue overflow, etc.)
    Error(String),
}

/// A request queued for pipeline execution on a backend
pub struct QueuedRequest {
    /// The full NNTP command string (including \r\n)
    pub command: String,
    /// Channel to send the response back to the waiting client session
    pub response_tx: oneshot::Sender<PipelineResponse>,
}

/// Errors returned when enqueuing fails
#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    /// Queue is at capacity; client should get a 503-like response
    #[error("pipeline queue full ({depth}/{max_depth})")]
    QueueFull { depth: usize, max_depth: usize },
}

/// Lock-free per-backend request queue with async notification
#[derive(Debug)]
pub struct BackendQueue {
    queue: SegQueue<QueuedRequest>,
    notify: Notify,
    depth: AtomicUsize,
    max_depth: usize,
}

impl BackendQueue {
    /// Create a new queue with the given maximum depth
    pub fn new(max_depth: usize) -> Self {
        Self {
            queue: SegQueue::new(),
            notify: Notify::new(),
            depth: AtomicUsize::new(0),
            max_depth,
        }
    }

    /// Try to enqueue a request. Returns `QueueFull` if at capacity.
    ///
    /// Uses compare-exchange loop to prevent TOCTOU race where multiple threads
    /// could both pass the depth check and exceed max_depth.
    pub fn try_enqueue(&self, request: QueuedRequest) -> Result<(), QueueError> {
        let mut current = self.depth.load(Ordering::Acquire);
        loop {
            if current >= self.max_depth {
                return Err(QueueError::QueueFull {
                    depth: current,
                    max_depth: self.max_depth,
                });
            }
            match self.depth.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
        self.queue.push(request);
        self.notify.notify_one();
        Ok(())
    }

    /// Dequeue up to `max_batch` requests. Waits if the queue is empty.
    ///
    /// Returns at least 1 request (blocks until one is available).
    /// Greedily takes up to `max_batch` without waiting for more.
    pub async fn dequeue_batch(&self, max_batch: usize) -> Vec<QueuedRequest> {
        // Wait for at least one item
        loop {
            if let Some(first) = self.queue.pop() {
                self.depth.fetch_sub(1, Ordering::AcqRel);
                let mut batch = Vec::with_capacity(max_batch);
                batch.push(first);

                // Greedily drain up to max_batch - 1 more
                while batch.len() < max_batch {
                    match self.queue.pop() {
                        Some(req) => {
                            self.depth.fetch_sub(1, Ordering::AcqRel);
                            batch.push(req);
                        }
                        None => break,
                    }
                }

                return batch;
            }

            // Queue is empty, wait for notification
            self.notify.notified().await;
        }
    }

    /// Current queue depth (approximate, for metrics)
    #[inline]
    pub fn len(&self) -> usize {
        self.depth.load(Ordering::Relaxed)
    }

    /// Whether the queue is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Maximum queue depth
    #[inline]
    pub fn max_depth(&self) -> usize {
        self.max_depth
    }
}

// QueuedRequest contains a oneshot::Sender which isn't Debug
impl std::fmt::Debug for QueuedRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueuedRequest")
            .field("command", &self.command)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_queue_enqueue_dequeue() {
        let queue = BackendQueue::new(10);
        let (tx, _rx) = oneshot::channel();
        queue
            .try_enqueue(QueuedRequest {
                command: "ARTICLE <test@example.com>\r\n".to_string(),
                response_tx: tx,
            })
            .unwrap();
        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn test_queue_full() {
        let queue = BackendQueue::new(2);
        for i in 0..2 {
            let (tx, _rx) = oneshot::channel();
            queue
                .try_enqueue(QueuedRequest {
                    command: format!("ARTICLE <test{}@example.com>\r\n", i),
                    response_tx: tx,
                })
                .unwrap();
        }
        let (tx, _rx) = oneshot::channel();
        let result = queue.try_enqueue(QueuedRequest {
            command: "ARTICLE <overflow@example.com>\r\n".to_string(),
            response_tx: tx,
        });
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            QueueError::QueueFull {
                depth: 2,
                max_depth: 2
            }
        ));
    }

    #[tokio::test]
    async fn test_dequeue_batch() {
        let queue = Arc::new(BackendQueue::new(100));
        for i in 0..5 {
            let (tx, _rx) = oneshot::channel();
            queue
                .try_enqueue(QueuedRequest {
                    command: format!("CMD {}\r\n", i),
                    response_tx: tx,
                })
                .unwrap();
        }

        let batch = queue.dequeue_batch(3).await;
        assert_eq!(batch.len(), 3);
        assert_eq!(queue.len(), 2);

        let batch = queue.dequeue_batch(10).await;
        assert_eq!(batch.len(), 2);
        assert_eq!(queue.len(), 0);
    }

    #[tokio::test]
    async fn test_dequeue_waits_for_item() {
        let queue = Arc::new(BackendQueue::new(100));
        let queue_clone = queue.clone();

        // Spawn dequeue that will wait
        let handle = tokio::spawn(async move { queue_clone.dequeue_batch(5).await });

        // Give it a moment to start waiting
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Now enqueue
        let (tx, _rx) = oneshot::channel();
        queue
            .try_enqueue(QueuedRequest {
                command: "HELLO\r\n".to_string(),
                response_tx: tx,
            })
            .unwrap();

        let batch = handle.await.unwrap();
        assert_eq!(batch.len(), 1);
    }
}
