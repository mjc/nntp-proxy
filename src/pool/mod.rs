//! Connection and buffer pooling modules
//!
//! This module provides connection management and buffer pooling for the NNTP proxy.
//! Uses simple connection providers that can be easily swapped out.

pub mod buffer;
pub mod connection_trait;
pub mod simple_connection;
pub mod deadpool_connection;

pub use buffer::BufferPool;
pub use connection_trait::ConnectionProvider;
pub use simple_connection::SimpleConnectionProvider;
pub use deadpool_connection::DeadpoolConnectionProvider;
