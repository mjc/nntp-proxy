//! Session management module
//!
//! Handles client sessions with different routing modes.
//!
//! This module handles the lifecycle of a client connection, including
//! command processing, authentication interception, and data transfer.
//!
//! # Quick Start
//!
//! ## Basic Stateful Session (1:1 mapping)
//!
//! ```no_run
//! use std::net::SocketAddr;
//! use std::sync::Arc;
//! use nntp_proxy::session::ClientSession;
//! use nntp_proxy::pool::BufferPool;
//! use nntp_proxy::types::BufferSize;
//! use nntp_proxy::auth::AuthHandler;
//!
//! # fn example() -> anyhow::Result<()> {
//! let client_addr: SocketAddr = "127.0.0.1:50000".parse()?;
//! let buffer_pool = BufferPool::new(BufferSize::DEFAULT, 10);
//! let auth = Arc::new(AuthHandler::new(None, None)?);
//!
//! // Create a simple 1:1 session (no load balancing)
//! let session = ClientSession::new(client_addr, buffer_pool, auth);
//! assert_eq!(session.mode(), nntp_proxy::session::SessionMode::Stateful);
//! # Ok(())
//! # }
//! ```
//!
//! ## Per-Command Routing (Load Balancing)
//!
//! ```no_run
//! use std::sync::Arc;
//! use nntp_proxy::session::ClientSession;
//! use nntp_proxy::pool::BufferPool;
//! use nntp_proxy::router::BackendSelector;
//! use nntp_proxy::config::RoutingMode;
//! use nntp_proxy::types::BufferSize;
//! use nntp_proxy::auth::AuthHandler;
//!
//! # fn example() -> anyhow::Result<()> {
//! let addr = "127.0.0.1:50000".parse()?;
//! let buffer_pool = BufferPool::new(BufferSize::DEFAULT, 10);
//! let router = Arc::new(BackendSelector::new());
//! let auth = Arc::new(AuthHandler::new(None, None)?);
//!
//! // Each command routed to potentially different backend
//! let session = ClientSession::builder(addr.into(), buffer_pool, auth)
//!     .with_router(router)
//!     .with_routing_mode(RoutingMode::PerCommand)
//!     .build();
//!
//! assert!(session.is_per_command_routing());
//! # Ok(())
//! # }
//! ```
//!
//! ## Hybrid Mode (Best of Both Worlds)
//!
//! ```no_run
//! use std::sync::Arc;
//! use nntp_proxy::session::ClientSession;
//! use nntp_proxy::pool::BufferPool;
//! use nntp_proxy::router::BackendSelector;
//! use nntp_proxy::config::RoutingMode;
//! use nntp_proxy::types::BufferSize;
//! use nntp_proxy::auth::AuthHandler;
//!
//! # fn example() -> anyhow::Result<()> {
//! let addr = "127.0.0.1:50000".parse()?;
//! let buffer_pool = BufferPool::new(BufferSize::DEFAULT, 10);
//! let router = Arc::new(BackendSelector::new());
//! let auth = Arc::new(AuthHandler::new(None, None)?);
//!
//! // Starts in per-command mode, auto-switches to stateful when needed
//! let session = ClientSession::builder(addr.into(), buffer_pool, auth)
//!     .with_router(router)
//!     .with_routing_mode(RoutingMode::Hybrid)
//!     .build();
//!
//! // Initially per-command for load balancing
//! assert_eq!(session.mode(), nntp_proxy::session::SessionMode::PerCommand);
//! // Will switch to Stateful automatically on GROUP, NEXT, LAST, etc.
//! # Ok(())
//! # }
//! ```
//!
//! ## With Metrics and Caching
//!
//! ```no_run
//! use std::sync::Arc;
//! use std::time::Duration;
//! use nntp_proxy::session::ClientSession;
//! use nntp_proxy::pool::BufferPool;
//! use nntp_proxy::router::BackendSelector;
//! use nntp_proxy::config::RoutingMode;
//! use nntp_proxy::types::BufferSize;
//! use nntp_proxy::auth::AuthHandler;
//! use nntp_proxy::metrics::MetricsCollector;
//! use nntp_proxy::cache::ArticleCache;
//!
//! # fn example() -> anyhow::Result<()> {
//! let addr = "127.0.0.1:50000".parse()?;
//! let buffer_pool = BufferPool::new(BufferSize::DEFAULT, 10);
//! let router = Arc::new(BackendSelector::new());
//! let auth = Arc::new(AuthHandler::new(None, None)?);
//! let metrics = MetricsCollector::new(2); // 2 backends
//! let cache = Arc::new(ArticleCache::new(1000, Duration::from_secs(3600)));
//!
//! // Full-featured session with all bells and whistles
//! let session = ClientSession::builder(addr.into(), buffer_pool, auth)
//!     .with_router(router)
//!     .with_routing_mode(RoutingMode::Hybrid)
//!     .with_metrics(metrics)
//!     .with_cache(cache)
//!     .build();
//! # Ok(())
//! # }
//! ```
//!
//! # Architecture Overview
//!
//! ## Three Operating Modes
//!
//! 1. **Stateful (1:1) Mode** - `handle_with_pooled_backend()`
//!    - One client maps to one backend connection for entire session
//!    - Lowest latency, simplest model
//!    - Used when routing_mode = Stateful
//!
//! 2. **Per-Command Mode (Stateless)** - `handle_per_command_routing()`
//!    - Each command is independently routed to potentially different backends
//!    - Enables load balancing across multiple backend servers
//!    - Rejects stateful commands (GROUP, NEXT, LAST, etc.)
//!    - Used when routing_mode = PerCommand
//!
//! 3. **Hybrid Mode** - `handle_per_command_routing()` + dynamic switching
//!    - Starts in per-command mode (stateless) for load balancing
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

