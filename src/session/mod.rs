//! Session management module
//!
//! Handles client sessions with different routing modes.
//!
//! This module handles the lifecycle of a client connection, including
//! command processing, authentication interception, and data transfer.
//!
//! # Architecture Overview
//!
//! ## Three Operating Modes
//!
//! 1. **Standard (1:1) Mode** - `handle_with_pooled_backend()`
//!    - One client maps to one backend connection for entire session
//!    - Lowest latency, simplest model
//!    - Used when routing_mode = Standard
//!
//! 2. **Per-Command Mode** - `handle_per_command_routing()`
//!    - Each command is independently routed to potentially different backends
//!    - Enables load balancing across multiple backend servers
//!    - Rejects stateful commands (MODE READER, etc.)
//!    - Used when routing_mode = PerCommand
//!
//! 3. **Hybrid Mode** - `handle_per_command_routing()` + dynamic switching
//!    - Starts in per-command mode for load balancing
//!    - Automatically switches to stateful mode when stateful command detected
//!    - Best of both worlds: load balancing + stateful command support
//!    - Used when routing_mode = Hybrid
//!
//! ## Key Functions
//!
//! - `execute_command_on_backend()` - **PERFORMANCE CRITICAL HOT PATH**
//!   - Pipelined streaming with double-buffering for 100x+ throughput
//!   - DO NOT refactor to buffer entire responses
//!
//! - `switch_to_stateful_mode()` - Hybrid mode transition
//!   - Acquires dedicated backend connection
//!   - Transitions from per-command to 1:1 mapping
//!
//! - `route_and_execute_command()` - Per-command orchestration
//!   - Routes command to backend
//!   - Handles connection pool management
//!   - Distinguishes backend errors from client disconnects

pub mod backend;
pub(crate) mod common;
pub mod connection;
pub mod error_classification;
pub mod handlers;
pub mod streaming;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;

use crate::auth::AuthHandler;
use crate::config::RoutingMode;
use crate::metrics::MetricsCollector;
use crate::pool::BufferPool;
use crate::router::BackendSelector;
use crate::types::ClientId;

/// Session mode for hybrid routing
///
/// In hybrid mode, sessions can dynamically transition between per-command
/// and stateful modes. This allows load balancing for stateless commands
/// while supporting stateful commands by switching to dedicated connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionMode {
    /// Per-command routing mode - each command can use a different backend
    ///
    /// Benefits:
    /// - Load balancing across multiple backend servers
    /// - Better resource utilization
    /// - Fault tolerance (can route around failed backends)
    ///
    /// Limitations:
    /// - Cannot support stateful commands (MODE READER, GROUP, etc.)
    /// - Slightly higher latency (connection pool overhead)
    PerCommand,

    /// Stateful mode - using a dedicated backend connection
    ///
    /// Benefits:
    /// - Lowest latency (no pool overhead)
    /// - Supports stateful commands
    /// - Simple 1:1 client-to-backend mapping
    ///
    /// Limitations:
    /// - No load balancing (one backend per client)
    /// - Less efficient resource usage
    Stateful,
}

/// Represents an active client session
pub struct ClientSession {
    client_addr: SocketAddr,
    buffer_pool: BufferPool,
    /// Unique identifier for this client
    client_id: ClientId,
    /// Optional router for per-command routing mode
    router: Option<Arc<BackendSelector>>,
    /// Current session mode (for hybrid routing)
    mode: SessionMode,
    /// Routing mode configuration (Standard, PerCommand, or Hybrid)
    routing_mode: RoutingMode,
    /// Authentication handler
    auth_handler: Arc<AuthHandler>,
    /// Whether client has authenticated (starts false, set true after successful auth)
    authenticated: AtomicBool,
    /// Authenticated username (write-once, lock-free reads with cheap clones)
    username: Arc<OnceLock<Arc<str>>>,
    /// Metrics collector for session statistics
    metrics: Option<crate::metrics::MetricsCollector>,

    /// Connection statistics aggregator for logging connection creation
    connection_stats: Option<crate::metrics::ConnectionStatsAggregator>,

    /// Optional article cache for ARTICLE responses
    cache: Option<Arc<crate::cache::ArticleCache>>,
}

