//! Request tracking for multiplexed connections

use std::collections::HashMap;
use std::time::Instant;

use crate::types::{ClientId, RequestId};

/// A pending request waiting for a response
#[derive(Debug, Clone, PartialEq)]
pub struct PendingRequest {
    /// The client that made this request
    pub client_id: ClientId,
    /// The command that was sent
    pub command: String,
    /// When the request was made
    pub timestamp: Instant,
}

impl PendingRequest {
    /// Create a new pending request
    pub fn new(client_id: ClientId, command: String) -> Self {
        Self {
            client_id,
            command,
            timestamp: Instant::now(),
        }
    }

    /// Get the age of this request
    #[allow(dead_code)]
    pub fn age(&self) -> std::time::Duration {
        self.timestamp.elapsed()
    }
}

/// Tracks pending requests and their associated clients
#[derive(Debug)]
pub struct RequestTracker {
    pending: HashMap<RequestId, PendingRequest>,
}

impl Default for RequestTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestTracker {
    /// Create a new request tracker
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }

    /// Add a new request to track
    pub fn add_request(&mut self, request_id: RequestId, client_id: ClientId, command: String) {
        let request = PendingRequest::new(client_id, command);
        self.pending.insert(request_id, request);
    }

    /// Get the client ID for a pending request
    pub fn get_client_id(&self, request_id: RequestId) -> Option<ClientId> {
        self.pending.get(&request_id).map(|req| req.client_id)
    }

    /// Get a pending request by ID
    #[allow(dead_code)]
    pub fn get_request(&self, request_id: RequestId) -> Option<&PendingRequest> {
        self.pending.get(&request_id)
    }

    /// Complete a request and remove it from tracking
    pub fn complete_request(&mut self, request_id: RequestId) -> Option<ClientId> {
        self.pending.remove(&request_id).map(|req| req.client_id)
    }

    /// Get the number of pending requests
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Get all pending requests (for debugging/monitoring)
    #[allow(dead_code)]
    pub fn all_pending(&self) -> Vec<(RequestId, &PendingRequest)> {
        self.pending.iter().map(|(id, req)| (*id, req)).collect()
    }

    /// Remove stale requests older than the given duration
    #[allow(dead_code)]
    pub fn cleanup_stale(&mut self, max_age: std::time::Duration) -> usize {
        let before = self.pending.len();
        self.pending.retain(|_, req| req.age() < max_age);
        before - self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_tracker_creation() {
        let tracker = RequestTracker::new();
        assert_eq!(tracker.pending_count(), 0);
    }

    #[test]
    fn test_add_and_get_request() {
        let mut tracker = RequestTracker::new();
        let request_id = RequestId::new();
        let client_id = ClientId::new();

        tracker.add_request(request_id, client_id, "LIST\r\n".to_string());

        assert_eq!(tracker.pending_count(), 1);
        assert_eq!(tracker.get_client_id(request_id), Some(client_id));
    }

    #[test]
    fn test_get_request_details() {
        let mut tracker = RequestTracker::new();
        let request_id = RequestId::new();
        let client_id = ClientId::new();
        let command = "HELP\r\n".to_string();

        tracker.add_request(request_id, client_id, command.clone());

        let request = tracker.get_request(request_id).unwrap();
        assert_eq!(request.client_id, client_id);
        assert_eq!(request.command, command);
    }

    #[test]
    fn test_complete_request() {
        let mut tracker = RequestTracker::new();
        let request_id = RequestId::new();
        let client_id = ClientId::new();

        tracker.add_request(request_id, client_id, "LIST\r\n".to_string());
        assert_eq!(tracker.pending_count(), 1);

        let completed = tracker.complete_request(request_id);
        assert_eq!(completed, Some(client_id));
        assert_eq!(tracker.pending_count(), 0);
    }

    #[test]
    fn test_multiple_requests() {
        let mut tracker = RequestTracker::new();
        let req1 = RequestId::new();
        let req2 = RequestId::new();
        let client1 = ClientId::new();
        let client2 = ClientId::new();

        tracker.add_request(req1, client1, "LIST\r\n".to_string());
        tracker.add_request(req2, client2, "HELP\r\n".to_string());

        assert_eq!(tracker.pending_count(), 2);
        assert_eq!(tracker.get_client_id(req1), Some(client1));
        assert_eq!(tracker.get_client_id(req2), Some(client2));
    }

    #[test]
    fn test_all_pending() {
        let mut tracker = RequestTracker::new();
        let req1 = RequestId::new();
        let req2 = RequestId::new();
        let client1 = ClientId::new();
        let client2 = ClientId::new();

        tracker.add_request(req1, client1, "LIST\r\n".to_string());
        tracker.add_request(req2, client2, "HELP\r\n".to_string());

        let all = tracker.all_pending();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_request_age() {
        let mut tracker = RequestTracker::new();
        let request_id = RequestId::new();
        let client_id = ClientId::new();

        tracker.add_request(request_id, client_id, "LIST\r\n".to_string());

        let request = tracker.get_request(request_id).unwrap();
        let age = request.age();

        // Age should be very small (just created)
        assert!(age < Duration::from_secs(1));
    }

    #[test]
    fn test_cleanup_stale() {
        let mut tracker = RequestTracker::new();
        let request_id = RequestId::new();
        let client_id = ClientId::new();

        tracker.add_request(request_id, client_id, "LIST\r\n".to_string());

        // No requests should be stale immediately
        let removed = tracker.cleanup_stale(Duration::from_secs(60));
        assert_eq!(removed, 0);
        assert_eq!(tracker.pending_count(), 1);

        // All requests should be stale with 0 duration
        let removed = tracker.cleanup_stale(Duration::from_secs(0));
        assert_eq!(removed, 1);
        assert_eq!(tracker.pending_count(), 0);
    }

    #[test]
    fn test_nonexistent_request() {
        let tracker = RequestTracker::new();
        let fake_id = RequestId::new();

        assert_eq!(tracker.get_client_id(fake_id), None);
        assert_eq!(tracker.get_request(fake_id), None);
    }

    #[test]
    fn test_complete_nonexistent() {
        let mut tracker = RequestTracker::new();
        let fake_id = RequestId::new();

        let result = tracker.complete_request(fake_id);
        assert_eq!(result, None);
    }
}
