//! Stream abstractions for NNTP connections
//!
//! This module provides:
//! - `ConnectionStream`: Enum supporting TCP, TLS, and compressed connections

mod connection_stream;

pub use connection_stream::{AsyncStream, ConnectionStream};