/// Builder for constructing `ClientSession` instances
///
/// Provides a fluent API for creating client sessions with different routing modes.
///
/// # Examples
///
/// ```
/// use std::net::SocketAddr;
/// use std::sync::Arc;
/// use nntp_proxy::session::ClientSession;
/// use nntp_proxy::pool::BufferPool;
/// use nntp_proxy::router::BackendSelector;
/// use nntp_proxy::config::RoutingMode;
/// use nntp_proxy::types::BufferSize;
/// use nntp_proxy::auth::AuthHandler;
///
/// let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
/// let buffer_pool = BufferPool::new(BufferSize::DEFAULT, 10);
/// let auth_handler = Arc::new(AuthHandler::new(None, None).unwrap());
///
/// // Standard 1:1 routing mode
/// let session = ClientSession::builder(addr, buffer_pool.clone(), auth_handler.clone())
///     .build();
///
/// // Per-command routing mode
/// let router = Arc::new(BackendSelector::new());
/// let session = ClientSession::builder(addr, buffer_pool.clone(), auth_handler)
///     .with_router(router)
///     .with_routing_mode(RoutingMode::PerCommand)
///     .build();
/// ```
pub struct ClientSessionBuilder {
    client_addr: SocketAddr,
    buffer_pool: BufferPool,
    router: Option<Arc<BackendSelector>>,
    routing_mode: RoutingMode,
    auth_handler: Arc<AuthHandler>,
    metrics: Option<MetricsCollector>,
    connection_stats: Option<crate::metrics::ConnectionStatsAggregator>,
    cache: Option<Arc<crate::cache::ArticleCache>>,
}

impl ClientSessionBuilder {
    /// Configure the session to use per-command routing with a backend router
    ///
    /// When a router is provided, the session will route each command independently
    /// to potentially different backend servers.
    #[must_use]
    pub fn with_router(mut self, router: Arc<BackendSelector>) -> Self {
        self.router = Some(router);
        self
    }

    /// Set the routing mode for this session
    ///
    /// # Arguments
    /// * `mode` - The routing mode (Standard, PerCommand, or Hybrid)
    ///
    /// Note: If you use `with_router()`, you typically want PerCommand or Hybrid mode.
    #[must_use]
    pub fn with_routing_mode(mut self, mode: RoutingMode) -> Self {
        self.routing_mode = mode;
        self
    }

    /// Set the authentication handler
    #[must_use]
    pub fn with_auth_handler(mut self, auth_handler: Arc<AuthHandler>) -> Self {
        self.auth_handler = auth_handler;
        self
    }

    /// Add metrics collection to this session
    #[must_use]
    pub fn with_metrics(mut self, metrics: MetricsCollector) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Add connection stats aggregation to this session
    #[must_use]
    pub fn with_connection_stats(
        mut self,
        connection_stats: crate::metrics::ConnectionStatsAggregator,
    ) -> Self {
        self.connection_stats = Some(connection_stats);
        self
    }

    /// Add article cache to this session
    #[must_use]
    pub fn with_cache(mut self, cache: Arc<crate::cache::ArticleCache>) -> Self {
        self.cache = Some(cache);
        self
    }

    /// Build the client session
    ///
    /// Creates a new `ClientSession` with a unique client ID and the configured
    /// routing mode.
    #[must_use]
    pub fn build(self) -> ClientSession {
        let (mode, routing_mode) = match (&self.router, self.routing_mode) {
            // If router is provided, start in per-command mode
            (Some(_), RoutingMode::PerCommand | RoutingMode::Hybrid) => {
                (SessionMode::PerCommand, self.routing_mode)
            }
            // If router is provided but mode is Standard, default to PerCommand
            (Some(_), RoutingMode::Standard) => (SessionMode::PerCommand, RoutingMode::PerCommand),
            // No router means Standard mode
            (None, _) => (SessionMode::Stateful, RoutingMode::Standard),
        };

        ClientSession {
            client_addr: self.client_addr,
            buffer_pool: self.buffer_pool,
            client_id: ClientId::new(),
            router: self.router,
            mode,
            routing_mode,
            auth_handler: self.auth_handler,
            authenticated: AtomicBool::new(false),
            username: Arc::new(OnceLock::new()),
            metrics: self.metrics,
            connection_stats: self.connection_stats,
            cache: self.cache,
        }
    }
}

