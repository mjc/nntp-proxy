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

use crate::protocol::RequestContext;
use crate::types::BackendId;

/// Response sent back to a client session from the pipeline worker
#[derive(Debug)]
pub enum PipelineResponse {
    /// Command executed successfully; `data` is the complete response bytes
    Success {
        /// Complete response data (status line + multiline body if applicable)
        data: crate::pool::ChunkedResponse,
        /// Parsed status code from the response
        status_code: crate::protocol::StatusCode,
        /// Which backend handled this request
        backend_id: BackendId,
    },
    /// Command failed (connection error, queue overflow, etc.)
    Error(PipelineError),
}

/// Queue/worker failures reported back to the waiting session without heap allocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineError {
    ConnectionAcquire,
    WriteFailed { index: usize, batch_len: usize },
    FlushFailed,
    ReadFailed,
    ConnectionLost { completed: usize, batch_len: usize },
}

impl std::fmt::Display for PipelineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConnectionAcquire => f.write_str("connection error"),
            Self::WriteFailed { index, batch_len } => {
                write!(f, "write failed at command {index}/{batch_len}")
            }
            Self::FlushFailed => f.write_str("flush failed"),
            Self::ReadFailed => f.write_str("read error"),
            Self::ConnectionLost {
                completed,
                batch_len,
            } => write!(f, "connection lost after response {completed}/{batch_len}"),
        }
    }
}

/// A request queued for pipeline execution on a backend
pub struct QueuedContext {
    /// Typed request context. Owns verb/args, not redundant full wire bytes.
    pub context: RequestContext,
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
    queue: SegQueue<QueuedContext>,
    notify: Notify,
    depth: AtomicUsize,
    max_depth: usize,
}

impl BackendQueue {
    /// Create a new queue with the given maximum depth
    #[must_use]
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
    /// could both pass the depth check and exceed `max_depth`.
    ///
    /// # Atomicity
    ///
    /// The compare-exchange reserves a slot by incrementing depth, then pushes
    /// to the queue. A RAII guard ensures depth is decremented if the push is
    /// not committed (e.g., on panic). This prevents the invariant violation
    /// where depth > actual queue size.
    pub fn try_enqueue(&self, request: QueuedContext) -> Result<(), QueueError> {
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

        // RAII guard ensures depth is decremented if we don't commit
        struct EnqueueGuard<'a> {
            depth: &'a AtomicUsize,
            committed: bool,
        }
        impl Drop for EnqueueGuard<'_> {
            fn drop(&mut self) {
                if !self.committed {
                    self.depth.fetch_sub(1, Ordering::AcqRel);
                }
            }
        }

        let mut guard = EnqueueGuard {
            depth: &self.depth,
            committed: false,
        };

        // Push to queue (SegQueue::push is infallible)
        self.queue.push(request);
        guard.committed = true; // Mark as committed so guard doesn't decrement

        self.notify.notify_one();
        Ok(())
    }

    /// Dequeue up to `max_batch` requests, reusing the provided Vec's allocation.
    ///
    /// Takes ownership of `batch`, clears it, fills with at least 1 request (blocks until
    /// one is available), then greedily takes up to `max_batch` without waiting for more.
    /// Returns the filled Vec so the caller can thread ownership through a loop.
    pub async fn dequeue_batch(
        &self,
        max_batch: usize,
        mut batch: Vec<QueuedContext>,
    ) -> Vec<QueuedContext> {
        batch.clear();

        // Wait for at least one item
        loop {
            // Register interest before the final check to prevent missed notifications
            let notified = self.notify.notified();

            if let Some(first) = self.queue.pop() {
                self.depth.fetch_sub(1, Ordering::AcqRel);
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

            // Queue is empty, await the notification we registered
            notified.await;
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
    pub const fn max_depth(&self) -> usize {
        self.max_depth
    }
}

// QueuedContext contains a oneshot::Sender which isn't Debug
impl std::fmt::Debug for QueuedContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueuedContext")
            .field("kind", &self.context.kind())
            .field("verb", &String::from_utf8_lossy(self.context.verb()))
            .field("args", &String::from_utf8_lossy(self.context.args()))
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
            .try_enqueue(QueuedContext {
                context: RequestContext::from_request_line("ARTICLE <test@example.com>\r\n"),
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
                .try_enqueue(QueuedContext {
                    context: RequestContext::from_request_line(&format!(
                        "ARTICLE <test{i}@example.com>\r\n"
                    )),
                    response_tx: tx,
                })
                .unwrap();
        }
        let (tx, _rx) = oneshot::channel();
        let result = queue.try_enqueue(QueuedContext {
            context: RequestContext::from_request_line("ARTICLE <overflow@example.com>\r\n"),
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
                .try_enqueue(QueuedContext {
                    context: RequestContext::from_request_line(&format!("CMD {i}\r\n")),
                    response_tx: tx,
                })
                .unwrap();
        }

        let batch = queue.dequeue_batch(3, Vec::new()).await;
        assert_eq!(batch.len(), 3);
        assert_eq!(queue.len(), 2);

        let batch = queue.dequeue_batch(10, batch).await;
        assert_eq!(batch.len(), 2);
        assert_eq!(queue.len(), 0);
    }

    #[tokio::test]
    async fn test_dequeue_waits_for_item() {
        let queue = Arc::new(BackendQueue::new(100));
        let queue_clone = queue.clone();

        // Spawn dequeue that will wait
        let handle = tokio::spawn(async move { queue_clone.dequeue_batch(5, Vec::new()).await });

        // Give it a moment to start waiting
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Now enqueue
        let (tx, _rx) = oneshot::channel();
        queue
            .try_enqueue(QueuedContext {
                context: RequestContext::from_request_line("HELLO\r\n"),
                response_tx: tx,
            })
            .unwrap();

        let batch = handle.await.unwrap();
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn test_pipeline_error_display_messages() {
        let cases = [
            (PipelineError::ConnectionAcquire, "connection error"),
            (
                PipelineError::WriteFailed {
                    index: 2,
                    batch_len: 5,
                },
                "write failed at command 2/5",
            ),
            (PipelineError::FlushFailed, "flush failed"),
            (PipelineError::ReadFailed, "read error"),
            (
                PipelineError::ConnectionLost {
                    completed: 3,
                    batch_len: 5,
                },
                "connection lost after response 3/5",
            ),
        ];

        cases.into_iter().for_each(|(error, expected)| {
            assert_eq!(error.to_string(), expected);
        });
    }

    #[test]
    fn test_queued_context_owns_typed_context() {
        let context = RequestContext::from_request_line("STAT <test@example.com>\r\n");
        assert_eq!(context.verb(), b"STAT");
        assert_eq!(context.args(), b"<test@example.com>");
        assert_eq!(context.wire_len(), 25);
    }
}
