//! NNTP protocol constants

/// NNTP response constants
pub const NNTP_PASSWORD_REQUIRED: &[u8] = b"381 Password required\r\n";
pub const NNTP_AUTH_ACCEPTED: &[u8] = b"281 Authentication accepted\r\n";
pub const NNTP_BACKEND_UNAVAILABLE: &[u8] = b"400 Backend server unavailable\r\n";
pub const NNTP_AUTH_FAILED: &[u8] = b"502 Authentication failed\r\n";
pub const NNTP_COMMAND_NOT_SUPPORTED: &[u8] = b"500 Command not supported by this proxy (stateless proxy mode)\r\n";

/// Buffer configuration constants
pub const BUFFER_SIZE: usize = 256 * 1024; // 256KB buffers for high throughput
pub const BUFFER_POOL_SIZE: usize = 32; // Number of buffers in pool
pub const HIGH_THROUGHPUT_BUFFER_SIZE: usize = 256 * 1024; // 256KB for direct allocation

/// Connection pool configuration constants
pub const PREWARMING_BATCH_SIZE: usize = 5; // Create connections in batches of 5
pub const BATCH_DELAY_MS: u64 = 100; // Wait 100ms between prewarming batches

/// TCP socket buffer sizes for high-throughput transfers (prepared for future use)
#[allow(dead_code)]
pub const HIGH_THROUGHPUT_RECV_BUFFER: usize = 16 * 1024 * 1024; // 16MB
#[allow(dead_code)]
pub const HIGH_THROUGHPUT_SEND_BUFFER: usize = 16 * 1024 * 1024; // 16MB