pub mod auth_state;
pub mod backend;
pub(crate) mod common;
pub mod connection;
pub mod error_classification;
pub mod handlers;
pub mod metrics_ext;
pub mod mode_state;
pub mod routing;
pub mod streaming;

use std::sync::Arc;

use crate::auth::AuthHandler;
use crate::config::RoutingMode;
use crate::metrics::MetricsCollector;
use crate::pool::BufferPool;
use crate::router::BackendSelector;
use crate::types::{ClientAddress, ClientId};

pub use auth_state::AuthState;
pub use metrics_ext::MetricsRecorder;
pub use mode_state::{ModeState, SessionMode};
pub use routing::RoutingDecision;

// SessionMode is now exported from mode_state module

/// Represents an active client session
pub struct ClientSession {
    client_addr: ClientAddress,
    buffer_pool: BufferPool,
    /// Unique identifier for this client
    client_id: ClientId,
    /// Optional router for per-command routing mode
    router: Option<Arc<BackendSelector>>,
    /// Session mode state (encapsulates mode and routing mode)
    mode_state: ModeState,
    /// Authentication handler
    auth_handler: Arc<AuthHandler>,
    /// Authentication state (encapsulates auth status and username)
    auth_state: AuthState,
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
/// // Stateful 1:1 routing mode
/// let session = ClientSession::builder(addr.into(), buffer_pool.clone(), auth_handler.clone())
///     .build();
///
/// // Per-command routing mode
/// let router = Arc::new(BackendSelector::new());
/// let session = ClientSession::builder(addr.into(), buffer_pool.clone(), auth_handler)
///     .with_router(router)
///     .with_routing_mode(RoutingMode::PerCommand)
///     .build();
/// ```
pub struct ClientSessionBuilder {
    client_addr: ClientAddress,
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
    /// * `mode` - The routing mode (Stateful, PerCommand, or Hybrid)
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
            // Per-command or Hybrid: start in per-command mode (stateless)
            (Some(_), RoutingMode::PerCommand | RoutingMode::Hybrid) => {
                (SessionMode::PerCommand, self.routing_mode)
            }
            // Stateful mode with router: honor the Stateful mode request
            (Some(_), RoutingMode::Stateful) => (SessionMode::Stateful, RoutingMode::Stateful),
            // No router: always Stateful mode
            (None, _) => (SessionMode::Stateful, RoutingMode::Stateful),
        };

