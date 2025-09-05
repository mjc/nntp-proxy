//! Connection and buffer pooling modules
//! 
//! This module provides connection management and buffer pooling for the NNTP proxy.
//! Uses simple connection providers that can be easily swapped out.

pub mod buffer;
pub mod simple_connection;

pub use buffer::BufferPool;
pub use simple_connection::SimpleConnectionProvider;
