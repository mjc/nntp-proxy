//! Core types for request tracking and identification
//!
//! This module provides unique identifiers used throughout the proxy.

pub mod config;
pub mod metrics;
pub mod protocol;
pub mod validated;

pub use config::{
    BufferSize, CacheCapacity, MaxConnections, MaxErrors, Port, ThreadCount, WindowSize,
    duration_serde, option_duration_serde,
};
pub use metrics::{BytesTransferred, TransferMetrics};
pub use protocol::MessageId;
pub use validated::{ConfigPath, HostName, ServerName, ValidationError};

use uuid::Uuid;

/// Unique identifier for client connections
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClientId(Uuid);

impl ClientId {
    /// Generate a new unique client ID
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Get the underlying UUID
    #[must_use]
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for ClientId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ClientId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifier for backend servers
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BackendId(usize);

impl BackendId {
    /// Create a backend ID from an index
    /// Marked const fn to allow compile-time evaluation
    #[must_use]
    #[inline]
    pub const fn from_index(index: usize) -> Self {
        Self(index)
    }

    /// Get the underlying index
    #[must_use]
    #[inline]
    pub fn as_index(&self) -> usize {
        self.0
    }
}

impl From<usize> for BackendId {
    fn from(index: usize) -> Self {
        Self(index)
    }
}

impl std::fmt::Display for BackendId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Backend({})", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_id_unique() {
        let id1 = ClientId::new();
        let id2 = ClientId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_backend_id() {
        let id1 = BackendId::from_index(0);
        let id2 = BackendId::from_index(1);
        assert_ne!(id1, id2);
        assert_eq!(id1.as_index(), 0);
        assert_eq!(id2.as_index(), 1);
    }

    #[test]
    fn test_display() {
        let client_id = ClientId::new();
        let backend_id = BackendId::from_index(5);

        assert!(!format!("{}", client_id).is_empty());
        assert_eq!(format!("{}", backend_id), "Backend(5)");
    }
}