        ClientSession {
            client_addr: self.client_addr,
            buffer_pool: self.buffer_pool,
            client_id: ClientId::new(),
            router: self.router,
            mode_state: ModeState::new(mode, routing_mode),
            auth_handler: self.auth_handler,
            auth_state: AuthState::new(),
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
        client_addr: ClientAddress,
        buffer_pool: BufferPool,
        auth_handler: Arc<AuthHandler>,
    ) -> Self {
        Self {
            client_addr,
            buffer_pool,
            client_id: ClientId::new(),
            router: None,
            mode_state: ModeState::new(SessionMode::Stateful, RoutingMode::Stateful),
            auth_handler,
            auth_state: AuthState::new(),
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
        client_addr: ClientAddress,
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
            mode_state: ModeState::new(SessionMode::PerCommand, routing_mode),
            auth_handler,
            auth_state: AuthState::new(),
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
    /// let session = ClientSession::builder(addr.into(), buffer_pool, auth_handler)
    ///     .build();
    /// ```
    #[must_use]
    pub fn builder(
        client_addr: ClientAddress,
        buffer_pool: BufferPool,
        auth_handler: Arc<AuthHandler>,
    ) -> ClientSessionBuilder {
        ClientSessionBuilder {
            client_addr,
            buffer_pool,
            router: None,
            routing_mode: RoutingMode::Stateful,
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
    ///
    /// Returns true if this session has a router available (regardless of current mode).
    /// This is slightly different from checking routing mode - a session can have a router
    /// but be in Stateful mode (e.g., after hybrid mode switches).
    #[must_use]
    #[inline]
    pub fn is_per_command_routing(&self) -> bool {
        self.router.is_some()
    }

    /// Get the current session mode
    #[must_use]
    #[inline]
    pub fn mode(&self) -> SessionMode {
        self.mode_state.mode()
    }

    /// Get the authenticated username (if any) - zero-cost reference
    ///\n    /// Returns the authenticated username as an Arc<str> for cheap cloning.
    /// Returns None if the client has not authenticated yet.
    #[inline]
    #[must_use]
    pub fn username(&self) -> Option<Arc<str>> {
        self.auth_state.username()
    }

    /// Set the authenticated username (write-once)
    ///
    /// This marks the session as authenticated and stores the username.
    /// Called after successful authentication with the backend.
    pub(crate) fn set_username(&self, username: Option<String>) {
        if let Some(name) = username {
            self.auth_state.mark_authenticated(name);
        }
    }

    /// Get the connection stats aggregator (if enabled)
    #[must_use]
    #[inline]
    pub(crate) fn connection_stats(&self) -> Option<&crate::metrics::ConnectionStatsAggregator> {
        self.connection_stats.as_ref()
    }

    // Metrics helper methods - delegate to MetricsRecorder trait

    #[inline]
    pub(crate) fn record_command(&self, backend_id: crate::types::BackendId) {
        self.metrics.record_command(backend_id);
    }

    #[inline]
    pub(crate) fn user_command(&self) {
        self.metrics.user_command(self.username().as_deref());
    }

    #[inline]
    pub(crate) fn stateful_session_started(&self) {
        self.metrics.stateful_session_started();
    }

    #[inline]
    pub(crate) fn stateful_session_ended(&self) {
        self.metrics.stateful_session_ended();
    }

    #[inline]
    pub(crate) fn user_bytes_sent(&self, bytes: u64) {
        self.metrics
            .user_bytes_sent(self.username().as_deref(), bytes);
    }

    #[inline]
    pub(crate) fn user_bytes_received(&self, bytes: u64) {
        self.metrics
            .user_bytes_received(self.username().as_deref(), bytes);
    }

    /// Flush incremental metrics for long-running sessions
    ///
    /// This should be called periodically (e.g., every 100 iterations) to report
    /// byte transfer progress for the TUI without waiting until session end.
    #[inline]
    pub(crate) fn flush_incremental_metrics(
        &self,
        backend_id: crate::types::BackendId,
        client_to_backend: crate::types::ClientToBackendBytes,
        backend_to_client: crate::types::BackendToClientBytes,
        last_reported_c2b: &mut crate::types::ClientToBackendBytes,
        last_reported_b2c: &mut crate::types::BackendToClientBytes,
    ) {
        let delta_c2b = client_to_backend.saturating_sub(*last_reported_c2b);
        let delta_b2c = backend_to_client.saturating_sub(*last_reported_b2c);

        if delta_c2b.as_u64() > 0 {
            self.metrics
                .record_client_to_backend_bytes_for(backend_id, delta_c2b.as_u64());
            self.user_bytes_sent(delta_c2b.as_u64());
        }
        if delta_b2c.as_u64() > 0 {
            self.metrics
                .record_backend_to_client_bytes_for(backend_id, delta_b2c.as_u64());
            self.user_bytes_received(delta_b2c.as_u64());
        }

        *last_reported_c2b = client_to_backend;
        *last_reported_b2c = backend_to_client;
    }

    /// Check if already authenticated (cached for performance)
    ///
    /// # Arguments
    /// * `skip_auth_check` - If true, bypasses the authentication check
    ///
    /// # Returns
    /// Returns true if authenticated or if skip_auth_check is true
    #[inline]
    pub(crate) fn is_authenticated_cached(&self, skip_auth_check: bool) -> bool {
        self.auth_state.is_authenticated_or_skipped(skip_auth_check)
    }

    /// Report final session metrics before ending
    #[inline]
    pub(crate) fn report_final_metrics(
        &self,
        backend_id: crate::types::BackendId,
        client_to_backend: impl Into<u64>,
        backend_to_client: impl Into<u64>,
        last_reported_c2b: u64,
        last_reported_b2c: u64,
    ) {
        let current_c2b = client_to_backend.into();
        let current_b2c = backend_to_client.into();

        let delta_c2b = current_c2b.saturating_sub(last_reported_c2b);
        let delta_b2c = current_b2c.saturating_sub(last_reported_b2c);

        if delta_c2b > 0 {
            self.metrics
                .record_client_to_backend_bytes_for(backend_id, delta_c2b);
            self.user_bytes_sent(delta_c2b);
        }
        if delta_b2c > 0 {
            self.metrics
                .record_backend_to_client_bytes_for(backend_id, delta_b2c);
            self.user_bytes_received(delta_b2c);
        }

        self.metrics
            .user_connection_closed(self.username().as_deref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::AuthHandler;
    use crate::types::BufferSize;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    /// Helper to create a default AuthHandler for tests (no auth)
    fn test_auth_handler() -> Arc<AuthHandler> {
        Arc::new(AuthHandler::new(None, None).unwrap())
    }

    #[test]
    fn test_client_session_creation() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let session = ClientSession::new(addr.into(), buffer_pool.clone(), test_auth_handler());

        assert_eq!(session.client_addr.port(), 8080);
        assert_eq!(
            session.client_addr.ip(),
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))
        );
    }

    #[test]
    fn test_client_session_with_different_ports() {
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);

        let addr1 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let session1 = ClientSession::new(addr1.into(), buffer_pool.clone(), test_auth_handler());

        let addr2 = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 9090);
        let session2 = ClientSession::new(addr2.into(), buffer_pool.clone(), test_auth_handler());

        assert_ne!(session1.client_addr.port(), session2.client_addr.port());
        assert_eq!(session1.client_addr.port(), 8080);
        assert_eq!(session2.client_addr.port(), 9090);
    }

    #[test]
    fn test_client_session_with_ipv6() {
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let addr = SocketAddr::new(IpAddr::V6("::1".parse().unwrap()), 8119);
        let session = ClientSession::new(addr.into(), buffer_pool, test_auth_handler());

        assert_eq!(session.client_addr.port(), 8119);
        assert!(session.client_addr.is_ipv6());
    }

    #[test]
    fn test_buffer_pool_cloning() {
        let buffer_pool = BufferPool::new(BufferSize::try_new(8192).unwrap(), 10);
        let buffer_pool_clone = buffer_pool.clone();

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 1234);
        let _session1 = ClientSession::new(addr.into(), buffer_pool, test_auth_handler());
        let _session2 = ClientSession::new(addr.into(), buffer_pool_clone, test_auth_handler());
    }

