//! Request routing and multiplexing
//!
//! This module handles routing NNTP commands to backend servers and
//! multiplexing multiple client connections over shared backend connections.

mod tracker;

pub use tracker::RequestTracker;

#[allow(unused_imports)]
use tracker::PendingRequest;

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::types::{BackendId, ClientId, RequestId};

/// Routes requests to backend servers and manages multiplexing
#[allow(dead_code)]
pub struct RequestRouter {
    /// Tracks pending requests waiting for responses
    tracker: Arc<RwLock<RequestTracker>>,
    /// Map of backend IDs to their connection info
    backends: HashMap<BackendId, String>,
}

#[allow(dead_code)]
impl RequestRouter {
    /// Create a new request router
    pub fn new() -> Self {
        Self {
            tracker: Arc::new(RwLock::new(RequestTracker::new())),
            backends: HashMap::new(),
        }
    }

    /// Add a backend server to the router
    pub fn add_backend(&mut self, backend_id: BackendId, address: String) {
        info!("Added backend {:?} at {}", backend_id, address);
        self.backends.insert(backend_id, address);
    }

    /// Route a command to an available backend
    pub async fn route_command(
        &self,
        client_id: ClientId,
        command: &str,
    ) -> Result<RequestId> {
        let request_id = RequestId::new();
        
        // Track the request
        let mut tracker = self.tracker.write().await;
        tracker.add_request(request_id, client_id, command.to_string());
        
        debug!(
            "Routed command from client {:?} -> request {:?}: {}",
            client_id,
            request_id,
            command.trim()
        );
        
        Ok(request_id)
    }

    /// Get the client ID for a pending request
    pub async fn get_client_for_request(&self, request_id: RequestId) -> Option<ClientId> {
        let tracker = self.tracker.read().await;
        tracker.get_client_id(request_id)
    }

    /// Complete a request and return the client ID
    pub async fn complete_request(&self, request_id: RequestId) -> Option<ClientId> {
        let mut tracker = self.tracker.write().await;
        tracker.complete_request(request_id)
    }

    /// Get number of pending requests
    pub async fn pending_count(&self) -> usize {
        let tracker = self.tracker.read().await;
        tracker.pending_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_router_creation() {
        let router = RequestRouter::new();
        assert_eq!(router.pending_count().await, 0);
    }

    #[tokio::test]
    async fn test_add_backend() {
        let mut router = RequestRouter::new();
        let backend_id = BackendId::from_index(0);
        router.add_backend(backend_id, "news.example.com:119".to_string());
        
        assert_eq!(router.backends.len(), 1);
    }

    #[tokio::test]
    async fn test_route_command() {
        let router = RequestRouter::new();
        let client_id = ClientId::new();
        
        let request_id = router.route_command(client_id, "LIST\r\n").await.unwrap();
        
        assert_eq!(router.pending_count().await, 1);
        
        let found_client = router.get_client_for_request(request_id).await;
        assert_eq!(found_client, Some(client_id));
    }

    #[tokio::test]
    async fn test_complete_request() {
        let router = RequestRouter::new();
        let client_id = ClientId::new();
        
        let request_id = router.route_command(client_id, "HELP\r\n").await.unwrap();
        assert_eq!(router.pending_count().await, 1);
        
        let completed_client = router.complete_request(request_id).await;
        assert_eq!(completed_client, Some(client_id));
        assert_eq!(router.pending_count().await, 0);
    }

    #[tokio::test]
    async fn test_multiple_requests() {
        let router = RequestRouter::new();
        let client1 = ClientId::new();
        let client2 = ClientId::new();
        
        let req1 = router.route_command(client1, "LIST\r\n").await.unwrap();
        let req2 = router.route_command(client2, "HELP\r\n").await.unwrap();
        
        assert_eq!(router.pending_count().await, 2);
        
        router.complete_request(req1).await;
        assert_eq!(router.pending_count().await, 1);
        
        router.complete_request(req2).await;
        assert_eq!(router.pending_count().await, 0);
    }
}
