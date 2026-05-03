//! Session lifecycle management, idle tracking, and connection helpers
//!
//! Contains the private helper methods on `NntpProxy` for managing
//! session setup, finalization, metrics recording, and idle pool cleanup.

use anyhow::Result;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::net::TcpStream;
use tracing::{debug, info, warn};

use crate::cache::UnifiedCache;
use crate::config::RoutingMode;
use crate::network::NetworkOptimizer;
use crate::pool::ConnectionProvider;
use crate::router;
use crate::session::ClientSession;
use crate::types::{self, ClientAddress, TransferMetrics};

use crate::session::SessionError;

use super::NntpProxy;

#[allow(clippy::cast_possible_truncation)] // Elapsed nanoseconds are saturated into a monotonic u64 diagnostic counter.
fn elapsed_nanos_u64(start: std::time::Instant) -> u64 {
    // This is a monotonic diagnostic timestamp. Saturating on overflow keeps
    // the last-activity marker valid without threading error handling through
    // the shutdown path.
    u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

impl NntpProxy {
    // Helper methods for session management

    #[inline]
    pub(super) fn record_connection_opened(&self) {
        self.metrics.connection_opened();
    }

    #[inline]
    pub(super) fn record_connection_closed(&self) {
        self.metrics.connection_closed();
    }

    /// Increment active client count
    ///
    /// Call this when a new client connection is accepted.
    #[inline]
    pub fn increment_active_clients(&self) {
        self.active_clients.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement active client count and update last activity timestamp
    ///
    /// Call this when a client connection closes.
    #[inline]
    pub fn decrement_active_clients(&self) {
        let prev = self.active_clients.fetch_sub(1, Ordering::Relaxed);

        // When last client disconnects, record the timestamp
        if prev == 1 {
            let nanos = elapsed_nanos_u64(self.start_instant);
            self.last_activity_nanos.store(nanos, Ordering::Relaxed);
        }
    }

    /// Check if any backend pools should be cleared due to idle timeout.
    ///
    /// Returns true if any pools were cleared.
    /// Each backend is checked independently against its configured `backend_idle_timeout`.
    /// A backend's pool is cleared when:
    /// 1. No clients are currently active
    /// 2. The last activity was more than that backend's `backend_idle_timeout` ago
    ///
    /// This prevents stale connections from accumulating during overnight idle periods.
    pub(crate) fn check_and_clear_stale_pools(&self) -> bool {
        // Fast path: if there are active clients, pools are in use
        if self.active_clients.load(Ordering::Relaxed) > 0 {
            return false;
        }

        let last_activity_nanos = self.last_activity_nanos.load(Ordering::Relaxed);

        // If never been active, no need to clear
        if last_activity_nanos == 0 {
            return false;
        }

        let last_activity = Duration::from_nanos(last_activity_nanos);
        let now = self.start_instant.elapsed();
        let idle_duration = now.saturating_sub(last_activity);

        let mut any_cleared = false;
        for (server, provider) in self.servers.iter().zip(&self.connection_providers) {
            if idle_duration > server.backend_idle_timeout {
                info!(
                    backend = server.name.as_ref(),
                    idle_secs = idle_duration.as_secs(),
                    timeout_secs = server.backend_idle_timeout.as_secs(),
                    "Clearing idle backend connections"
                );
                provider.clear_idle_connections();
                any_cleared = true;
            }
        }

        any_cleared
    }

    /// Build a session with standard configuration (conditionally enables metrics)
    pub(super) fn build_session(
        &self,
        client_addr: ClientAddress,
        router: Option<Arc<router::BackendSelector>>,
        routing_mode: RoutingMode,
        cache: Arc<UnifiedCache>,
    ) -> ClientSession {
        // Start with base builder
        let builder = ClientSession::builder(
            client_addr,
            self.buffer_pool.clone(),
            self.auth_handler.clone(),
            self.metrics.clone(),
        )
        .with_routing_mode(routing_mode)
        .with_connection_stats(self.connection_stats.clone())
        .with_cache(cache)
        .with_cache_articles(self.cache_articles)
        .with_adaptive_precheck(self.adaptive_precheck);

        // Apply optional router
        let builder = match router {
            Some(r) => builder.with_router(r),
            None => builder,
        };

        builder.build()
    }

    /// Log session completion and record stats
    pub(super) fn log_session_completion(
        &self,
        client_addr: ClientAddress,
        session_id: &str,
        session: &ClientSession,
        routing_mode: crate::config::RoutingMode,
        metrics: &types::TransferMetrics,
    ) {
        self.connection_stats
            .record_disconnection(session.username(), routing_mode.short_name());

        debug!(
            "Session {} [{}] ↑{} ↓{}",
            client_addr,
            session_id,
            crate::formatting::format_bytes(metrics.client_to_backend.as_u64()),
            crate::formatting::format_bytes(metrics.backend_to_client.as_u64())
        );
    }

    /// Log backend routing selection
    #[inline]
    pub(super) fn log_routing_selection(
        client_addr: ClientAddress,
        backend_id: crate::types::BackendId,
        server: &crate::config::Server,
    ) {
        info!(
            "Routing client {} to backend {:?} ({}:{})",
            client_addr, backend_id, server.host, server.port
        );
    }

    /// Log connection pool status for monitoring
    #[inline]
    pub(super) fn log_pool_status(&self, server_idx: usize) {
        let pool_status = self.connection_providers[server_idx].status();
        debug!(
            "Pool status for {}: {}/{} available, {} created",
            self.servers[server_idx].name,
            pool_status.available,
            pool_status.max_size,
            pool_status.created
        );
    }

    /// Prepare stateful connection - route, greet, optimize
    pub(super) async fn prepare_stateful_connection(
        &self,
        client_stream: &mut TcpStream,
        client_addr: ClientAddress,
    ) -> Result<crate::types::BackendId> {
        self.record_connection_opened();

        let client_id = types::ClientId::new();
        let backend_id = self.router.route(client_id)?;
        let server_idx = backend_id.as_index();

        Self::log_routing_selection(client_addr, backend_id, &self.servers[server_idx]);
        self.send_greeting(client_stream, client_addr).await?;
        self.log_pool_status(server_idx);
        Self::apply_tcp_optimizations(client_stream);

        Ok(backend_id)
    }

    /// Prepare per-command connection - record, greet, optimize
    pub(super) async fn prepare_per_command_connection(
        &self,
        client_stream: &mut TcpStream,
        client_addr: ClientAddress,
    ) -> Result<()> {
        self.record_connection_opened();
        self.send_greeting(client_stream, client_addr).await?;
        Self::apply_tcp_optimizations(client_stream);
        Ok(())
    }

    /// Create session with router and cache configuration
    #[inline]
    pub(super) fn create_session(
        &self,
        client_addr: ClientAddress,
        router: Option<Arc<crate::router::BackendSelector>>,
    ) -> ClientSession {
        self.build_session(client_addr, router, self.routing_mode, self.cache.clone())
    }

    /// Generate short session ID for logging
    #[inline]
    pub(super) fn generate_session_id(session: &ClientSession) -> String {
        crate::formatting::short_id(session.client_id().as_uuid())
    }

    /// Send greeting to client
    #[inline]
    pub(super) async fn send_greeting(
        &self,
        client_stream: &mut TcpStream,
        client_addr: ClientAddress,
    ) -> Result<()> {
        crate::protocol::send_proxy_greeting(client_stream, client_addr).await
    }

    /// Apply TCP optimizations to client socket
    #[inline]
    pub(super) fn apply_tcp_optimizations(client_stream: &TcpStream) {
        use crate::network::TcpOptimizer;
        TcpOptimizer::new(client_stream)
            .optimize()
            .map_err(|e| debug!("Failed to optimize client socket: {}", e))
            .ok();
    }

    /// Get display name for current routing mode
    #[inline]
    pub(super) fn routing_mode_display_name(&self) -> &'static str {
        if self.cache.entry_count() > 0 {
            "caching"
        } else {
            "per-command"
        }
    }

    /// Finalize stateful session with metrics and cleanup
    pub(super) fn finalize_stateful_session(
        &self,
        metrics: Result<TransferMetrics, SessionError>,
        client_addr: ClientAddress,
        session_id: &str,
        session: &ClientSession,
        backend_id: crate::types::BackendId,
    ) -> Result<(), SessionError> {
        self.record_connection_if_unauthenticated(session);
        self.router.complete_command(backend_id);
        self.record_session_metrics(metrics, client_addr, session_id, session, Some(backend_id))?;
        self.record_connection_closed();
        Ok(())
    }

    /// Finalize per-command session with logging and cleanup
    pub(super) fn finalize_per_command_session(
        &self,
        metrics: Result<TransferMetrics, SessionError>,
        client_addr: ClientAddress,
        session_id: &str,
        session: &ClientSession,
    ) -> Result<(), SessionError> {
        self.record_session_metrics(metrics, client_addr, session_id, session, None)?;
        self.record_connection_closed();
        Ok(())
    }

    /// Record connection for unauthenticated sessions only
    #[inline]
    pub(super) fn record_connection_if_unauthenticated(&self, session: &ClientSession) {
        if !self.auth_handler.is_enabled() || session.username().is_none() {
            let mode = self.session_mode_label(session.mode());
            self.connection_stats
                .record_connection(session.username(), mode);
        }
    }

    /// Record session metrics and log completion or errors
    pub(super) fn record_session_metrics(
        &self,
        metrics: Result<TransferMetrics, SessionError>,
        client_addr: ClientAddress,
        session_id: &str,
        session: &ClientSession,
        backend_id: Option<crate::types::BackendId>,
    ) -> Result<(), SessionError> {
        match metrics {
            Ok(m) => {
                self.log_session_completion(
                    client_addr,
                    session_id,
                    session,
                    self.routing_mode,
                    &m,
                );

                if let Some(bid) = backend_id {
                    self.metrics
                        .record_client_to_backend_bytes_for(bid, m.client_to_backend.as_u64());
                    self.metrics
                        .record_backend_to_client_bytes_for(bid, m.backend_to_client.as_u64());
                }
                Ok(())
            }
            Err(e) => {
                if let Some(bid) = backend_id {
                    self.metrics.record_error(bid);
                }

                // Only log backend errors — client disconnects are normal operation
                if let SessionError::Backend(ref inner) = e {
                    warn!("Session error for client {}: {:?}", client_addr, inner);
                }
                Err(e)
            }
        }
    }

    /// Get session mode label for logging
    #[inline]
    pub(super) const fn session_mode_label(
        &self,
        session_mode: crate::session::SessionMode,
    ) -> &'static str {
        use crate::session::SessionMode;
        match (session_mode, self.routing_mode) {
            (SessionMode::PerCommand, _) => "per-command",
            (SessionMode::Stateful, RoutingMode::Stateful) => "standard",
            (SessionMode::Stateful, RoutingMode::Hybrid) => "hybrid",
            (SessionMode::Stateful, _) => "stateful",
        }
    }

    /// Handle a client connection using stateful routing.
    ///
    /// # Errors
    /// Returns `SessionError::ClientDisconnect` when the client closes the
    /// connection mid-session and `SessionError::Backend` for greeting, routing,
    /// or stateful session-processing failures.
    pub async fn handle_client(
        &self,
        mut client_stream: TcpStream,
        client_addr: ClientAddress,
    ) -> Result<(), SessionError> {
        debug!("New client connection from {}", client_addr);

        // Check for stale pools before handling (lazy recreation after idle)
        self.check_and_clear_stale_pools();
        self.increment_active_clients();

        let result: Result<(), SessionError> = async {
            let backend_id = self
                .prepare_stateful_connection(&mut client_stream, client_addr)
                .await
                .map_err(SessionError::Backend)?;
            let server_idx = backend_id.as_index();

            let session = self.create_session(client_addr, None);
            let session_id = Self::generate_session_id(&session);

            debug!("Starting stateful session for client {}", client_addr);

            let metrics = session
                .handle_stateful_session(
                    client_stream,
                    backend_id,
                    &self.connection_providers[server_idx],
                    &self.servers[server_idx].name,
                )
                .await;

            self.finalize_stateful_session(metrics, client_addr, &session_id, &session, backend_id)
        }
        .await;

        self.decrement_active_clients();
        result
    }

    /// Handle client connection using per-command routing mode
    ///
    /// This creates a session with the router, allowing commands from this client
    /// to be routed to different backends based on load balancing.
    ///
    /// # Errors
    /// Returns `SessionError::ClientDisconnect` when the client closes the
    /// connection mid-session and `SessionError::Backend` for greeting, routing,
    /// or per-command session-processing failures.
    pub async fn handle_client_per_command_routing(
        &self,
        client_stream: TcpStream,
        client_addr: ClientAddress,
    ) -> Result<(), SessionError> {
        // Check for stale pools before handling (lazy recreation after idle)
        self.check_and_clear_stale_pools();
        self.increment_active_clients();

        let result = Box::pin(self.handle_per_command_client(client_stream, client_addr)).await;

        self.decrement_active_clients();
        result
    }

    /// Handle a per-command routing session
    async fn handle_per_command_client(
        &self,
        mut client_stream: TcpStream,
        client_addr: ClientAddress,
    ) -> Result<(), SessionError> {
        let mode_label = self.routing_mode_display_name();
        debug!(
            "New {} routing client connection from {}",
            mode_label, client_addr
        );

        self.prepare_per_command_connection(&mut client_stream, client_addr)
            .await
            .map_err(SessionError::Backend)?;

        let session = self.create_session(client_addr, Some(self.router.clone()));
        let session_id = Self::generate_session_id(&session);

        let metrics = Box::pin(session.handle_per_command_routing(client_stream)).await;

        self.finalize_per_command_session(metrics, client_addr, &session_id, &session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RoutingMode;
    use crate::session::SessionMode;
    use std::sync::Arc;

    fn create_test_config() -> crate::config::Config {
        super::super::tests::create_test_config()
    }

    #[test]
    fn test_session_mode_label_per_command() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::PerCommand).unwrap();

        let label = proxy.session_mode_label(SessionMode::PerCommand);
        assert_eq!(label, "per-command");
    }

    #[test]
    fn test_session_mode_label_stateful_standard() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();

        let label = proxy.session_mode_label(SessionMode::Stateful);
        assert_eq!(label, "standard");
    }

    #[test]
    fn test_session_mode_label_stateful_hybrid() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Hybrid).unwrap();

        let label = proxy.session_mode_label(SessionMode::Stateful);
        assert_eq!(label, "hybrid");
    }

    #[test]
    fn test_routing_mode_display_name_caching() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::PerCommand).unwrap();

        // Empty cache (default 0 capacity) should return "per-command"
        assert_eq!(proxy.routing_mode_display_name(), "per-command");
    }

    #[test]
    fn test_generate_session_id_format() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();

        let session = proxy.create_session(
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap()),
            None,
        );

        let session_id = NntpProxy::generate_session_id(&session);

        // Should be a short UUID (8 characters)
        assert_eq!(session_id.len(), 8);
    }

    #[test]
    fn test_create_session_without_router() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();

        let session = proxy.create_session(
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap()),
            None,
        );

        // Session should be created successfully
        assert_eq!(session.mode(), SessionMode::Stateful);
    }

    #[test]
    fn test_create_session_with_router() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::PerCommand).unwrap();

        let session = proxy.create_session(
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap()),
            Some(proxy.router.clone()),
        );

        // Session mode depends on router presence and config
        // With router, it should be per-command
        assert_eq!(session.mode(), SessionMode::PerCommand);
    }

    #[test]
    fn test_record_connection_if_unauthenticated_no_auth() {
        let config = create_test_config();
        let proxy = Arc::new(NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap());

        let session = proxy.create_session(
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap()),
            None,
        );

        // Should not panic
        proxy.record_connection_if_unauthenticated(&session);

        // Connection should be recorded (we can't easily verify without exposing internals)
    }

    #[test]
    fn test_record_session_metrics_success() {
        let config = create_test_config();
        let proxy = Arc::new(NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap());

        let session = proxy.create_session(
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap()),
            None,
        );
        let session_id = NntpProxy::generate_session_id(&session);

        let metrics = TransferMetrics {
            client_to_backend: crate::types::ClientToBackendBytes::new(1024),
            backend_to_client: crate::types::BackendToClientBytes::new(2048),
        };

        let result = proxy.record_session_metrics(
            Ok(metrics),
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap()),
            &session_id,
            &session,
            Some(crate::types::BackendId::from_index(0)),
        );

        assert!(result.is_ok());
    }

    #[test]
    fn test_record_session_metrics_error() {
        let config = create_test_config();
        let proxy = Arc::new(NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap());

        let session = proxy.create_session(
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap()),
            None,
        );
        let session_id = NntpProxy::generate_session_id(&session);

        let result = proxy.record_session_metrics(
            Err(SessionError::Backend(anyhow::anyhow!("test error"))),
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap()),
            &session_id,
            &session,
            Some(crate::types::BackendId::from_index(0)),
        );

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "test error");
    }

    #[tokio::test]
    async fn test_prepare_per_command_connection() {
        let config = create_test_config();
        let proxy = Arc::new(
            NntpProxy::new(config, RoutingMode::PerCommand)
                .await
                .unwrap(),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Spawn a simple acceptor that reads greeting
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = stream.try_read(&mut buf); // Read greeting
        });

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let client_addr = ClientAddress::from(stream.peer_addr().unwrap());

        let result = proxy
            .prepare_per_command_connection(&mut stream, client_addr)
            .await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_routing_mode_display_name_empty_cache() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Hybrid).unwrap();

        let _empty_cache = Arc::new(crate::cache::UnifiedCache::memory(
            100,
            std::time::Duration::from_hours(1),
            false,
        ));
        assert_eq!(proxy.routing_mode_display_name(), "per-command");
    }

    #[test]
    fn test_log_routing_selection() {
        let config = create_test_config();
        let proxy = Arc::new(NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap());

        let backend_id = crate::types::BackendId::from_index(0);
        let client_addr =
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap());

        // Should not panic
        NntpProxy::log_routing_selection(client_addr, backend_id, &proxy.servers()[0]);
    }

    #[test]
    fn test_log_pool_status() {
        let config = create_test_config();
        let proxy = Arc::new(NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap());

        // Should not panic
        proxy.log_pool_status(0);
    }

    #[tokio::test]
    async fn test_apply_tcp_optimizations() {
        let config = create_test_config();
        let _proxy = Arc::new(NntpProxy::new(config, RoutingMode::Stateful).await.unwrap());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        // Should not panic
        NntpProxy::apply_tcp_optimizations(&stream);
    }

    #[test]
    fn test_session_mode_labels_all_combinations() {
        use crate::session::SessionMode;

        // PerCommand mode
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config.clone(), RoutingMode::PerCommand).unwrap();
        assert_eq!(
            proxy.session_mode_label(SessionMode::PerCommand),
            "per-command"
        );

        // Stateful mode
        let proxy = NntpProxy::new_sync(config.clone(), RoutingMode::Stateful).unwrap();
        assert_eq!(proxy.session_mode_label(SessionMode::Stateful), "standard");

        // Hybrid mode
        let proxy = NntpProxy::new_sync(config, RoutingMode::Hybrid).unwrap();
        assert_eq!(proxy.session_mode_label(SessionMode::Stateful), "hybrid");
    }

    #[test]
    fn test_finalize_stateful_session_success() {
        let config = create_test_config();
        let proxy = Arc::new(NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap());

        let client_addr =
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap());
        let session = proxy.create_session(client_addr, None);
        let session_id = NntpProxy::generate_session_id(&session);
        let backend_id = crate::types::BackendId::from_index(0);

        let metrics = TransferMetrics {
            client_to_backend: crate::types::ClientToBackendBytes::new(512),
            backend_to_client: crate::types::BackendToClientBytes::new(1024),
        };

        let result = proxy.finalize_stateful_session(
            Ok(metrics),
            client_addr,
            &session_id,
            &session,
            backend_id,
        );

        assert!(result.is_ok());
    }

    #[test]
    fn test_finalize_stateful_session_error() {
        let config = create_test_config();
        let proxy = Arc::new(NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap());

        let client_addr =
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap());
        let session = proxy.create_session(client_addr, None);
        let session_id = NntpProxy::generate_session_id(&session);
        let backend_id = crate::types::BackendId::from_index(0);

        let result = proxy.finalize_stateful_session(
            Err(SessionError::Backend(anyhow::anyhow!("connection error"))),
            client_addr,
            &session_id,
            &session,
            backend_id,
        );

        assert!(result.is_err());
    }

    #[test]
    fn test_finalize_per_command_session_success() {
        let config = create_test_config();
        let proxy = Arc::new(NntpProxy::new_sync(config, RoutingMode::PerCommand).unwrap());

        let client_addr =
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap());
        let session = proxy.create_session(client_addr, Some(proxy.router.clone()));
        let session_id = NntpProxy::generate_session_id(&session);

        let metrics = TransferMetrics {
            client_to_backend: crate::types::ClientToBackendBytes::new(256),
            backend_to_client: crate::types::BackendToClientBytes::new(512),
        };

        let result =
            proxy.finalize_per_command_session(Ok(metrics), client_addr, &session_id, &session);

        assert!(result.is_ok());
    }

    #[test]
    fn test_finalize_per_command_session_error() {
        let config = create_test_config();
        let proxy = Arc::new(NntpProxy::new_sync(config, RoutingMode::PerCommand).unwrap());

        let client_addr =
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap());
        let session = proxy.create_session(client_addr, Some(proxy.router.clone()));
        let session_id = NntpProxy::generate_session_id(&session);

        let result = proxy.finalize_per_command_session(
            Err(SessionError::Backend(anyhow::anyhow!("session failed"))),
            client_addr,
            &session_id,
            &session,
        );

        assert!(result.is_err());
    }

    #[test]
    fn test_record_session_metrics_without_backend() {
        let config = create_test_config();
        let proxy = Arc::new(NntpProxy::new_sync(config, RoutingMode::PerCommand).unwrap());

        let client_addr =
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap());
        let session = proxy.create_session(client_addr, Some(proxy.router.clone()));
        let session_id = NntpProxy::generate_session_id(&session);

        let metrics = TransferMetrics {
            client_to_backend: crate::types::ClientToBackendBytes::new(128),
            backend_to_client: crate::types::BackendToClientBytes::new(256),
        };

        // Without backend_id (per-command mode)
        let result =
            proxy.record_session_metrics(Ok(metrics), client_addr, &session_id, &session, None);

        assert!(result.is_ok());
    }

    #[test]
    fn test_generate_session_id_uniqueness() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();

        let client_addr =
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap());

        let session1 = proxy.create_session(client_addr, None);
        let session2 = proxy.create_session(client_addr, None);

        let id1 = NntpProxy::generate_session_id(&session1);
        let id2 = NntpProxy::generate_session_id(&session2);

        // Should generate different IDs for different sessions
        assert_ne!(id1, id2);
    }

    // Idle tracking tests

    #[test]
    fn test_per_backend_idle_timeout_defaults() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();

        // All test servers should have the default backend_idle_timeout (10 minutes)
        for server in proxy.servers() {
            assert_eq!(
                server.backend_idle_timeout,
                Duration::from_mins(10),
                "Server '{}' should have default 10-minute backend_idle_timeout",
                server.name.as_ref(),
            );
        }
    }

    #[test]
    fn test_active_clients_increment_decrement() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();

        assert_eq!(proxy.active_clients.load(Ordering::Relaxed), 0);

        proxy.increment_active_clients();
        assert_eq!(proxy.active_clients.load(Ordering::Relaxed), 1);

        proxy.increment_active_clients();
        assert_eq!(proxy.active_clients.load(Ordering::Relaxed), 2);

        proxy.decrement_active_clients();
        assert_eq!(proxy.active_clients.load(Ordering::Relaxed), 1);

        proxy.decrement_active_clients();
        assert_eq!(proxy.active_clients.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_last_activity_updated_on_last_client_disconnect() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();

        // Initially no activity
        assert_eq!(proxy.last_activity_nanos.load(Ordering::Relaxed), 0);

        // Connect two clients
        proxy.increment_active_clients();
        proxy.increment_active_clients();

        // First client disconnects - should not update timestamp
        proxy.decrement_active_clients();
        assert_eq!(proxy.last_activity_nanos.load(Ordering::Relaxed), 0);

        // Second (last) client disconnects - should update timestamp
        proxy.decrement_active_clients();
        assert!(proxy.last_activity_nanos.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn test_check_and_clear_skips_when_clients_active() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();

        // Simulate a past activity
        proxy.last_activity_nanos.store(1, Ordering::Relaxed);

        // With active clients, should not clear
        proxy.increment_active_clients();
        let cleared = proxy.check_and_clear_stale_pools();
        assert!(!cleared);
    }

    #[test]
    fn test_check_and_clear_skips_when_never_active() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();

        // No prior activity (last_activity_nanos = 0)
        let cleared = proxy.check_and_clear_stale_pools();
        assert!(!cleared);
    }

    #[test]
    fn test_check_and_clear_skips_when_recently_active() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();

        // Connect and disconnect to set timestamp
        proxy.increment_active_clients();
        proxy.decrement_active_clients();

        // Should not clear - just disconnected (within timeout)
        let cleared = proxy.check_and_clear_stale_pools();
        assert!(!cleared);
    }

    #[test]
    fn test_check_and_clear_clears_when_timeout_exceeded() {
        use crate::config::{Config, Server};
        use crate::types::{MaxConnections, Port};

        let config = Config {
            servers: vec![
                Server::builder("server1.example.com", Port::try_new(119).unwrap())
                    .name("Fast Timeout Server")
                    .max_connections(MaxConnections::try_new(2).unwrap())
                    .backend_idle_timeout(Duration::from_nanos(1)) // Expires immediately
                    .build()
                    .unwrap(),
            ],
            ..Default::default()
        };
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();

        // Store a very old last_activity (1ns from proxy start)
        // Any subsequent check will see idle_duration >> 1ns timeout
        proxy.last_activity_nanos.store(1, Ordering::Relaxed);

        // Give the clock at least 1ms to advance past the 1ns timeout
        std::thread::sleep(std::time::Duration::from_millis(1));

        let cleared = proxy.check_and_clear_stale_pools();
        assert!(cleared, "Should clear pools when idle timeout exceeded");
    }

    #[test]
    fn test_check_and_clear_per_backend_independent_short_timeout() {
        use crate::config::{Config, Server};
        use crate::types::{MaxConnections, Port};

        // Only one backend with a short timeout — should clear
        let config = Config {
            servers: vec![
                Server::builder("server1.example.com", Port::try_new(119).unwrap())
                    .name("Short Timeout")
                    .max_connections(MaxConnections::try_new(2).unwrap())
                    .backend_idle_timeout(Duration::from_nanos(1))
                    .build()
                    .unwrap(),
            ],
            ..Default::default()
        };
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();
        proxy.last_activity_nanos.store(1, Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(1));
        assert!(proxy.check_and_clear_stale_pools());
    }

    #[test]
    fn test_check_and_clear_per_backend_independent_long_timeout() {
        use crate::config::{Config, Server};
        use crate::types::{MaxConnections, Port};

        // Only one backend with a very long timeout — should NOT clear
        let config = Config {
            servers: vec![
                Server::builder("server2.example.com", Port::try_new(119).unwrap())
                    .name("Long Timeout")
                    .max_connections(MaxConnections::try_new(2).unwrap())
                    .backend_idle_timeout(Duration::from_hours(24)) // 24h
                    .build()
                    .unwrap(),
            ],
            ..Default::default()
        };
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();
        proxy.last_activity_nanos.store(1, Ordering::Relaxed);

        // Even though activity was long ago, the 24h timeout hasn't passed
        let cleared = proxy.check_and_clear_stale_pools();
        assert!(
            !cleared,
            "Should NOT clear pools when timeout not yet exceeded"
        );
    }

    #[test]
    fn test_check_and_clear_mixed_timeouts_returns_true_when_any_cleared() {
        use crate::config::{Config, Server};
        use crate::types::{MaxConnections, Port};

        // Two backends: one exceeded, one not — returns true (something was cleared)
        let config = Config {
            servers: vec![
                Server::builder("server1.example.com", Port::try_new(119).unwrap())
                    .name("Short Timeout")
                    .max_connections(MaxConnections::try_new(2).unwrap())
                    .backend_idle_timeout(Duration::from_nanos(1))
                    .build()
                    .unwrap(),
                Server::builder("server2.example.com", Port::try_new(119).unwrap())
                    .name("Long Timeout")
                    .max_connections(MaxConnections::try_new(2).unwrap())
                    .backend_idle_timeout(Duration::from_hours(24))
                    .build()
                    .unwrap(),
            ],
            ..Default::default()
        };
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();
        proxy.last_activity_nanos.store(1, Ordering::Relaxed);
        std::thread::sleep(std::time::Duration::from_millis(1));

        let cleared = proxy.check_and_clear_stale_pools();
        assert!(
            cleared,
            "Should return true when at least one backend exceeded its timeout"
        );
    }
}
