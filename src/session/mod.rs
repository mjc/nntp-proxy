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
//! use nntp_proxy::metrics::MetricsCollector;
//!
//! # fn example() -> anyhow::Result<()> {
//! let client_addr: SocketAddr = "127.0.0.1:50000".parse()?;
//! let buffer_pool = BufferPool::new(BufferSize::try_new(8192)?, 10);
//! let auth = Arc::new(AuthHandler::new(None, None)?);
//! let metrics = MetricsCollector::new(1);
//!
//! // Create a simple 1:1 session (no load balancing)
//! let session = ClientSession::new(client_addr.into(), buffer_pool, auth, metrics);
//! assert_eq!(session.mode(), nntp_proxy::session::SessionMode::Stateful);
//! # Ok(())
//! # }
//! ```
//!
//! ## Per-Command Routing (Load Balancing)
//!
//! ```no_run
//! use std::sync::Arc;
//! use std::net::SocketAddr;
//! use nntp_proxy::session::ClientSession;
//! use nntp_proxy::pool::BufferPool;
//! use nntp_proxy::router::BackendSelector;
//! use nntp_proxy::config::RoutingMode;
//! use nntp_proxy::types::BufferSize;
//! use nntp_proxy::auth::AuthHandler;
//! use nntp_proxy::metrics::MetricsCollector;
//!
//! # fn example() -> anyhow::Result<()> {
//! let addr: SocketAddr = "127.0.0.1:50000".parse()?;
//! let buffer_pool = BufferPool::new(BufferSize::try_new(8192)?, 10);
//! let router = Arc::new(BackendSelector::new());
//! let auth = Arc::new(AuthHandler::new(None, None)?);
//! let metrics = MetricsCollector::new(1);
//!
//! // Each command routed to potentially different backend
//! let session = ClientSession::builder(addr.into(), buffer_pool, auth, metrics)
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
//! use std::net::SocketAddr;
//! use nntp_proxy::session::ClientSession;
//! use nntp_proxy::pool::BufferPool;
//! use nntp_proxy::router::BackendSelector;
//! use nntp_proxy::config::RoutingMode;
//! use nntp_proxy::types::BufferSize;
//! use nntp_proxy::auth::AuthHandler;
//! use nntp_proxy::metrics::MetricsCollector;
//!
//! # fn example() -> anyhow::Result<()> {
//! let addr: SocketAddr = "127.0.0.1:50000".parse()?;
//! let buffer_pool = BufferPool::new(BufferSize::try_new(8192)?, 10);
//! let router = Arc::new(BackendSelector::new());
//! let auth = Arc::new(AuthHandler::new(None, None)?);
//! let metrics = MetricsCollector::new(1);
//!
//! // Starts in per-command mode, auto-switches to stateful when needed
//! let session = ClientSession::builder(addr.into(), buffer_pool, auth, metrics)
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
//! ## With Caching
//!
//! ```no_run
//! use std::sync::Arc;
//! use std::net::SocketAddr;
//! use std::time::Duration;
//! use nntp_proxy::session::ClientSession;
//! use nntp_proxy::pool::BufferPool;
//! use nntp_proxy::router::BackendSelector;
//! use nntp_proxy::config::RoutingMode;
//! use nntp_proxy::types::BufferSize;
//! use nntp_proxy::auth::AuthHandler;
//! use nntp_proxy::metrics::MetricsCollector;
//! use nntp_proxy::cache::UnifiedCache;
//!
//! # fn example() -> anyhow::Result<()> {
//! let addr: SocketAddr = "127.0.0.1:50000".parse()?;
//! let buffer_pool = BufferPool::new(BufferSize::try_new(8192)?, 10);
//! let router = Arc::new(BackendSelector::new());
//! let auth = Arc::new(AuthHandler::new(None, None)?);
//! let metrics = MetricsCollector::new(2); // 2 backends
//! let cache = Arc::new(UnifiedCache::memory(1000, Duration::from_secs(3600), true));
//!
//! // Full-featured session with caching
//! let session = ClientSession::builder(addr.into(), buffer_pool, auth, metrics)
//!     .with_router(router)
//!     .with_routing_mode(RoutingMode::Hybrid)
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
//! - `handle_stateful_proxy_loop()` - **PERFORMANCE CRITICAL HOT PATH**
//!   - Bidirectional streaming with tokio::select! for concurrent I/O
//!   - Used by both stateful mode and hybrid mode after switching
//!
//! - `switch_to_stateful_mode()` - Hybrid mode transition
//!   - Acquires dedicated backend connection
//!   - Hands off to stateful proxy loop
//!
//! - `route_and_execute_command()` - Per-command orchestration
//!   - Routes command to backend
//!   - Handles connection pool management
//!   - Distinguishes backend errors from client disconnects

pub mod auth_state;
pub mod backend;
pub(crate) mod common;
pub mod connection;
pub mod core;
pub mod error_classification;
pub mod handlers;
pub mod metrics_ext;
pub mod mode_state;
pub mod precheck;
pub(crate) mod routing;
pub mod state;
pub mod streaming;

pub use auth_state::AuthState;
pub use core::{ClientSession, ClientSessionBuilder};
pub use metrics_ext::MetricsRecorder;
pub use mode_state::{ModeState, SessionMode};
pub use state::SessionLoopState;