impl ClientSession {
    /// Create a new client session for 1:1 backend mapping
    #[must_use]
    pub fn new(
        client_addr: SocketAddr,
        buffer_pool: BufferPool,
        auth_handler: Arc<AuthHandler>,
    ) -> Self {
        Self {
            client_addr,
            buffer_pool,
            client_id: ClientId::new(),
            router: None,
            mode: SessionMode::Stateful, // 1:1 mode is always stateful
            routing_mode: RoutingMode::Standard,
            auth_handler,
            authenticated: AtomicBool::new(false),
            username: Arc::new(OnceLock::new()),
            metrics: None,
            connection_stats: None,
            cache: None,
        }
    }

    /// Create a new client session with per-command routing
    ///
    /// Each command will be routed to a potentially different backend server
    /// using round-robin load balancing.
    #[must_use]
    pub fn new_with_router(
        client_addr: SocketAddr,
        buffer_pool: BufferPool,
        router: Arc<BackendSelector>,
        routing_mode: RoutingMode,
        auth_handler: Arc<AuthHandler>,
    ) -> Self {
        Self {
            client_addr,
            buffer_pool,
            client_id: ClientId::new(),
            router: Some(router),
            mode: SessionMode::PerCommand, // Starts in per-command mode
            routing_mode,
            auth_handler,
            authenticated: AtomicBool::new(false),
            username: Arc::new(OnceLock::new()),
            metrics: None,
            connection_stats: None,
            cache: None,
        }
    }

    /// Create a builder for constructing a client session
    ///
    /// # Examples
    ///
    /// ```
    /// use std::net::SocketAddr;
    /// use std::sync::Arc;
    /// use nntp_proxy::session::ClientSession;
    /// use nntp_proxy::pool::BufferPool;
    /// use nntp_proxy::types::BufferSize;
    /// use nntp_proxy::auth::AuthHandler;
    ///
    /// let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    /// let buffer_pool = BufferPool::new(BufferSize::DEFAULT, 10);
    /// let auth_handler = Arc::new(AuthHandler::new(None, None).unwrap());
    ///
    /// let session = ClientSession::builder(addr, buffer_pool, auth_handler)
    ///     .build();
    /// ```
    #[must_use]
    pub fn builder(
        client_addr: SocketAddr,
        buffer_pool: BufferPool,
        auth_handler: Arc<AuthHandler>,
    ) -> ClientSessionBuilder {
        ClientSessionBuilder {
            client_addr,
            buffer_pool,
            router: None,
            routing_mode: RoutingMode::Standard,
            auth_handler,
            metrics: None,
            connection_stats: None,
            cache: None,
        }
    }
}

impl ClientSession {
    /// Get the unique client ID
    #[must_use]
    #[inline]
    pub fn client_id(&self) -> ClientId {
        self.client_id
    }

    /// Check if this session is using per-command routing
    #[must_use]
    #[inline]
    pub fn is_per_command_routing(&self) -> bool {
        self.router.is_some()
    }

    /// Get the current session mode
    #[must_use]
    #[inline]
    pub fn mode(&self) -> SessionMode {
        self.mode
    }

    /// Get the authenticated username (if any) - cheap clone via Arc
    #[must_use]
    pub fn username(&self) -> Option<Arc<str>> {
        self.username.get().map(Arc::clone)
    }

    /// Set the authenticated username (write-once)
    pub(crate) fn set_username(&self, username: Option<String>) {
        if let Some(name) = username {
            let _ = self.username.set(name.into());
        }
    }

    /// Get the connection stats aggregator (if enabled)
    #[must_use]
    #[inline]
    pub(crate) fn connection_stats(&self) -> Option<&crate::metrics::ConnectionStatsAggregator> {
        self.connection_stats.as_ref()
    }

    // Metrics helper methods - encapsulate Option checks for cleaner handler code

    #[inline]
    pub(crate) fn record_command(&self, backend_id: crate::types::BackendId) {
        if let Some(ref m) = self.metrics {
            m.record_command(backend_id);
        }
    }

    #[inline]
    pub(crate) fn user_command(&self) {
        if let Some(ref m) = self.metrics {
            m.user_command(self.username().as_deref());
        }
    }

    #[inline]
    pub(crate) fn stateful_session_started(&self) {
        if let Some(ref m) = self.metrics {
            m.stateful_session_started();
        }
    }

