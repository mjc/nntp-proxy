//! Request routing and multiplexing
//!
//! This module handles routing NNTP commands to backend servers and
//! multiplexing multiple client connections over shared backend connections.

mod demux;
mod tracker;

#[allow(unused_imports)]
pub use demux::{ClientResponse, ResponseDemultiplexer};
pub use tracker::RequestTracker;

#[allow(unused_imports)]
use tracker::PendingRequest;

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::pool::DeadpoolConnectionProvider;
use crate::types::{BackendId, ClientId, RequestId};

/// Backend connection information
#[derive(Clone)]
struct BackendInfo {
    /// Backend identifier
    id: BackendId,
    /// Server name for logging
    name: String,
    /// Connection provider for this backend
    provider: DeadpoolConnectionProvider,
    /// Number of pending requests on this backend (for load balancing)
    pending_count: Arc<AtomicUsize>,
    /// Optional username for authentication
    username: Option<String>,
    /// Optional password for authentication
    password: Option<String>,
}

/// Routes requests to backend servers and manages multiplexing
#[allow(dead_code)]
pub struct RequestRouter {
    /// Tracks pending requests waiting for responses
    tracker: Arc<RwLock<RequestTracker>>,
    /// Backend connection providers
    backends: Vec<BackendInfo>,
    /// Current backend index for round-robin selection
    current_backend: AtomicUsize,
}

#[allow(dead_code)]
impl RequestRouter {
    /// Create a new request router
    pub fn new() -> Self {
        Self {
            tracker: Arc::new(RwLock::new(RequestTracker::new())),
            backends: Vec::new(),
            current_backend: AtomicUsize::new(0),
        }
    }

    /// Add a backend server to the router
    pub fn add_backend(
        &mut self,
        backend_id: BackendId,
        name: String,
        provider: DeadpoolConnectionProvider,
        username: Option<String>,
        password: Option<String>,
    ) {
        info!("Added backend {:?} ({})", backend_id, name);
        self.backends.push(BackendInfo {
            id: backend_id,
            name,
            provider,
            pending_count: Arc::new(AtomicUsize::new(0)),
            username,
            password,
        });
    }

    /// Select the best backend using round-robin strategy
    fn select_backend(&self) -> Option<&BackendInfo> {
        if self.backends.is_empty() {
            return None;
        }

        let index = self.current_backend.fetch_add(1, Ordering::Relaxed) % self.backends.len();
        Some(&self.backends[index])
    }

    /// Select the least loaded backend
    #[allow(dead_code)]
    fn select_least_loaded_backend(&self) -> Option<&BackendInfo> {
        if self.backends.is_empty() {
            return None;
        }

        // Find backend with minimum pending requests
        self.backends
            .iter()
            .min_by_key(|backend| backend.pending_count.load(Ordering::Relaxed))
    }

    /// Route a command to an available backend
    /// Returns just the backend ID - no request tracking needed for synchronous multiplexing
    pub fn route_command_sync(
        &self,
        _client_id: ClientId,
        _command: &str,
    ) -> Result<BackendId> {
        // Select a backend using round-robin
        let backend = self
            .select_backend()
            .ok_or_else(|| anyhow::anyhow!("No backends available"))?;

        // Increment pending count for this backend
        backend.pending_count.fetch_add(1, Ordering::Relaxed);

        Ok(backend.id)
    }

