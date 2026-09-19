//! # NNTP Proxy Library
//!
//! A high-performance NNTP proxy server and pooled NNTP client with hybrid,
//! stateful, and per-command routing modes.
//!
//! The crate requires Rust 1.91 or newer. The `nntp-proxy` binary is the usual
//! entry point for running a local proxy; the library API is useful when an
//! application needs to embed the proxy or fetch article responses directly.
//!
//! Applications embedding the crate can use [`NntpProxy`] and
//! [`NntpProxyBuilder`] to construct the server, or use [`client::NntpClient`]
//! to fetch article responses from an upstream server.
//!
//! ## Architecture
//!
//! The proxy is organized into several modules for clean separation of concerns:
//!
//! - **auth**: Authentication handling for both client and backend connections
//! - **command**: Local NNTP command policy and intercept/reject actions
//! - **config**: Configuration loading and management
//! - **pool**: Connection and buffer pooling for high performance
//! - **protocol**: Typed NNTP request metadata, response constants, and status parsing
//! - **proxy**: Main proxy orchestration (`NntpProxy` struct)
//! - **router**: Backend selection and load balancing
//! - **types**: Core type definitions (`ClientId`, `RequestId`, `BackendId`)
//! - **client**: Standalone pooled article-fetching API
//!
//! ## Operating Modes
//!
//! - **Hybrid mode**: Starts with per-command routing and switches to a
//!   dedicated backend connection when stateful protocol context is needed.
//! - **Stateful mode**: Traditional mode where each client gets a dedicated
//!   backend connection.
//! - **Per-command routing mode**: Each supported stateless command can route to
//!   a backend independently.
//!
//! Response shape is request-scoped: callers must not infer multiline behavior
//! from a status code alone. The session layer owns response boundaries,
//! continuation, packed-response suffixes, and ordered delivery; ordinary
//! forwarding can therefore borrow pooled bytes without constructing a second
//! owned representation.
//!
//! ## Article response ownership
//!
//! [`client::NntpClient`] returns a [`client::FramedArticle`] after the response
//! boundary has been established. Calling [`client::FramedArticle::validate`]
//! or [`client::FramedArticle::validate_with_yenc`] consumes that state and
//! returns a reusable [`client::ValidatedArticle`]. Its
//! [`client::ValidatedArticle::article`] method returns a zero-copy
//! [`protocol::ArticleView`] without repeating semantic validation.
//!
//! Framing, article semantics, and optional yEnc validation remain separate
//! operations. A framed response is not automatically a semantically valid
//! article, and a validated article does not imply that its body has been
//! yEnc-decoded. This lets a caller select the amount of work appropriate for
//! forwarding, indexing, inspection, or decoding.

#![allow(clippy::disallowed_methods)]

#[macro_use]
mod macros;

// Module declarations
pub mod args;
pub mod auth;
pub mod client;
mod compression;
pub mod connection_error;
pub mod formatting;
mod io_util;
pub mod logging;
pub mod metrics;
mod network;
pub mod protocol;
mod proxy;
pub mod stream;
pub mod tui;

// Public modules for integration tests
pub mod cache;
pub mod command;
pub mod config;
pub mod constants;
pub mod pool;
pub mod router;
pub mod runtime;
pub mod session;
pub mod tls;
pub mod types;

// Public exports
pub use args::{CommonArgs, UiMode};
pub use config::{
    Cache, Config, ConfigSource, RoutingMode, Server, create_default_config, has_server_env_vars,
    load_config, load_config_from_env, load_config_with_fallback,
};
pub use proxy::{NntpProxy, NntpProxyBuilder};
pub use runtime::{RuntimeConfig, shutdown_signal};
