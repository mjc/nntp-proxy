//! Connection pooling and buffer pooling modules
//!
//! This module provides connection management and buffer pooling for the NNTP proxy.

pub mod buffer;
pub mod connection_trait;
pub mod deadpool_connection;
pub mod prewarming;

pub use buffer::BufferPool;
pub use connection_trait::ConnectionProvider;
pub use deadpool_connection::DeadpoolConnectionProvider;
pub use prewarming::PoolPrewarmer;
