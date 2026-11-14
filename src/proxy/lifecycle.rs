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
use crate::pool::ConnectionProvider;
use crate::router;
use crate::session::ClientSession;
use crate::types::{self, ClientAddress, TransferMetrics};

use crate::session::SessionError;

use super::NntpProxy;

/// Bounds for how often unused backend connections are swept.
const IDLE_SWEEP_MIN_INTERVAL: Duration = Duration::from_secs(1);
const IDLE_SWEEP_MAX_INTERVAL: Duration = Duration::from_secs(60);

struct ClientConnectionGuard<'a>(&'a NntpProxy);

impl<'a> ClientConnectionGuard<'a> {
    fn new(proxy: &'a NntpProxy) -> Self {
        proxy.increment_active_clients();
        proxy.record_connection_opened();
        Self(proxy)
    }
}

impl Drop for ClientConnectionGuard<'_> {
    fn drop(&mut self) {
        self.0.record_connection_closed();
        self.0.decrement_active_clients();
    }
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

    /// Decrement active client count
    ///
    /// Call this when a client connection closes.
    #[inline]
    pub fn decrement_active_clients(&self) {
        self.active_clients.fetch_sub(1, Ordering::Relaxed);
    }

    /// Close backend connections that have sat unused longer than their
    /// server's `backend_idle_timeout`, and return how many were closed.
    ///
    /// This runs whether or not clients are connected. NZBGet and similar
    /// clients keep their own sockets open for minutes after a queue empties,
    /// and gating on "no clients" left every provider login open that long.
    /// A timeout of zero disables closing for that backend.
    pub(crate) fn close_unused_backend_connections(&self) -> usize {
        let mut closed_by_backend = Vec::new();
        for (server, provider) in self.servers.iter().zip(&self.connection_providers) {
            if server.backend_idle_timeout.is_zero() {
                continue;
            }
            let closed = provider.close_unused_connections(server.backend_idle_timeout);
            if closed > 0 {
                closed_by_backend.push((server.name.as_ref().to_string(), closed));
            }
        }
        let total = closed_by_backend.iter().map(|(_, n)| n).sum();
        if total > 0 {
            info!(
                closed = total,
                backends = ?closed_by_backend,
                "Closed unused backend connections"
            );
        }
        total
    }

    /// How often the idle sweep runs: half the shortest idle timeout, so a
    /// connection closes within 1.5x its timeout, bounded to [1 s, 60 s].
    /// Backends with closing disabled (zero) don't count.
    pub(crate) fn idle_sweep_interval(&self) -> Duration {
        let shortest = self
            .servers
            .iter()
            .map(|server| server.backend_idle_timeout)
            .filter(|timeout| !timeout.is_zero())
            .min()
            .unwrap_or(IDLE_SWEEP_MAX_INTERVAL);
        (shortest / 2).clamp(IDLE_SWEEP_MIN_INTERVAL, IDLE_SWEEP_MAX_INTERVAL)
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
        .with_adaptive_precheck(self.adaptive_precheck)
        .with_reader_message_id_failover(self.reader_message_id_failover);

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
        metrics: &types::TransferMetrics,
    ) {
        debug!(
            mode = ?session.mode(),
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
        &self,
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
    ) -> Result<(crate::types::BackendId, crate::router::CommandGuard)> {
        let client_id = types::ClientId::new();
        let backend_id = self.router.route_stateful_session(client_id)?;
        let reservation = crate::router::BackendSelector::guard_for_routed_backend(
            Arc::clone(&self.router),
            backend_id,
        );
        let server_idx = backend_id.as_index();

        self.log_routing_selection(client_addr, backend_id, &self.servers[server_idx]);
        crate::protocol::send_proxy_greeting(client_stream, client_addr).await?;
        self.log_pool_status(server_idx);
        self.apply_tcp_optimizations(client_stream);

        Ok((backend_id, reservation))
    }

    /// Prepare per-command connection - record, greet, optimize
    pub(super) async fn prepare_per_command_connection(
        &self,
        client_stream: &mut TcpStream,
        client_addr: ClientAddress,
    ) -> Result<()> {
        crate::protocol::send_proxy_greeting(client_stream, client_addr).await?;
        self.apply_tcp_optimizations(client_stream);
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

    /// Apply TCP optimizations to client socket
    #[inline]
    pub(super) fn apply_tcp_optimizations(&self, client_stream: &TcpStream) {
        crate::network::optimize_tcp_stream(
            client_stream,
            self.memory.socket_recv_buffer_size,
            self.memory.socket_send_buffer_size,
        )
        .map_err(|e| debug!("Failed to optimize client socket: {}", e))
        .ok();
    }

    /// Get display name for current routing mode
    #[inline]
    pub(super) fn routing_mode_display_name(&self) -> &'static str {
        self.routing_mode.short_name()
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
        self.record_session_metrics(metrics, client_addr, session_id, session, Some(backend_id))
    }

    /// Finalize per-command session with logging and cleanup
    pub(super) fn finalize_per_command_session(
        &self,
        metrics: Result<TransferMetrics, SessionError>,
        client_addr: ClientAddress,
        session_id: &str,
        session: &ClientSession,
    ) -> Result<(), SessionError> {
        self.record_session_metrics(metrics, client_addr, session_id, session, None)
    }

    /// Record connection for unauthenticated sessions only
    #[inline]
    pub(super) fn record_connection_if_unauthenticated(&self, session: &ClientSession) {
        if !self.auth_handler.is_enabled() || session.username().is_none() {
            let mode = self.session_mode_label(session.mode());
            self.connection_stats.record_session_connection(
                session.client_id(),
                session.username(),
                mode,
            );
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
                // Byte totals were already reported by the session loops:
                // per-backend bytes as they flowed, client totals on exit.
                self.log_session_completion(client_addr, session_id, session, &m);
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
            (SessionMode::Stateful, RoutingMode::Stateful) => "stateful",
            (SessionMode::Stateful, RoutingMode::Hybrid) => "hybrid",
            (SessionMode::Stateful, _) => "stateful",
        }
    }

    pub async fn handle_client(
        &self,
        mut client_stream: TcpStream,
        client_addr: ClientAddress,
    ) -> Result<(), SessionError> {
        debug!("New client connection from {}", client_addr);

        let _connection_guard = ClientConnectionGuard::new(self);

        async {
            let (backend_id, _backend_reservation) = self
                .prepare_stateful_connection(&mut client_stream, client_addr)
                .await
                .map_err(SessionError::from)?;
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
        .await
    }

    /// Handle client connection using per-command routing mode
    ///
    /// This creates a session with the router, allowing commands from this client
    /// to be routed to different backends based on load balancing.
    /// The boxed entry-point future keeps instrumentation wrappers from expanding
    /// the complete session state machine into their own generated future types.
    pub fn handle_client_per_command_routing(
        &self,
        client_stream: TcpStream,
        client_addr: ClientAddress,
    ) -> futures::future::BoxFuture<'_, Result<(), SessionError>> {
        Box::pin(async move {
            let _connection_guard = ClientConnectionGuard::new(self);

            self.handle_per_command_client(client_stream, client_addr)
                .await
        })
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
            .map_err(SessionError::from)?;

        let mut session = self.create_session(client_addr, Some(self.router.clone()));
        let session_id = Self::generate_session_id(&session);

        let metrics = session.handle_per_command_routing(client_stream).await;

        self.finalize_per_command_session(metrics, client_addr, &session_id, &session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RoutingMode;
    use crate::session::SessionMode;
    use std::sync::Arc;
    use std::time::Duration;

    async fn spawn_mock_backend() -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    if stream
                        .write_all(b"200 Mock Backend Ready\r\n")
                        .await
                        .is_err()
                    {
                        return;
                    }

                    let mut buffer = [0u8; 1024];
                    loop {
                        let n = match stream.read(&mut buffer).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };

                        let cmd = String::from_utf8_lossy(&buffer[..n]).to_uppercase();
                        if cmd.starts_with("DATE") {
                            let _ = stream.write_all(b"111 20260101000000\r\n").await;
                        } else if cmd.starts_with("QUIT") {
                            let _ = stream.write_all(b"205 Goodbye\r\n").await;
                            break;
                        } else {
                            let _ = stream.write_all(b"200 OK\r\n").await;
                        }
                    }
                });
            }
        });

        addr
    }

    fn create_test_config() -> crate::config::Config {
        super::super::tests::create_test_config()
    }

    #[tokio::test]
    async fn cancelled_handlers_release_both_connection_counters() {
        for mode in [
            RoutingMode::Stateful,
            RoutingMode::PerCommand,
            RoutingMode::Hybrid,
        ] {
            let proxy = NntpProxy::new_sync(create_test_config(), mode).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = TcpStream::connect(listener.local_addr().unwrap())
                .await
                .unwrap();
            let (stream, addr) = listener.accept().await.unwrap();
            let mut handler = Box::pin(async {
                match mode {
                    RoutingMode::Stateful => proxy.handle_client(stream, addr.into()).await,
                    _ => {
                        proxy
                            .handle_client_per_command_routing(stream, addr.into())
                            .await
                    }
                }
            });
            assert!(futures::poll!(&mut handler).is_pending());
            assert_eq!(proxy.active_clients.load(Ordering::Relaxed), 1);
            drop(handler);
            drop(client);
            assert_eq!(proxy.active_clients.load(Ordering::Relaxed), 0);
            assert_eq!(proxy.metrics.snapshot(None).active_connections.get(), 0);
        }
    }

    #[test]
    fn errored_sessions_release_aggregator_bookkeeping_on_drop() {
        let proxy = NntpProxy::new_sync(create_test_config(), RoutingMode::PerCommand).unwrap();
        let addr = ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap());
        for error in [
            SessionError::Backend(anyhow::anyhow!("test")),
            SessionError::ClientDisconnect(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
        ] {
            let session = proxy.create_session(addr, Some(proxy.router.clone()));
            proxy.record_connection_if_unauthenticated(&session);
            assert_eq!(proxy.connection_stats.tracked_session_count(), 1);
            let result = proxy.finalize_per_command_session(Err(error), addr, "session", &session);
            assert!(result.is_err());
            drop(session);
            assert_eq!(proxy.connection_stats.tracked_session_count(), 0);
        }
    }

    #[test]
    fn test_session_mode_label_per_command() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::PerCommand).unwrap();

        let label = proxy.session_mode_label(SessionMode::PerCommand);
        assert_eq!(label, "per-command");
    }

    #[test]
    fn test_session_mode_label_stateful() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();

        let label = proxy.session_mode_label(SessionMode::Stateful);
        assert_eq!(label, "stateful");
    }

    #[test]
    fn test_session_mode_label_stateful_hybrid() {
        let config = create_test_config();
        let proxy = NntpProxy::new_sync(config, RoutingMode::Hybrid).unwrap();

        let label = proxy.session_mode_label(SessionMode::Stateful);
        assert_eq!(label, "hybrid");
    }

    #[test]
    fn test_routing_mode_display_name_matches_configured_mode() {
        let config = create_test_config();

        let per_command = NntpProxy::new_sync(config.clone(), RoutingMode::PerCommand).unwrap();
        assert_eq!(per_command.routing_mode_display_name(), "per-command");

        let hybrid = NntpProxy::new_sync(config.clone(), RoutingMode::Hybrid).unwrap();
        assert_eq!(hybrid.routing_mode_display_name(), "hybrid");

        let stateful = NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap();
        assert_eq!(stateful.routing_mode_display_name(), "stateful");
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

        let _empty_cache = Arc::new(crate::cache::UnifiedCache::availability(
            std::time::Duration::MAX,
        ));
        assert_eq!(proxy.routing_mode_display_name(), "hybrid");
    }

    #[test]
    fn test_log_routing_selection() {
        let config = create_test_config();
        let proxy = Arc::new(NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap());

        let backend_id = crate::types::BackendId::from_index(0);
        let client_addr =
            ClientAddress::from("127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap());

        // Should not panic
        proxy.log_routing_selection(client_addr, backend_id, &proxy.servers()[0]);
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
        let proxy = Arc::new(NntpProxy::new(config, RoutingMode::Stateful).await.unwrap());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
        });

        let stream = tokio::net::TcpStream::connect(addr).await.unwrap();

        // Should not panic
        proxy.apply_tcp_optimizations(&stream);
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
        assert_eq!(proxy.session_mode_label(SessionMode::Stateful), "stateful");

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
                Duration::from_secs(600),
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

    fn proxy_with_idle_timeouts(backend: std::net::SocketAddr, timeouts: &[Duration]) -> NntpProxy {
        use crate::config::{Config, Server};
        use crate::types::{MaxConnections, Port};

        let servers = timeouts
            .iter()
            .enumerate()
            .map(|(i, timeout)| {
                Server::builder("127.0.0.1", Port::try_new(backend.port()).unwrap())
                    .name(format!("backend-{i}"))
                    .max_connections(MaxConnections::try_new(2).unwrap())
                    .prewarm_connections(2)
                    .backend_idle_timeout(*timeout)
                    .build()
                    .unwrap()
            })
            .collect();
        let config = Config {
            servers,
            ..Default::default()
        };
        NntpProxy::new_sync(config, RoutingMode::Stateful).unwrap()
    }

    /// A connected client must not keep unused provider logins open: NZBGet
    /// holds its sockets for minutes after its queue empties.
    #[tokio::test]
    async fn unused_backend_connections_close_while_a_client_is_connected() {
        let backend = spawn_mock_backend().await;
        let proxy = proxy_with_idle_timeouts(backend, &[Duration::from_secs(30)]);
        proxy.prewarm_connections().await.unwrap();
        proxy.increment_active_clients();

        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(29)).await;
        assert_eq!(proxy.close_unused_backend_connections(), 0);

        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(proxy.close_unused_backend_connections(), 2);
        assert_eq!(proxy.connection_providers[0].status().created.get(), 0);
    }

    /// Each backend closes against its own timeout.
    #[tokio::test]
    async fn each_backend_closes_against_its_own_idle_timeout() {
        let backend = spawn_mock_backend().await;
        let proxy = proxy_with_idle_timeouts(
            backend,
            &[Duration::from_secs(30), Duration::from_secs(24 * 60 * 60)],
        );
        proxy.prewarm_connections().await.unwrap();

        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(31)).await;
        assert_eq!(proxy.close_unused_backend_connections(), 2);
        assert_eq!(proxy.connection_providers[0].status().created.get(), 0);
        assert_eq!(proxy.connection_providers[1].status().created.get(), 2);
    }

    #[test]
    fn idle_sweep_runs_at_half_the_shortest_timeout_within_bounds() {
        let addr = "127.0.0.1:119".parse().unwrap();
        let interval =
            |timeouts: &[Duration]| proxy_with_idle_timeouts(addr, timeouts).idle_sweep_interval();
        assert_eq!(
            interval(&[Duration::from_secs(30), Duration::from_secs(600)]),
            Duration::from_secs(15)
        );
        assert_eq!(
            interval(&[Duration::from_secs(600)]),
            Duration::from_secs(60)
        );
        assert_eq!(
            interval(&[Duration::from_millis(10)]),
            Duration::from_secs(1)
        );
        assert_eq!(
            interval(&[Duration::ZERO, Duration::from_secs(30)]),
            Duration::from_secs(15)
        );
    }

    /// `backend_idle_timeout = 0` disables closing, as documented, rather
    /// than closing every connection between exchanges.
    #[tokio::test]
    async fn zero_idle_timeout_never_closes_connections() {
        let backend = spawn_mock_backend().await;
        let proxy = proxy_with_idle_timeouts(backend, &[Duration::ZERO]);
        proxy.prewarm_connections().await.unwrap();

        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(24 * 60 * 60)).await;
        assert_eq!(proxy.close_unused_backend_connections(), 0);
        assert_eq!(proxy.connection_providers[0].status().created.get(), 2);
    }
}
