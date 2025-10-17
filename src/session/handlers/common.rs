//! Common utilities shared across handler modules

/// Threshold for logging detailed transfer info (bytes)
/// Transfers under this size are considered "small" (test connections, etc.)
pub(super) const SMALL_TRANSFER_THRESHOLD: u64 = 500;

/// Extract message-ID from NNTP command if present
#[inline]
pub(super) fn extract_message_id(command: &str) -> Option<&str> {
    let start = command.find('<')?;
    let end = command[start..].find('>')?;
    Some(&command[start..start + end + 1])
}