    #[inline]
    pub(crate) fn stateful_session_ended(&self) {
        if let Some(ref m) = self.metrics {
            m.stateful_session_ended();
        }
    }

    #[inline]
    pub(crate) fn user_bytes_sent(&self, bytes: u64) {
        if let Some(ref m) = self.metrics {
            m.user_bytes_sent(self.username().as_deref(), bytes);
        }
    }

    #[inline]
    pub(crate) fn user_bytes_received(&self, bytes: u64) {
        if let Some(ref m) = self.metrics {
            m.user_bytes_received(self.username().as_deref(), bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthHandler;
    use crate::types::BufferSize;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::Arc;

    /// Helper to create a default AuthHandler for tests (no auth)
    fn test_auth_handler() -> Arc<AuthHandler> {
        Arc::new(AuthHandler::new(None, None).unwrap())
    }

    #[test]
    fn test_client_session_creation() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 4);
        let session = ClientSession::new(addr, buffer_pool.clone(), test_auth_handler());

        assert_eq!(session.client_addr.port(), 8080);
        assert_eq!(
            session.client_addr.ip(),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
        );
    }

    #[test]
    fn test_client_session_with_different_ports() {
        let buffer_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 4);

        let addr1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let session1 = ClientSession::new(addr1, buffer_pool.clone(), test_auth_handler());

        let addr2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 9090);
        let session2 = ClientSession::new(addr2, buffer_pool.clone(), test_auth_handler());

