//! NNTP protocol constants
//!
//! This module contains protocol-specific buffer sizes and operational constants.
//! For protocol response messages, see `crate::constants::protocol`.

/// NNTP response constant (backend unavailable - not duplicated elsewhere)
pub const NNTP_BACKEND_UNAVAILABLE: &[u8] = b"400 Backend server unavailable\r\n";

/// Buffer configuration constants for protocol operations
pub const PROTOCOL_BUFFER_SIZE: usize = 256 * 1024; // 256KB buffers for high throughput
pub const BUFFER_POOL_SIZE: usize = 32; // Number of buffers in pool
pub const HIGH_THROUGHPUT_BUFFER_SIZE: usize = 256 * 1024; // 256KB for direct allocation

/// Connection pool prewarming configuration constants
pub const PREWARMING_BATCH_SIZE: usize = 5; // Create connections in batches of 5
pub const BATCH_DELAY_MS: u64 = 100; // Wait 100ms between prewarming batches
