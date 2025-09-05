//! Connection and buffer pooling modules
//! 
//! This module provides connection management and buffer pooling for the NNTP proxy.
//! Simplified to use direct connections without complex pooling.

pub mod buffer;
pub mod simple_connection;

pub use buffer::BufferPool;
pub use simple_connection::ConnectionManager;
