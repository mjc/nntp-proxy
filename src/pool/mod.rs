//! Connection pool management for backend NNTP servers
//!
//! Provides efficient connection pooling and buffer management

pub mod buffer;
pub mod connection_trait;
pub mod deadpool_connection;
pub mod prewarming;
pub mod simple_connection;

pub use buffer::BufferPool;
pub use connection_trait::ConnectionProvider;
