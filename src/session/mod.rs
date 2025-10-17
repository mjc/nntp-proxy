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
pub mod connection;
pub mod handlers;
pub mod streaming;

use std::net::SocketAddr;
use std::sync::Arc;

use crate::config::RoutingMode;
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
///
/// let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
/// let buffer_pool = BufferPool::new(BufferSize::DEFAULT, 10);
///
/// // Standard 1:1 routing mode
/// let session = ClientSession::builder(addr, buffer_pool.clone())
///     .build();
///
/// // Per-command routing mode
/// let router = Arc::new(BackendSelector::new());
/// let session = ClientSession::builder(addr, buffer_pool.clone())
///     .with_router(router)
///     .with_routing_mode(RoutingMode::PerCommand)
///     .build();
/// ```
pub struct ClientSessionBuilder {
    client_addr: SocketAddr,
    buffer_pool: BufferPool,
    router: Option<Arc<BackendSelector>>,
    routing_mode: RoutingMode,
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
            (Some(_), RoutingMode::Standard) => {
                (SessionMode::PerCommand, RoutingMode::PerCommand)
            }
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
        }
    }
}

impl ClientSession {
    /// Create a new client session for 1:1 backend mapping
    #[must_use]
    pub fn new(client_addr: SocketAddr, buffer_pool: BufferPool) -> Self {
        Self {
            client_addr,
            buffer_pool,
            client_id: ClientId::new(),
            router: None,
            mode: SessionMode::Stateful, // 1:1 mode is always stateful
            routing_mode: RoutingMode::Standard,
        }
    }

    /// Create a new client session for per-command routing mode
    #[must_use]
    pub fn new_with_router(
        client_addr: SocketAddr,
        buffer_pool: BufferPool,
        router: Arc<BackendSelector>,
        routing_mode: RoutingMode,
    ) -> Self {
        Self {
            client_addr,
            buffer_pool,
            client_id: ClientId::new(),
            router: Some(router),
            mode: SessionMode::PerCommand, // Starts in per-command mode
            routing_mode,
        }
    }

    /// Create a builder for constructing a client session
    ///
    /// # Examples
    ///
    /// ```
    /// use std::net::SocketAddr;
    /// use nntp_proxy::session::ClientSession;
    /// use nntp_proxy::pool::BufferPool;
    /// use nntp_proxy::types::BufferSize;
    ///
    /// let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    /// let buffer_pool = BufferPool::new(BufferSize::DEFAULT, 10);
    ///
    /// // Standard 1:1 routing mode
    /// let session = ClientSession::builder(addr, buffer_pool.clone())
    ///     .build();
    ///
    /// assert!(!session.is_per_command_routing());
    /// ```
    #[must_use]
    pub fn builder(client_addr: SocketAddr, buffer_pool: BufferPool) -> ClientSessionBuilder {
        ClientSessionBuilder {
            client_addr,
            buffer_pool,
            router: None,
            routing_mode: RoutingMode::Standard,
        }
    }

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
}

#[cfg(test)]
mod tests;
