//! Core types for request tracking and identification
//!
//! This module provides unique identifiers and type definitions
//! used throughout the proxy for tracking requests and clients.

use uuid::Uuid;

/// Unique identifier for client connections
/// Used to track which client made which request
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ClientId(Uuid);

#[allow(dead_code)]
impl ClientId {
    /// Generate a new unique client ID
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Get the underlying UUID
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

/// Unique identifier for a request
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Hash, Eq, PartialEq)]
pub struct RequestId(Uuid);

#[allow(dead_code)]
impl RequestId {
    /// Generate a new unique request ID
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Get the underlying UUID
    pub fn as_uuid(&self) -> &Uuid {
        &self.0
    }
}

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Identifier for backend servers
/// Used to track which backend is handling which request
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BackendId(usize);

#[allow(dead_code)]
impl BackendId {
    /// Create a backend ID from an index
    pub fn from_index(index: usize) -> Self {
        Self(index)
    }

    /// Get the underlying index
    pub fn index(&self) -> usize {
        self.0
    }

    /// Get the underlying index
    pub fn as_index(&self) -> usize {
        self.0
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
    fn test_request_id_unique() {
        let id1 = RequestId::new();
        let id2 = RequestId::new();
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
        let request_id = RequestId::new();
        let backend_id = BackendId::from_index(5);

        assert!(!format!("{}", client_id).is_empty());
        assert!(!format!("{}", request_id).is_empty());
        assert_eq!(format!("{}", backend_id), "Backend(5)");
    }
}
