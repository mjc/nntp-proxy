//! Connection and byte-buffer pooling.
//!
//! [`BufferPool`] owns reusable transport allocations and returns them through
//! [`PooledBuffer`]. Normal article forwarding borrows slices from the current
//! pooled read buffer. Full-response capture is an explicit operation for the
//! standalone client, cache ingestion, and other consumers that need ownership;
//! it is not part of the ordinary pass-through path.
//!
//! [`DeadpoolConnectionProvider`] exposes ready backend connections. A
//! connection is reusable only after its response exchange has completed and
//! all pending bytes have been consumed.

pub mod buffer;
pub mod connection_guard;
pub mod connection_trait;
pub(crate) mod deadpool_connection;
pub mod health_check;
pub mod prewarming;
pub mod provider;

pub(crate) use buffer::AppendOutcome;
pub use buffer::{
    BufferPool, ChunkedResponse, HotPathAllocationMetricsSnapshot, PooledBuffer,
    hot_path_allocation_metrics_snapshot, reset_hot_path_allocation_metrics,
};
pub(crate) use connection_guard::ConnectionGuard;
pub(crate) use connection_guard::salvage_with_health_check;
pub use connection_trait::{ConnectionProvider, PoolStatus};
pub use health_check::{HealthCheckError, HealthCheckMetrics};
pub use prewarming::prewarm_pools;
pub use provider::DeadpoolConnectionProvider;
