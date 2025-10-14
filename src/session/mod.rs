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