    /// Decrement pending count for a backend after synchronous request completion
    pub fn complete_command_sync(&self, backend_id: BackendId) {
        if let Some(backend) = self.backends.iter().find(|b| b.id == backend_id) {
            backend.pending_count.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// Route a command to an available backend (legacy async version with request tracking)
    pub async fn route_command(
        &self,
        client_id: ClientId,
        command: &str,
    ) -> Result<(RequestId, BackendId)> {
        let request_id = RequestId::new();

        // Select a backend
        let backend = self
            .select_backend()
            .ok_or_else(|| anyhow::anyhow!("No backends available"))?;

        // Increment pending count for this backend
        backend.pending_count.fetch_add(1, Ordering::Relaxed);

        // Track the request
        let mut tracker = self.tracker.write().await;
        tracker.add_request(request_id, client_id, command.to_string());

        debug!(
            "Routed command from client {:?} -> backend {:?} ({}), request {:?}: {}",
            client_id,
            backend.id,
            backend.name,
            request_id,
            command.trim()
        );

        Ok((request_id, backend.id))
    }

    /// Complete a request and return the client ID
    /// Also decrements the pending count for the backend
    pub async fn complete_request(
        &self,
        request_id: RequestId,
        backend_id: BackendId,
    ) -> Option<ClientId> {
        // Decrement pending count for this backend
        if let Some(backend) = self.backends.iter().find(|b| b.id == backend_id) {
            backend.pending_count.fetch_sub(1, Ordering::Relaxed);
        }

        let mut tracker = self.tracker.write().await;
        tracker.complete_request(request_id)
    }

    /// Get the client ID for a pending request
    pub async fn get_client_for_request(&self, request_id: RequestId) -> Option<ClientId> {
        let tracker = self.tracker.read().await;
        tracker.get_client_id(request_id)
    }

    /// Get number of pending requests
    pub async fn pending_count(&self) -> usize {
        let tracker = self.tracker.read().await;
        tracker.pending_count()
    }

    /// Get the connection provider for a backend
    pub fn get_backend_provider(
        &self,
        backend_id: BackendId,
    ) -> Option<&DeadpoolConnectionProvider> {
        self.backends
            .iter()
            .find(|b| b.id == backend_id)
            .map(|b| &b.provider)
    }

    /// Get the number of backends
    pub fn backend_count(&self) -> usize {
        self.backends.len()
    }

    /// Get backend load (pending requests) for monitoring
    pub fn backend_load(&self, backend_id: BackendId) -> Option<usize> {
        self.backends
            .iter()
            .find(|b| b.id == backend_id)
            .map(|b| b.pending_count.load(Ordering::Relaxed))
    }

    /// Get backend credentials for authentication
    pub fn get_backend_credentials(&self, backend_id: BackendId) -> Option<(Option<String>, Option<String>)> {
        self.backends
            .iter()
            .find(|b| b.id == backend_id)
            .map(|b| (b.username.clone(), b.password.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_provider() -> DeadpoolConnectionProvider {
        DeadpoolConnectionProvider::new(
            "localhost".to_string(),
            9999,
            "test".to_string(),
            2,
            None,
            None,
        )
    }

    #[tokio::test]
    async fn test_router_creation() {
        let router = RequestRouter::new();
        assert_eq!(router.pending_count().await, 0);
        assert_eq!(router.backend_count(), 0);
    }

    #[tokio::test]
    async fn test_add_backend() {
        let mut router = RequestRouter::new();
        let backend_id = BackendId::from_index(0);
        let provider = create_test_provider();

        router.add_backend(backend_id, "test-backend".to_string(), provider, None, None);

        assert_eq!(router.backend_count(), 1);
    }

    #[tokio::test]
    async fn test_add_multiple_backends() {
        let mut router = RequestRouter::new();

        for i in 0..3 {
            let backend_id = BackendId::from_index(i);
            let provider = create_test_provider();
            router.add_backend(backend_id, format!("backend-{}", i), provider, None, None);
        }

        assert_eq!(router.backend_count(), 3);
    }

    #[tokio::test]
    async fn test_route_command_with_backend() {
        let mut router = RequestRouter::new();
        let backend_id = BackendId::from_index(0);
        let provider = create_test_provider();
        router.add_backend(backend_id, "test".to_string(), provider, None, None);

        let client_id = ClientId::new();
        let (request_id, selected_backend) =
            router.route_command(client_id, "LIST\r\n").await.unwrap();

        assert_eq!(router.pending_count().await, 1);
        assert_eq!(selected_backend, backend_id);

        let found_client = router.get_client_for_request(request_id).await;
        assert_eq!(found_client, Some(client_id));
    }

    #[tokio::test]
    async fn test_complete_request_with_backend() {
        let mut router = RequestRouter::new();
        let backend_id = BackendId::from_index(0);
        let provider = create_test_provider();
        router.add_backend(backend_id, "test".to_string(), provider, None, None);

        let client_id = ClientId::new();
        let (request_id, backend) = router.route_command(client_id, "HELP\r\n").await.unwrap();
        assert_eq!(router.pending_count().await, 1);

        let completed_client = router.complete_request(request_id, backend).await;
        assert_eq!(completed_client, Some(client_id));
        assert_eq!(router.pending_count().await, 0);
    }

    #[tokio::test]
    async fn test_round_robin_selection() {
        let mut router = RequestRouter::new();

        // Add 3 backends
        for i in 0..3 {
            let backend_id = BackendId::from_index(i);
            let provider = create_test_provider();
            router.add_backend(backend_id, format!("backend-{}", i), provider, None, None);
        }

        let client_id = ClientId::new();

        // Route 6 commands and verify round-robin distribution
        let mut backend_counts = [0; 3];
        for _ in 0..6 {
            let (_, backend) = router.route_command(client_id, "LIST\r\n").await.unwrap();
            backend_counts[backend.index()] += 1;
        }

        // Each backend should have been selected exactly twice
        assert_eq!(backend_counts, [2, 2, 2]);
    }

    #[tokio::test]
    async fn test_backend_load_tracking() {
        let mut router = RequestRouter::new();
        let backend_id = BackendId::from_index(0);
        let provider = create_test_provider();
        router.add_backend(backend_id, "test".to_string(), provider, None, None);

        let client1 = ClientId::new();
        let client2 = ClientId::new();

        // Route two commands
        let (req1, backend1) = router.route_command(client1, "LIST\r\n").await.unwrap();
        let (req2, backend2) = router.route_command(client2, "HELP\r\n").await.unwrap();

        // Load should be 2
        assert_eq!(router.backend_load(backend_id), Some(2));

        // Complete one request
        router.complete_request(req1, backend1).await;
        assert_eq!(router.backend_load(backend_id), Some(1));

        // Complete second request
        router.complete_request(req2, backend2).await;
        assert_eq!(router.backend_load(backend_id), Some(0));
    }

    #[tokio::test]
    async fn test_no_backends_fails() {
        let router = RequestRouter::new();
        let client_id = ClientId::new();

        let result = router.route_command(client_id, "LIST\r\n").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_concurrent_routing() {
        let mut router = RequestRouter::new();

        // Add 2 backends
        for i in 0..2 {
            let backend_id = BackendId::from_index(i);
            let provider = create_test_provider();
            router.add_backend(backend_id, format!("backend-{}", i), provider, None, None);
        }

        let router = Arc::new(router);
        let mut handles = Vec::new();

        // Spawn 10 concurrent tasks routing commands
        for i in 0..10 {
            let router_clone = router.clone();
            let handle = tokio::spawn(async move {
                let client_id = ClientId::new();
                router_clone
                    .route_command(client_id, &format!("LIST {}\r\n", i))
                    .await
            });
            handles.push(handle);
        }

        // Wait for all tasks to complete
        for handle in handles {
            let result = handle.await;
            assert!(result.is_ok());
            assert!(result.unwrap().is_ok());
        }

        // Should have 10 pending requests
        assert_eq!(router.pending_count().await, 10);
    }

    #[tokio::test]
    async fn test_concurrent_completion() {
        let mut router = RequestRouter::new();
        let backend_id = BackendId::from_index(0);
        let provider = create_test_provider();
        router.add_backend(backend_id, "test".to_string(), provider, None, None);

        let router = Arc::new(router);

        // Create multiple requests
        let mut request_ids = Vec::new();
        for _ in 0..20 {
            let client_id = ClientId::new();
            let (req_id, _) = router.route_command(client_id, "LIST\r\n").await.unwrap();
            request_ids.push(req_id);
        }

        assert_eq!(router.pending_count().await, 20);

        // Complete them concurrently
        let mut handles = Vec::new();
        for req_id in request_ids {
            let router_clone = router.clone();
            let handle =
                tokio::spawn(
                    async move { router_clone.complete_request(req_id, backend_id).await },
                );
            handles.push(handle);
        }

        for handle in handles {
            handle.await.unwrap();
        }

        // All should be completed
        assert_eq!(router.pending_count().await, 0);
    }

    #[tokio::test]
    async fn test_load_balancing_fairness() {
        let mut router = RequestRouter::new();

        // Add 4 backends
        for i in 0..4 {
            let backend_id = BackendId::from_index(i);
            let provider = create_test_provider();
            router.add_backend(backend_id, format!("backend-{}", i), provider, None, None);
        }

        let client_id = ClientId::new();

        // Route 100 commands
        let mut backend_selections = std::collections::HashMap::new();
        for _ in 0..100 {
            let (_, backend_id) = router.route_command(client_id, "LIST\r\n").await.unwrap();
            *backend_selections.entry(backend_id.index()).or_insert(0) += 1;
        }

        // Each backend should get approximately 25 requests (round-robin)
        for i in 0..4 {
            let count = backend_selections.get(&i).unwrap_or(&0);
            assert_eq!(
                *count, 25,
                "Backend {} should have 25 requests, got {}",
                i, count
            );
        }
    }

    #[tokio::test]
    async fn test_get_backend_provider() {
        let mut router = RequestRouter::new();
        let backend_id = BackendId::from_index(0);
        let provider = create_test_provider();
        router.add_backend(backend_id, "test".to_string(), provider, None, None);

        // Should be able to get provider
        let retrieved = router.get_backend_provider(backend_id);
        assert!(retrieved.is_some());

        // Non-existent backend should return None
        let fake_id = BackendId::from_index(999);
        assert!(router.get_backend_provider(fake_id).is_none());
    }

    #[tokio::test]
    async fn test_backend_load_nonexistent() {
        let router = RequestRouter::new();
        let fake_id = BackendId::from_index(999);

        assert_eq!(router.backend_load(fake_id), None);
    }

    #[tokio::test]
    async fn test_stress_many_requests() {
        let mut router = RequestRouter::new();

        // Add 3 backends
        for i in 0..3 {
            let backend_id = BackendId::from_index(i);
            let provider = create_test_provider();
            router.add_backend(backend_id, format!("backend-{}", i), provider, None, None);
        }

        let router = Arc::new(router);

        // Create 1000 requests
        for _ in 0..1000 {
            let client_id = ClientId::new();
            router.route_command(client_id, "LIST\r\n").await.unwrap();
        }

        assert_eq!(router.pending_count().await, 1000);

        // Load should be distributed
        let total_load: usize = (0..3)
            .map(|i| router.backend_load(BackendId::from_index(i)).unwrap_or(0))
            .sum();
        assert_eq!(total_load, 1000);
    }

    #[tokio::test]
    async fn test_request_client_mapping() {
        let mut router = RequestRouter::new();
        let backend_id = BackendId::from_index(0);
        let provider = create_test_provider();
        router.add_backend(backend_id, "test".to_string(), provider, None, None);

        let client1 = ClientId::new();
        let client2 = ClientId::new();
        let client3 = ClientId::new();

        // Route commands from different clients
        let (req1, _) = router.route_command(client1, "LIST\r\n").await.unwrap();
        let (req2, _) = router.route_command(client2, "HELP\r\n").await.unwrap();
        let (req3, _) = router.route_command(client3, "DATE\r\n").await.unwrap();

        // Verify correct client mapping
        assert_eq!(router.get_client_for_request(req1).await, Some(client1));
        assert_eq!(router.get_client_for_request(req2).await, Some(client2));
        assert_eq!(router.get_client_for_request(req3).await, Some(client3));

        // Non-existent request should return None
        let fake_req = RequestId::new();
        assert_eq!(router.get_client_for_request(fake_req).await, None);
    }

    #[tokio::test]
    async fn test_backend_count() {
        let mut router = RequestRouter::new();
        assert_eq!(router.backend_count(), 0);

        for i in 0..5 {
            let backend_id = BackendId::from_index(i);
            let provider = create_test_provider();
            router.add_backend(backend_id, format!("backend-{}", i), provider, None, None);
        }

        assert_eq!(router.backend_count(), 5);
    }

    #[tokio::test]
    async fn test_different_command_types() {
        let mut router = RequestRouter::new();
        let backend_id = BackendId::from_index(0);
        let provider = create_test_provider();
        router.add_backend(backend_id, "test".to_string(), provider, None, None);

        let client_id = ClientId::new();

        // Route various command types
        let commands = vec![
            "LIST\r\n",
            "HELP\r\n",
            "DATE\r\n",
            "CAPABILITIES\r\n",
            "ARTICLE <test@example.com>\r\n",
            "QUIT\r\n",
        ];

        for cmd in commands {
            let result = router.route_command(client_id, cmd).await;
            assert!(result.is_ok(), "Failed to route command: {}", cmd);
        }

        assert_eq!(router.pending_count().await, 6);
    }

    #[tokio::test]
    async fn test_single_client_multiple_articles_distributed() {
        let mut router = RequestRouter::new();

        // Add 3 backends
        for i in 0..3 {
            let backend_id = BackendId::from_index(i);
            let provider = create_test_provider();
            router.add_backend(backend_id, format!("backend-{}", i), provider, None, None);
        }

        // Single client requests 10 articles
        let client_id = ClientId::new();
        let mut backend_distribution = vec![0, 0, 0];

        for i in 0..10 {
            let (_, backend_id) = router
                .route_command(client_id, &format!("ARTICLE <msg{}@example.com>\r\n", i))
                .await
                .unwrap();
            backend_distribution[backend_id.index()] += 1;
        }

        // Verify round-robin distribution: each backend should get 3-4 requests
        // (10 requests / 3 backends = 3.33 per backend)
        assert_eq!(
            backend_distribution[0], 4,
            "Backend 0 should get 4 requests"
        );
        assert_eq!(
            backend_distribution[1], 3,
            "Backend 1 should get 3 requests"
        );
        assert_eq!(
            backend_distribution[2], 3,
            "Backend 2 should get 3 requests"
        );

        // Verify all commands from same client but distributed across backends
        assert_eq!(router.pending_count().await, 10);

        // Each backend should have its share of load
        assert_eq!(router.backend_load(BackendId::from_index(0)), Some(4));
        assert_eq!(router.backend_load(BackendId::from_index(1)), Some(3));
        assert_eq!(router.backend_load(BackendId::from_index(2)), Some(3));
    }
}
