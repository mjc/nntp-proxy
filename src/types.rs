//! Core types for request tracking and identification
//!
//! This module provides unique identifiers used throughout the proxy.

pub mod config;
pub mod metrics;
pub mod metrics_recording;
pub mod pool;
pub mod protocol;
pub mod tui;
pub mod validated;

pub use config::{
    BackendReadTimeout, BufferSize, CacheCapacity, CommandExecutionTimeout, ConnectionTimeout,
    HealthCheckTimeout, MaxConnections, MaxErrors, Port, ThreadCount, WindowSize, duration_serde,
    option_duration_serde,
};
pub use metrics::{
    ArticleBytesTotal, BackendToClientBytes, BytesPerSecondRate, BytesReceived, BytesSent,
    BytesTransferred, ClientBytes, ClientToBackendBytes, TimingMeasurementCount, TotalConnections,
    TransferMetrics,
};
pub use metrics_recording::{
    DirectionalBytes, MetricsBytes, Recorded, RecordingState, TransferDirection, Unrecorded,
};
pub use pool::{
    AvailableConnections, CreatedConnections, InUseConnections, MaxPoolSize, PoolUtilization,
};
pub use protocol::MessageId;
pub use validated::{ConfigPath, HostName, Password, ServerName, Username, ValidationError};

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

    // ClientId tests
    #[test]
    fn test_client_id_unique() {
        let id1 = ClientId::new();
        let id2 = ClientId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_client_id_default() {
        let id1 = ClientId::default();
        let id2 = ClientId::default();
        assert_ne!(id1, id2); // Each default() creates unique ID
    }

    #[test]
    fn test_client_id_as_uuid() {
        let id = ClientId::new();
        let uuid = id.as_uuid();
        assert_eq!(uuid.get_version(), Some(uuid::Version::Random));
    }

    #[test]
    fn test_client_id_display() {
        let id = ClientId::new();
        let display = format!("{}", id);
        assert!(!display.is_empty());
        // UUID format: 8-4-4-4-12 hex characters
        assert_eq!(display.len(), 36);
        assert_eq!(display.chars().filter(|&c| c == '-').count(), 4);
    }

    #[test]
    fn test_client_id_debug() {
        let id = ClientId::new();
        let debug = format!("{:?}", id);
        assert!(debug.contains("ClientId"));
    }

    #[test]
    fn test_client_id_clone() {
        let id1 = ClientId::new();
        let id2 = id1;
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_client_id_equality() {
        let id1 = ClientId::new();
        let id2 = id1;
        let id3 = ClientId::new();

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_client_id_hash() {
        use std::collections::HashSet;

        let id1 = ClientId::new();
        let id2 = id1;
        let id3 = ClientId::new();

        let mut set = HashSet::new();
        set.insert(id1);
        set.insert(id2); // Duplicate, should not increase size
        set.insert(id3);

        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_client_id_ordering() {
        let id1 = ClientId::new();
        let id2 = ClientId::new();

        // ClientIds can be equal or different - both are valid
        let _ = (id1, id2);

        let id3 = id1;
        assert!(id1 == id3);
    }

    // BackendId tests
    #[test]
    fn test_backend_id() {
        let id1 = BackendId::from_index(0);
        let id2 = BackendId::from_index(1);
        assert_ne!(id1, id2);
        assert_eq!(id1.as_index(), 0);
        assert_eq!(id2.as_index(), 1);
    }

    #[test]
    fn test_backend_id_from_usize() {
        let id: BackendId = 42.into();
        assert_eq!(id.as_index(), 42);
    }

    #[test]
    fn test_backend_id_const_fn() {
        // This tests that from_index is const (compile-time constant)
        const ID: BackendId = BackendId::from_index(10);
        assert_eq!(ID.as_index(), 10);
    }

    #[test]
    fn test_backend_id_display() {
        let id = BackendId::from_index(5);
        assert_eq!(format!("{}", id), "Backend(5)");
    }

    #[test]
    fn test_backend_id_debug() {
        let id = BackendId::from_index(7);
        let debug = format!("{:?}", id);
        assert!(debug.contains("BackendId"));
        assert!(debug.contains("7"));
    }

    #[test]
    fn test_backend_id_clone() {
        let id1 = BackendId::from_index(15);
        let id2 = id1;
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_backend_id_equality() {
        let id1 = BackendId::from_index(10);
        let id2 = BackendId::from_index(10);
        let id3 = BackendId::from_index(20);

        assert_eq!(id1, id2);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_backend_id_hash() {
        use std::collections::HashSet;

        let id1 = BackendId::from_index(1);
        let id2 = BackendId::from_index(1);
        let id3 = BackendId::from_index(2);

        let mut set = HashSet::new();
        set.insert(id1);
        set.insert(id2); // Duplicate
        set.insert(id3);

        assert_eq!(set.len(), 2);
    }

    #[test]
    fn test_backend_id_ordering() {
        let id1 = BackendId::from_index(1);
        let id2 = BackendId::from_index(2);
        let id3 = BackendId::from_index(3);

        assert!(id1 < id2);
        assert!(id2 < id3);
        assert!(id1 < id3);
        assert!(id2 > id1);
    }

    #[test]
    fn test_backend_id_zero() {
        let id = BackendId::from_index(0);
        assert_eq!(id.as_index(), 0);
    }

    #[test]
    fn test_backend_id_large_index() {
        let id = BackendId::from_index(usize::MAX);
        assert_eq!(id.as_index(), usize::MAX);
    }

    #[test]
    fn test_display() {
        let client_id = ClientId::new();
        let backend_id = BackendId::from_index(5);

        assert!(!format!("{}", client_id).is_empty());
        assert_eq!(format!("{}", backend_id), "Backend(5)");
    }
}
