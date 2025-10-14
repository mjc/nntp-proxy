//! Connection pooling and buffer pooling modules
//!
//! This module provides connection management and buffer pooling for the NNTP proxy.

pub mod buffer;
pub mod connection_guard;
pub mod connection_trait;
pub mod deadpool_connection;
pub mod health_check;
pub mod prewarming;
pub mod provider;

pub use buffer::BufferPool;
pub use connection_guard::{execute_with_guard, is_connection_error, remove_from_pool};
pub use connection_trait::{ConnectionProvider, PoolStatus};
pub use health_check::{HealthCheckError, HealthCheckMetrics};
pub use provider::DeadpoolConnectionProvider;
pub use prewarming::prewarm_pools;