    #[test]
    fn test_session_addr_formatting() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 5555);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let session = ClientSession::new(addr.into(), buffer_pool, test_auth_handler());

        let addr_str = format!("{}", session.client_addr);
        assert!(addr_str.contains("10.0.0.1"));
        assert!(addr_str.contains("5555"));
    }

    #[test]
    fn test_multiple_sessions_same_buffer_pool() {
        let buffer_pool = BufferPool::new(BufferSize::try_new(4096).unwrap(), 8);
        let sessions: Vec<_> = (0..5)
            .map(|i| {
                let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8000 + i);
                ClientSession::new(addr.into(), buffer_pool.clone(), test_auth_handler())
            })
            .collect();

        assert_eq!(sessions.len(), 5);
        for (i, session) in sessions.iter().enumerate() {
            assert_eq!(session.client_addr.port(), 8000 + i as u16);
        }
    }

    #[test]
    fn test_loopback_address() {
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8119);
        let session = ClientSession::new(addr.into(), buffer_pool, test_auth_handler());

        assert!(session.client_addr.ip().is_loopback());
    }

    #[test]
    fn test_unspecified_address() {
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);
        let session = ClientSession::new(addr.into(), buffer_pool, test_auth_handler());

        assert!(session.client_addr.ip().is_unspecified());
        assert_eq!(session.client_addr.port(), 0);
    }

    #[test]
    fn test_session_without_router() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let session = ClientSession::new(addr.into(), buffer_pool, test_auth_handler());

        assert!(!session.is_per_command_routing());
        assert_eq!(session.client_addr.port(), 8080);
    }

    #[test]
    fn test_session_with_router() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let router = Arc::new(BackendSelector::new());
        let session = ClientSession::new_with_router(
            addr.into(),
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
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);

        let session1 = ClientSession::new(addr.into(), buffer_pool.clone(), test_auth_handler());
        let session2 = ClientSession::new(addr.into(), buffer_pool, test_auth_handler());

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
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let router = Arc::new(BackendSelector::new());

        let session = ClientSession::new_with_router(
            addr.into(),
            buffer_pool,
            router,
            RoutingMode::Hybrid,
            test_auth_handler(),
        );

        assert!(session.is_per_command_routing());
        assert_eq!(session.mode_state.routing_mode(), RoutingMode::Hybrid);
        assert_eq!(session.mode(), SessionMode::PerCommand);
    }

    #[test]
    fn test_routing_mode_configurations() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let router = Arc::new(BackendSelector::new());

        // Stateful mode
        let session = ClientSession::new_with_router(
            addr.into(),
            buffer_pool.clone(),
            router.clone(),
            RoutingMode::Stateful,
            test_auth_handler(),
        );
        assert!(session.is_per_command_routing());
        assert_eq!(session.mode_state.routing_mode(), RoutingMode::Stateful);

        // PerCommand mode
        let session = ClientSession::new_with_router(
            addr.into(),
            buffer_pool.clone(),
            router.clone(),
            RoutingMode::PerCommand,
            test_auth_handler(),
        );
        assert!(session.is_per_command_routing());
        assert_eq!(session.mode_state.routing_mode(), RoutingMode::PerCommand);
        assert_eq!(session.mode(), SessionMode::PerCommand);

        // Hybrid mode
        let session = ClientSession::new_with_router(
            addr.into(),
            buffer_pool,
            router,
            RoutingMode::Hybrid,
            test_auth_handler(),
        );
        assert!(session.is_per_command_routing());
        assert_eq!(session.mode_state.routing_mode(), RoutingMode::Hybrid);
        assert_eq!(session.mode(), SessionMode::PerCommand);
    }

    #[test]
    fn test_hybrid_mode_initial_state() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let router = Arc::new(BackendSelector::new());

        let session = ClientSession::new_with_router(
            addr.into(),
            buffer_pool,
            router,
            RoutingMode::Hybrid,
            test_auth_handler(),
        );

        assert_eq!(session.mode(), SessionMode::PerCommand);
        assert_eq!(session.mode_state.routing_mode(), RoutingMode::Hybrid);
        assert!(session.is_per_command_routing());
    }

    #[test]
    fn test_is_per_command_routing_logic() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let router = Arc::new(BackendSelector::new());

        // Stateful mode has router capability
        let session = ClientSession::new_with_router(
            addr.into(),
            buffer_pool.clone(),
            router.clone(),
            RoutingMode::Stateful,
            test_auth_handler(),
        );
        assert!(session.is_per_command_routing());

        // PerCommand mode
        let session = ClientSession::new_with_router(
            addr.into(),
            buffer_pool.clone(),
            router.clone(),
            RoutingMode::PerCommand,
            test_auth_handler(),
        );
        assert!(session.is_per_command_routing());

        // Hybrid mode (initially)
        let session = ClientSession::new_with_router(
            addr.into(),
            buffer_pool.clone(),
            router,
            RoutingMode::Hybrid,
            test_auth_handler(),
        );
        assert!(session.is_per_command_routing());

        // Session without router
        let session = ClientSession::new(addr.into(), buffer_pool, test_auth_handler());
        assert!(!session.is_per_command_routing());
    }

    // ==================== ClientSessionBuilder Tests ====================

    #[test]
    fn test_builder_basic_construction() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let auth_handler = test_auth_handler();

        let session = ClientSession::builder(addr.into(), buffer_pool, auth_handler).build();

        assert_eq!(*session.client_addr, addr);
        assert!(!session.is_per_command_routing());
        assert_eq!(session.mode(), SessionMode::Stateful);
        assert_eq!(session.mode_state.routing_mode(), RoutingMode::Stateful);
    }

    #[test]
    fn test_builder_with_router() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let router = Arc::new(BackendSelector::new());

        let session = ClientSession::builder(addr.into(), buffer_pool, test_auth_handler())
            .with_router(router)
            .with_routing_mode(RoutingMode::PerCommand)
            .build();

        assert!(session.is_per_command_routing());
        assert_eq!(session.mode(), SessionMode::PerCommand);
        assert_eq!(session.mode_state.routing_mode(), RoutingMode::PerCommand);
    }

    #[test]
    fn test_builder_with_hybrid_mode() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let router = Arc::new(BackendSelector::new());

        let session = ClientSession::builder(addr.into(), buffer_pool, test_auth_handler())
            .with_router(router)
            .with_routing_mode(RoutingMode::Hybrid)
            .build();

        assert!(session.is_per_command_routing());
        assert_eq!(session.mode(), SessionMode::PerCommand);
        assert_eq!(session.mode_state.routing_mode(), RoutingMode::Hybrid);
    }

    #[test]
    fn test_builder_with_stateful_mode() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let router = Arc::new(BackendSelector::new());

        // Builder with router but Stateful mode requested
        let session = ClientSession::builder(addr.into(), buffer_pool, test_auth_handler())
            .with_router(router)
            .with_routing_mode(RoutingMode::Stateful)
            .build();

        assert!(session.is_per_command_routing());
        assert_eq!(session.mode(), SessionMode::Stateful);
        assert_eq!(session.mode_state.routing_mode(), RoutingMode::Stateful);
    }

    #[test]
    fn test_builder_without_router_ignores_mode() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);

        // Request PerCommand mode but no router - should default to Stateful
        let session = ClientSession::builder(addr.into(), buffer_pool, test_auth_handler())
            .with_routing_mode(RoutingMode::PerCommand)
            .build();

        assert!(!session.is_per_command_routing());
        assert_eq!(session.mode(), SessionMode::Stateful);
        assert_eq!(session.mode_state.routing_mode(), RoutingMode::Stateful);
    }

    #[test]
    fn test_builder_with_auth_handler() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let auth_handler =
            Arc::new(AuthHandler::new(Some("user".to_string()), Some("pass".to_string())).unwrap());

        let session = ClientSession::builder(addr.into(), buffer_pool, test_auth_handler())
            .with_auth_handler(auth_handler.clone())
            .build();

        // Verify auth_handler is set (can't test internals, but creation succeeds)
        assert_eq!(*session.client_addr, addr);
    }

    #[test]
    fn test_builder_with_metrics() {
        use crate::metrics::MetricsCollector;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let metrics = MetricsCollector::new(1); // 1 backend

        let session = ClientSession::builder(addr.into(), buffer_pool, test_auth_handler())
            .with_metrics(metrics)
            .build();

        assert!(session.metrics.is_some());
    }

    #[test]
    fn test_builder_with_connection_stats() {
        use crate::metrics::ConnectionStatsAggregator;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let stats = ConnectionStatsAggregator::default();

        let session = ClientSession::builder(addr.into(), buffer_pool, test_auth_handler())
            .with_connection_stats(stats)
            .build();

        assert!(session.connection_stats().is_some());
    }

    #[test]
    fn test_builder_with_cache() {
        use crate::cache::ArticleCache;
        use std::time::Duration;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let cache = Arc::new(ArticleCache::new(100, Duration::from_secs(3600)));

        let session = ClientSession::builder(addr.into(), buffer_pool, test_auth_handler())
            .with_cache(cache)
            .build();

        assert!(session.cache.is_some());
    }

    #[test]
    fn test_builder_method_chaining() {
        use crate::metrics::MetricsCollector;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let router = Arc::new(BackendSelector::new());
        let metrics = MetricsCollector::new(1); // 1 backend

        // Chain all builder methods
        let session = ClientSession::builder(addr.into(), buffer_pool, test_auth_handler())
            .with_router(router)
            .with_routing_mode(RoutingMode::Hybrid)
            .with_metrics(metrics)
            .build();

        assert!(session.is_per_command_routing());
        assert_eq!(session.mode(), SessionMode::PerCommand);
        assert_eq!(session.mode_state.routing_mode(), RoutingMode::Hybrid);
        assert!(session.metrics.is_some());
    }

    // ==================== Session Business Logic Tests ====================

    #[test]
    fn test_mode_getter() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let router = Arc::new(BackendSelector::new());

        // Per-command mode
        let session = ClientSession::new_with_router(
            addr.into(),
            buffer_pool.clone(),
            router.clone(),
            RoutingMode::PerCommand,
            test_auth_handler(),
        );
        assert_eq!(session.mode(), SessionMode::PerCommand);

        // Stateful mode (no router)
        let session = ClientSession::new(addr.into(), buffer_pool, test_auth_handler());
        assert_eq!(session.mode(), SessionMode::Stateful);
    }

    #[test]
    fn test_username_initially_none() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let session = ClientSession::new(addr.into(), buffer_pool, test_auth_handler());

        assert!(session.username().is_none());
    }

    #[test]
    fn test_set_username_and_get() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let session = ClientSession::new(addr.into(), buffer_pool, test_auth_handler());

        session.set_username(Some("testuser".to_string()));

        let username = session.username();
        assert!(username.is_some());
        assert_eq!(username.as_deref(), Some("testuser"));
    }

    #[test]
    fn test_set_username_none() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let session = ClientSession::new(addr.into(), buffer_pool, test_auth_handler());

        session.set_username(None);

        assert!(session.username().is_none());
    }

    #[test]
    fn test_username_cheap_clone() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let session = ClientSession::new(addr.into(), buffer_pool, test_auth_handler());

        session.set_username(Some("testuser".to_string()));

        let username1 = session.username();
        let username2 = session.username();

        assert!(username1.is_some());
        assert!(username2.is_some());
        assert_eq!(username1, username2);
        assert_eq!(username1.as_deref(), Some("testuser"));
    }

    #[test]
    fn test_connection_stats_none_by_default() {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let session = ClientSession::new(addr.into(), buffer_pool, test_auth_handler());

        assert!(session.connection_stats().is_none());
    }

    #[test]
    fn test_connection_stats_with_aggregator() {
        use crate::metrics::ConnectionStatsAggregator;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let stats = ConnectionStatsAggregator::default();

        let session = ClientSession::builder(addr.into(), buffer_pool, test_auth_handler())
            .with_connection_stats(stats)
            .build();

        assert!(session.connection_stats().is_some());
    }

    // ==================== Metrics Helper Methods Tests ====================

    #[test]
    fn test_metrics_helpers_no_metrics() {
        use crate::types::BackendId;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let session = ClientSession::new(addr.into(), buffer_pool, test_auth_handler());

        // Should not panic when metrics is None
        session.record_command(BackendId::from_index(0));
        session.user_command();
        session.stateful_session_started();
        session.stateful_session_ended();
        session.user_bytes_sent(1024);
        session.user_bytes_received(2048);
    }

    #[test]
    fn test_metrics_helpers_with_metrics() {
        use crate::metrics::MetricsCollector;
        use crate::types::BackendId;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let metrics = MetricsCollector::new(1); // 1 backend

        let session = ClientSession::builder(addr.into(), buffer_pool, test_auth_handler())
            .with_metrics(metrics.clone())
            .build();

        session.set_username(Some("testuser".to_string()));

        // Call all metrics helpers - should not panic
        session.record_command(BackendId::from_index(0));
        session.user_command();
        session.stateful_session_started();
        session.stateful_session_ended();
        session.user_bytes_sent(1024);
        session.user_bytes_received(2048);

        // Verify metrics were recorded (snapshot should have data)
        let snapshot = metrics.snapshot(None);
        assert!(!snapshot.backend_stats.is_empty());
        assert!(snapshot.backend_stats[0].total_commands.get() > 0);
    }

    #[test]
    fn test_record_command_with_metrics() {
        use crate::metrics::MetricsCollector;
        use crate::types::BackendId;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let metrics = MetricsCollector::new(1); // 1 backend

        let session = ClientSession::builder(addr.into(), buffer_pool, test_auth_handler())
            .with_metrics(metrics.clone())
            .build();

        let backend_id = BackendId::from_index(0);
        session.record_command(backend_id);

        let snapshot = metrics.snapshot(None);
        assert_eq!(snapshot.backend_stats[0].total_commands.get(), 1);
    }

    #[test]
    fn test_user_bytes_tracking() {
        use crate::metrics::MetricsCollector;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let metrics = MetricsCollector::new(1); // 1 backend

        let session = ClientSession::builder(addr.into(), buffer_pool, test_auth_handler())
            .with_metrics(metrics.clone())
            .build();

        session.set_username(Some("testuser".to_string()));
        session.user_bytes_sent(1024);
        session.user_bytes_received(2048);

        let snapshot = metrics.snapshot(None);
        let user_stats = snapshot
            .user_stats
            .iter()
            .find(|s| s.username == "testuser");
        assert!(user_stats.is_some());

        let stats = user_stats.unwrap();
        assert_eq!(stats.bytes_sent.as_u64(), 1024);
        assert_eq!(stats.bytes_received.as_u64(), 2048);
    }

    #[test]
    fn test_stateful_session_tracking() {
        use crate::metrics::MetricsCollector;

        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
        let buffer_pool = BufferPool::new(BufferSize::try_new(1024).unwrap(), 4);
        let metrics = MetricsCollector::new(1); // 1 backend

        let session = ClientSession::builder(addr.into(), buffer_pool, test_auth_handler())
            .with_metrics(metrics.clone())
            .build();

        session.stateful_session_started();

        let snapshot = metrics.snapshot(None);
        assert_eq!(snapshot.stateful_sessions, 1);

        session.stateful_session_ended();

        let snapshot = metrics.snapshot(None);
        assert_eq!(snapshot.stateful_sessions, 0);
    }
}