        assert_ne!(session1.client_addr.port(), session2.client_addr.port());
        assert_eq!(session1.client_addr.port(), 8080);
        assert_eq!(session2.client_addr.port(), 9090);
    }

    #[test]
    fn test_client_session_with_ipv6() {
        let buffer_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 4);
        let addr = SocketAddr::new(IpAddr::V6("::1".parse().unwrap()), 8119);
        let session = ClientSession::new(addr, buffer_pool, test_auth_handler());

        assert_eq!(session.client_addr.port(), 8119);
        assert!(session.client_addr.is_ipv6());
    }

    #[test]
    fn test_buffer_pool_cloning() {
        let buffer_pool = BufferPool::new(BufferSize::new(8192).unwrap(), 10);
        let buffer_pool_clone = buffer_pool.clone();

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 1234);
        let _session1 = ClientSession::new(addr, buffer_pool, test_auth_handler());
        let _session2 = ClientSession::new(addr, buffer_pool_clone, test_auth_handler());
    }

    #[test]
    fn test_session_addr_formatting() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 5555);
        let buffer_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 4);
        let session = ClientSession::new(addr, buffer_pool, test_auth_handler());

        let addr_str = format!("{}", session.client_addr);
        assert!(addr_str.contains("10.0.0.1"));
        assert!(addr_str.contains("5555"));
    }

    #[test]
    fn test_multiple_sessions_same_buffer_pool() {
        let buffer_pool = BufferPool::new(BufferSize::new(4096).unwrap(), 8);
        let sessions: Vec<_> = (0..5)
            .map(|i| {
                let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8000 + i);
                ClientSession::new(addr, buffer_pool.clone(), test_auth_handler())
            })
            .collect();

        assert_eq!(sessions.len(), 5);
        for (i, session) in sessions.iter().enumerate() {
            assert_eq!(session.client_addr.port(), 8000 + i as u16);
        }
    }

    #[test]
    fn test_loopback_address() {
        let buffer_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 4);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8119);
        let session = ClientSession::new(addr, buffer_pool, test_auth_handler());

        assert!(session.client_addr.ip().is_loopback());
    }

    #[test]
    fn test_unspecified_address() {
        let buffer_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 4);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
        let session = ClientSession::new(addr, buffer_pool, test_auth_handler());

        assert!(session.client_addr.ip().is_unspecified());
        assert_eq!(session.client_addr.port(), 0);
    }

    #[test]
    fn test_session_without_router() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 4);
        let session = ClientSession::new(addr, buffer_pool, test_auth_handler());

        assert!(!session.is_per_command_routing());
        assert_eq!(session.client_addr.port(), 8080);
    }

    #[test]
    fn test_session_with_router() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 4);
        let router = Arc::new(BackendSelector::new());
        let session = ClientSession::new_with_router(
            addr,
            buffer_pool,
            router,
            RoutingMode::PerCommand,
            test_auth_handler(),
        );

        assert!(session.is_per_command_routing());
        assert_eq!(session.client_addr.port(), 8080);
    }

    #[test]
    fn test_client_id_uniqueness() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 4);

        let session1 = ClientSession::new(addr, buffer_pool.clone(), test_auth_handler());
        let session2 = ClientSession::new(addr, buffer_pool, test_auth_handler());

        assert_ne!(session1.client_id(), session2.client_id());
    }

    #[test]
    fn test_session_mode_enum() {
        assert_eq!(SessionMode::PerCommand, SessionMode::PerCommand);
        assert_eq!(SessionMode::Stateful, SessionMode::Stateful);
        assert_ne!(SessionMode::PerCommand, SessionMode::Stateful);

        let per_command = format!("{:?}", SessionMode::PerCommand);
        let stateful = format!("{:?}", SessionMode::Stateful);
        assert!(per_command.contains("PerCommand"));
        assert!(stateful.contains("Stateful"));
    }

    #[test]
    fn test_hybrid_session_creation() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 4);
        let router = Arc::new(BackendSelector::new());

        let session = ClientSession::new_with_router(
            addr,
            buffer_pool,
            router,
            RoutingMode::Hybrid,
            test_auth_handler(),
        );

        assert!(session.is_per_command_routing());
        assert_eq!(session.routing_mode, RoutingMode::Hybrid);
        assert_eq!(session.mode, SessionMode::PerCommand);
    }

    #[test]
    fn test_routing_mode_configurations() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 4);
        let router = Arc::new(BackendSelector::new());

        // Standard mode
        let session = ClientSession::new_with_router(
            addr,
            buffer_pool.clone(),
            router.clone(),
            RoutingMode::Standard,
            test_auth_handler(),
        );
        assert!(session.is_per_command_routing());
        assert_eq!(session.routing_mode, RoutingMode::Standard);

        // PerCommand mode
        let session = ClientSession::new_with_router(
            addr,
            buffer_pool.clone(),
            router.clone(),
            RoutingMode::PerCommand,
            test_auth_handler(),
        );
        assert!(session.is_per_command_routing());
        assert_eq!(session.routing_mode, RoutingMode::PerCommand);
        assert_eq!(session.mode, SessionMode::PerCommand);

        // Hybrid mode
        let session = ClientSession::new_with_router(
            addr,
            buffer_pool,
            router,
            RoutingMode::Hybrid,
            test_auth_handler(),
        );
        assert!(session.is_per_command_routing());
        assert_eq!(session.routing_mode, RoutingMode::Hybrid);
        assert_eq!(session.mode, SessionMode::PerCommand);
    }

    #[test]
    fn test_hybrid_mode_initial_state() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 4);
        let router = Arc::new(BackendSelector::new());

        let session = ClientSession::new_with_router(
            addr,
            buffer_pool,
            router,
            RoutingMode::Hybrid,
            test_auth_handler(),
        );

        assert_eq!(session.mode, SessionMode::PerCommand);
        assert_eq!(session.routing_mode, RoutingMode::Hybrid);
        assert!(session.is_per_command_routing());
    }

    #[test]
    fn test_is_per_command_routing_logic() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::new(1024).unwrap(), 4);
        let router = Arc::new(BackendSelector::new());

        // Standard mode has router capability
        let session = ClientSession::new_with_router(
            addr,
            buffer_pool.clone(),
            router.clone(),
            RoutingMode::Standard,
            test_auth_handler(),
        );
        assert!(session.is_per_command_routing());

        // PerCommand mode
        let session = ClientSession::new_with_router(
            addr,
            buffer_pool.clone(),
            router.clone(),
            RoutingMode::PerCommand,
            test_auth_handler(),
        );
        assert!(session.is_per_command_routing());

        // Hybrid mode (initially)
        let session = ClientSession::new_with_router(
            addr,
            buffer_pool.clone(),
            router,
            RoutingMode::Hybrid,
            test_auth_handler(),
        );
        assert!(session.is_per_command_routing());

        // Session without router
        let session = ClientSession::new(addr, buffer_pool, test_auth_handler());
        assert!(!session.is_per_command_routing());
    }
}
