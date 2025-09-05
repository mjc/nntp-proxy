//! Connection and buffer pooling modules
//! 
//! This module provides connection pooling and buffer management for the NNTP proxy.
//! The pools are designed for high-performance scenarios with lock-free operations.

pub mod buffer;
pub mod connection;

pub use buffer::BufferPool;
pub use connection::{ConnectionPool, PooledConnection};
