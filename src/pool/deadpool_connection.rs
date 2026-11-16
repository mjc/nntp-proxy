//! Core TCP connection manager for deadpool
//!
//! This module provides the `TcpManager` struct which handles the low-level
//! creation of optimized TCP/TLS connections to NNTP servers.

use deadpool::managed;
use hickory_resolver::TokioResolver;
use hickory_resolver::config::{ResolveHosts, ResolverConfig};
use hickory_resolver::net::NetError;
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hickory_resolver::proto::rr::RecordType;
use smallvec::SmallVec;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::OnceCell;

use crate::connection_error::ConnectionError;
use crate::pool::address_balance::AddressLease;
use crate::pool::churn::{CloseReason, ConnectOutcome};
use crate::pool::login_backoff::{Admission, LoginBackoff};
use crate::protocol::{RequestContext, authinfo_pass, authinfo_user};
use crate::stream::ConnectionStream;
use crate::tls::{TlsConfig, TlsManager, TlsPolicy};

/// A backend connection that has completed all setup negotiations.
///
/// The last field is when the connection last finished useful work (setup or a
/// completed exchange). Deadpool's own `last_used` metric restarts at every
/// checkout, including health-check checkouts, so it cannot tell how long a
/// connection has really sat unused.
#[derive(Debug)]
pub struct ReadyBackendConnection(ConnectionStream, CloseTracking, tokio::time::Instant);

/// Counts this connection's close in its backend's churn counters when the
/// object is destroyed, under the first reason anyone tagged it with.
#[derive(Debug)]
struct CloseTracking {
    churn: Arc<crate::pool::churn::ConnectionChurn>,
    reason: Option<CloseReason>,
    /// Released when the connection is destroyed, freeing its address slot.
    _address: Option<crate::pool::address_balance::AddressLease>,
}

impl Drop for CloseTracking {
    fn drop(&mut self) {
        self.churn
            .note_close(self.reason.unwrap_or(CloseReason::Idle));
    }
}

impl ReadyBackendConnection {
    fn new(
        stream: ConnectionStream,
        churn: Arc<crate::pool::churn::ConnectionChurn>,
        address: Option<crate::pool::address_balance::AddressLease>,
    ) -> Self {
        Self(
            stream,
            CloseTracking {
                churn,
                reason: None,
                _address: address,
            },
            tokio::time::Instant::now(),
        )
    }

    /// Note that a client exchange just completed on this connection.
    pub(crate) fn mark_used(&mut self) {
        self.2 = tokio::time::Instant::now();
    }

    /// How long this connection has gone without a completed exchange.
    pub(crate) fn unused_for(&self, now: tokio::time::Instant) -> Duration {
        now.saturating_duration_since(self.2)
    }

    /// Record why this connection is being retired. Later tags are ignored so
    /// the cause that started the retirement is the one counted.
    pub(crate) const fn tag_close_reason(&mut self, reason: CloseReason) {
        if self.1.reason.is_none() {
            self.1.reason = Some(reason);
        }
    }
}

impl std::ops::Deref for ReadyBackendConnection {
    type Target = ConnectionStream;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ReadyBackendConnection {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl tokio::io::AsyncRead for ReadyBackendConnection {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for ReadyBackendConnection {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
    }
}

/// Type alias for the deadpool connection pool
pub type Pool = managed::Pool<TcpManager>;
pub(crate) type PooledConnection = managed::Object<TcpManager>;

/// Build a pool whose wait, create, and recycle deadlines match connection setup.
///
/// Deadpool reports those deadlines as distinct timeout kinds. Callers can tell
/// a full pool from a setup timeout instead of treating every failure as an
/// empty pool.
pub(crate) fn configured_pool(manager: TcpManager, max_size: usize) -> Pool {
    let limit = crate::constants::timeout::CONNECTION;
    Pool::builder(manager)
        .max_size(max_size)
        .timeouts(managed::Timeouts {
            wait: Some(limit),
            create: Some(limit),
            recycle: Some(limit),
        })
        .runtime(deadpool::Runtime::Tokio1)
        .build()
        .expect("Failed to create connection pool")
}

/// Shared DNS resolver. The hosts-file fallback is tried against
/// `/etc/resolv.conf` once more, then left in place.
struct CachedResolver {
    resolver: Arc<TokioResolver>,
    kind: CacheKind,
}

#[derive(Clone, Copy)]
enum CacheKind {
    System,
    HostsPending(Instant),
    HostsFinal,
}

static DNS_RESOLVER: Mutex<Option<CachedResolver>> = Mutex::new(None);
static NEXT_IPV4_ROTATION: AtomicUsize = AtomicUsize::new(0);

const RESOLV_CONF_RETRY: Duration = Duration::from_secs(5 * 60);

/// Return the DNS resolver for this lookup.
///
/// Hickory reads `/etc/resolv.conf` for upstream nameservers. When that file
/// is missing, the lookup uses the system hosts file. Five minutes later one
/// lookup tries the system file again. If it is still missing, later lookups
/// stay on the hosts file. A resolver built from `resolv.conf` stays for the
/// process.
fn dns_resolver() -> Result<Arc<TokioResolver>, ConnectionError> {
    if let Some(resolver) = cached_resolver() {
        return Ok(resolver);
    }

    match TokioResolver::builder_tokio() {
        Ok(builder) => {
            let resolver = Arc::new(
                builder
                    .build()
                    .map_err(|error| ConnectionError::dns_resolver_build("global", error))?,
            );
            Ok(store_system_resolver(resolver))
        }
        Err(error) if system_resolv_conf_missing(&error) => {
            let resolver = Arc::new(hosts_file_resolver()?);
            Ok(store_hosts_resolver(resolver))
        }
        Err(error) => Err(ConnectionError::dns_resolver_init("global", error)),
    }
}

fn cached_resolver() -> Option<Arc<TokioResolver>> {
    let cache = resolver_cache();
    let cached = cache.as_ref()?;
    if matches!(cached.kind, CacheKind::HostsPending(retry_at) if Instant::now() >= retry_at) {
        return None;
    }
    Some(Arc::clone(&cached.resolver))
}

fn store_system_resolver(resolver: Arc<TokioResolver>) -> Arc<TokioResolver> {
    let mut cache = resolver_cache();
    if let Some(existing) = cache
        .as_ref()
        .filter(|cached| matches!(cached.kind, CacheKind::System))
    {
        return Arc::clone(&existing.resolver);
    }
    *cache = Some(CachedResolver {
        resolver: Arc::clone(&resolver),
        kind: CacheKind::System,
    });
    resolver
}

fn store_hosts_resolver(resolver: Arc<TokioResolver>) -> Arc<TokioResolver> {
    let mut cache = resolver_cache();
    let now = Instant::now();
    let existing = cache.as_ref().map(|cached| cached.kind);
    let keep_existing = match existing {
        Some(CacheKind::System | CacheKind::HostsFinal) => true,
        Some(CacheKind::HostsPending(retry_at)) => now < retry_at,
        None => false,
    };
    if keep_existing {
        return Arc::clone(
            &cache
                .as_ref()
                .expect("cache entry was just matched")
                .resolver,
        );
    }
    *cache = Some(CachedResolver {
        resolver: Arc::clone(&resolver),
        kind: hosts_cache_after_miss(existing, now, RESOLV_CONF_RETRY),
    });
    resolver
}

fn hosts_cache_after_miss(existing: Option<CacheKind>, now: Instant, delay: Duration) -> CacheKind {
    match existing {
        Some(CacheKind::HostsPending(retry_at)) if now >= retry_at => CacheKind::HostsFinal,
        Some(CacheKind::HostsPending(retry_at)) => CacheKind::HostsPending(retry_at),
        Some(CacheKind::HostsFinal) => CacheKind::HostsFinal,
        Some(CacheKind::System) | None => CacheKind::HostsPending(now + delay),
    }
}

fn resolver_cache() -> std::sync::MutexGuard<'static, Option<CachedResolver>> {
    DNS_RESOLVER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn system_resolv_conf_missing(error: &NetError) -> bool {
    matches!(
        error,
        NetError::Io(io_error) if io_error.kind() == io::ErrorKind::NotFound
    )
}

fn hosts_file_resolver() -> Result<TokioResolver, ConnectionError> {
    let mut builder = TokioResolver::builder_with_config(
        ResolverConfig::from_name_servers(Vec::new()),
        TokioRuntimeProvider::default(),
    );
    builder.options_mut().use_hosts_file = ResolveHosts::Always;
    builder
        .build()
        .map_err(|error| ConnectionError::dns_resolver_build("global", error))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompressionSupport {
    Supported,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompressionMode {
    Disabled,
    Auto,
    Required,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompressionPolicy {
    mode: CompressionMode,
    level: u32,
}

impl CompressionPolicy {
    fn from_raw(mode: Option<bool>, level: Option<u32>) -> Result<Self, ConnectionError> {
        let level = level.unwrap_or(1);
        if level > 9 {
            return Err(ConnectionError::IoError(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("compression level {level} is outside the supported 0-9 range"),
            )));
        }

        Ok(Self {
            mode: match mode {
                Some(false) => CompressionMode::Disabled,
                Some(true) => CompressionMode::Required,
                None => CompressionMode::Auto,
            },
            level,
        })
    }
}

/// Optional settings for [`TcpManager`] construction
///
/// Groups optional parameters (credentials, TLS, compression) to keep
/// the `TcpManager::new()` signature concise.
#[derive(Clone)]
pub struct TcpManagerOptions {
    pub username: Option<String>,
    pub password: Option<String>,
    pub tls_config: Option<TlsConfig>,
    /// TCP receive buffer size for this connection.
    pub recv_buffer_size: usize,
    /// TCP send buffer size for this connection.
    pub send_buffer_size: usize,
    /// Wire compression mode: None = auto-detect, Some(true) = require, Some(false) = disable
    pub compress: Option<bool>,
    /// Compression level (0-9). None = fast (level 1).
    pub compress_level: Option<u32>,
    /// Send MODE READER to the backend after authentication.
    ///
    /// RFC 3977 §5.3: MODE READER switches a transit server to reading mode.
    /// Defaults to `true` — required for reader-capable backends.
    pub send_mode_reader: bool,
}

/// Hand-written so the backend account name and password never reach `Debug`
/// output. The destructuring makes a new field a compile error here.
impl std::fmt::Debug for TcpManagerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::config::redact;
        let Self {
            username,
            password,
            tls_config,
            recv_buffer_size,
            send_buffer_size,
            compress,
            compress_level,
            send_mode_reader,
        } = self;
        f.debug_struct("TcpManagerOptions")
            .field("username", &redact(username.as_ref()))
            .field("password", &redact(password.as_ref()))
            .field("tls_config", tls_config)
            .field("recv_buffer_size", recv_buffer_size)
            .field("send_buffer_size", send_buffer_size)
            .field("compress", compress)
            .field("compress_level", compress_level)
            .field("send_mode_reader", send_mode_reader)
            .finish()
    }
}

impl Default for TcpManagerOptions {
    fn default() -> Self {
        Self {
            username: None,
            password: None,
            tls_config: None,
            recv_buffer_size: crate::constants::socket::HIGH_THROUGHPUT_RECV_BUFFER,
            send_buffer_size: crate::constants::socket::HIGH_THROUGHPUT_SEND_BUFFER,
            compress: None,
            compress_level: None,
            send_mode_reader: true,
        }
    }
}

/// TCP connection manager for deadpool with cached TLS config
#[derive(Clone)]
pub struct TcpManager {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) name: String,
    pub(crate) username: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) tls_policy: TlsPolicy,
    /// Cached TLS manager with pre-loaded certificates (avoids base64 decode overhead)
    pub(crate) tls_manager: Option<Arc<TlsManager>>,
    pub(crate) recv_buffer_size: usize,
    pub(crate) send_buffer_size: usize,
    compression: CompressionPolicy,
    compression_support: Arc<OnceCell<CompressionSupport>>,
    /// Whether to send MODE READER after authentication (RFC 3977 §5.3)
    pub(crate) send_mode_reader: bool,
    /// Stops repeated logins while the backend keeps refusing them.
    login_backoff: Arc<LoginBackoff>,
    /// Connection births and deaths, shared with the provider for export.
    pub(crate) churn: Arc<crate::pool::churn::ConnectionChurn>,
    /// Live connections per resolved address, so new ones go to the emptiest.
    address_load: Arc<crate::pool::address_balance::AddressLoad>,
    /// Rate-limited operator events for connection setup failures.
    events: Arc<SetupEvents>,
}

/// Why a new backend session could not be set up, as operators act on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupFailure {
    /// Credentials or the login exchange were rejected after `AUTHINFO`.
    LoginRefused,
    /// The account is at its provider-side connection limit.
    ConnectionLimit,
    /// The backend answered but refused service before login: a non-2xx
    /// greeting such as `400` (unavailable, too many connections) or `502`.
    /// Usually transient or load-related, so it is not reported as a
    /// credential problem.
    Unavailable,
    /// `MODE READER` refused after a successful login: an account or address
    /// permission problem, not a transient one.
    ReaderModeRefused,
    /// Any other refusal at `AUTHINFO`; logged with the reply text.
    LoginRejected,
    /// DNS, TCP, TLS, or a timeout: the backend could not be reached.
    Unreachable,
    /// Reached and accepted, but the session cannot be used as configured
    /// (a required password or compression is missing).
    Misconfigured,
}

impl SetupFailure {
    /// Refusals use the same classification as the login backoff.
    fn of(error: &ConnectionError) -> Self {
        use crate::connection_error::LoginRefusal;
        match error.login_refusal() {
            Some(LoginRefusal::Credentials) => Self::LoginRefused,
            Some(LoginRefusal::ConnectionLimit) => Self::ConnectionLimit,
            Some(LoginRefusal::ServiceUnavailable) => Self::Unavailable,
            Some(LoginRefusal::ReaderModeRefused) => Self::ReaderModeRefused,
            Some(LoginRefusal::Rejected) => Self::LoginRejected,
            None => match error {
                ConnectionError::PasswordRequired { .. }
                | ConnectionError::CompressionRequired { .. } => Self::Misconfigured,
                _ => Self::Unreachable,
            },
        }
    }
}

/// One [`EventGate`](crate::logging::EventGate) per setup failure kind.
///
/// Shared by every clone of the manager so one backend reports each
/// condition at most about once a minute, with a count, and once when
/// sessions set up again with no failure for a full window.
#[derive(Debug, Default)]
struct SetupEvents {
    login_refused: crate::logging::EventGate,
    connection_limit: crate::logging::EventGate,
    unavailable: crate::logging::EventGate,
    reader_mode_refused: crate::logging::EventGate,
    login_rejected: crate::logging::EventGate,
    unreachable: crate::logging::EventGate,
    misconfigured: crate::logging::EventGate,
}

impl SetupEvents {
    fn failed(&self, manager: &TcpManager, error: &ConnectionError, retry_in: Duration) {
        let backend = manager.name.as_str();
        let host = manager.host.as_str();
        let port = manager.port;
        let retry_in_secs = retry_in.as_secs();
        match SetupFailure::of(error) {
            SetupFailure::LoginRefused => {
                if let Some(count) = self.login_refused.record() {
                    tracing::error!(
                        event = "backend_login_refused",
                        backend,
                        host,
                        port,
                        count,
                        retry_in_secs,
                        error = %error,
                        "Backend refused login"
                    );
                }
            }
            SetupFailure::ConnectionLimit => {
                if let Some(count) = self.connection_limit.record() {
                    tracing::warn!(
                        event = "backend_connection_limit",
                        backend,
                        host,
                        port,
                        count,
                        retry_in_secs,
                        error = %error,
                        "Backend account connection limit reached"
                    );
                }
            }
            SetupFailure::Unavailable => {
                if let Some(count) = self.unavailable.record() {
                    tracing::warn!(
                        event = "backend_unavailable",
                        backend,
                        host,
                        port,
                        count,
                        retry_in_secs,
                        error = %error,
                        "Backend refused service before login"
                    );
                }
            }
            SetupFailure::ReaderModeRefused => {
                if let Some(count) = self.reader_mode_refused.record() {
                    tracing::error!(
                        event = "backend_reader_mode_refused",
                        backend,
                        host,
                        port,
                        count,
                        retry_in_secs,
                        error = %error,
                        "Backend refused MODE READER after login"
                    );
                }
            }
            SetupFailure::LoginRejected => {
                if let Some(count) = self.login_rejected.record() {
                    tracing::warn!(
                        event = "backend_login_rejected",
                        backend,
                        host,
                        port,
                        count,
                        retry_in_secs,
                        error = %error,
                        "Backend rejected login"
                    );
                }
            }
            SetupFailure::Unreachable => {
                if let Some(count) = self.unreachable.record() {
                    tracing::warn!(
                        event = "backend_unreachable",
                        backend,
                        host,
                        port,
                        count,
                        error = %error,
                        "Backend unreachable"
                    );
                }
            }
            SetupFailure::Misconfigured => {
                if let Some(count) = self.misconfigured.record() {
                    tracing::error!(
                        event = "backend_misconfigured",
                        backend,
                        host,
                        port,
                        count,
                        error = %error,
                        "Backend session cannot be set up as configured"
                    );
                }
            }
        }
    }

    fn gates(&self) -> [(&crate::logging::EventGate, &'static str); 7] {
        [
            (&self.login_refused, "backend_login_refused_cleared"),
            (&self.connection_limit, "backend_connection_limit_cleared"),
            (&self.unavailable, "backend_unavailable_cleared"),
            (
                &self.reader_mode_refused,
                "backend_reader_mode_refused_cleared",
            ),
            (&self.login_rejected, "backend_login_rejected_cleared"),
            (&self.unreachable, "backend_unreachable_cleared"),
            (&self.misconfigured, "backend_misconfigured_cleared"),
        ]
    }

    /// A new session connected and logged in: evidence against every
    /// condition. Only a successful login proves the account is accepted
    /// again, so this is the only success the login-type conditions see.
    fn login_succeeded(&self) {
        for (gate, _) in self.gates() {
            gate.note_success();
        }
    }

    /// Established connections passed a health check: evidence that the
    /// backend is reachable, but not that a new login would be accepted.
    fn connections_healthy(&self) {
        self.unreachable.note_success();
    }

    /// End every condition that has seen a success since its last failure
    /// and a full window without failing. Polled by a small task per
    /// backend, so it runs whether or not the pool has idle connections or
    /// creates new ones; with nothing open it is one relaxed load per gate.
    fn settle(&self, backend: &str) {
        for (gate, event) in self.gates() {
            if let Some(cleared) = gate.clear_if_recovered() {
                tracing::info!(
                    event,
                    backend,
                    occurrences = cleared.occurrences,
                    duration_secs = cleared.duration.as_secs(),
                    "Backend connection setup failures cleared"
                );
            }
        }
    }
}

/// How often each backend checks whether its setup-failure episodes have
/// recovered. Well under the one-minute quiet window.
const SETUP_EVENT_SETTLE_INTERVAL: Duration = Duration::from_secs(15);

/// Poll [`SetupEvents::settle`] until the manager (and with it the events)
/// is dropped. Holds only a weak reference so it never keeps a pool alive.
/// Without a runtime (synchronous construction) episodes settle on the next
/// successful login instead.
fn spawn_setup_event_settler(events: &Arc<SetupEvents>, backend: String) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let events = Arc::downgrade(events);
    runtime.spawn(async move {
        let mut interval = tokio::time::interval(SETUP_EVENT_SETTLE_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let Some(events) = events.upgrade() else {
                break;
            };
            events.settle(&backend);
        }
    });
}

/// Hand-written so the backend account name and password never reach `Debug`
/// output (the pool and provider `Debug` print the manager). The
/// destructuring makes a new field a compile error here.
impl std::fmt::Debug for TcpManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use crate::config::redact;
        let Self {
            host,
            port,
            name,
            username,
            password,
            tls_policy,
            tls_manager,
            recv_buffer_size,
            send_buffer_size,
            compression,
            compression_support,
            send_mode_reader,
            login_backoff,
            churn,
            address_load,
            events,
        } = self;
        f.debug_struct("TcpManager")
            .field("host", host)
            .field("port", port)
            .field("name", name)
            .field("username", &redact(username.as_ref()))
            .field("password", &redact(password.as_ref()))
            .field("tls_policy", tls_policy)
            .field("tls_manager", tls_manager)
            .field("recv_buffer_size", recv_buffer_size)
            .field("send_buffer_size", send_buffer_size)
            .field("compression", compression)
            .field("compression_support", compression_support)
            .field("send_mode_reader", send_mode_reader)
            .field("login_backoff", login_backoff)
            .field("churn", churn)
            .field("address_load", address_load)
            .field("events", events)
            .finish()
    }
}

impl TcpManager {
    /// A health-check cycle found working established connections; see
    /// [`SetupEvents::connections_healthy`].
    pub(crate) fn note_healthy_connections(&self) {
        self.events.connections_healthy();
    }

    /// Report a setup failure observed outside [`managed::Manager::create`].
    pub(crate) fn report_setup_failure(&self, error: &ConnectionError) {
        // `create` was cut off before it could count the attempt.
        self.churn.note_connect(ConnectOutcome::Failed);
        self.events.failed(self, error, Duration::ZERO);
    }

    /// Whether login backoff is currently keeping new connections away.
    pub(crate) fn login_backoff_active(&self) -> bool {
        self.login_backoff.is_waiting()
    }

    fn socket_buffer_size_u32(size: usize, label: &str) -> Result<u32, ConnectionError> {
        u32::try_from(size).map_err(|_| {
            ConnectionError::IoError(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{label} socket buffer size {size} exceeds u32::MAX"),
            ))
        })
    }

    async fn lookup_socket_addrs(&self) -> Result<SmallVec<[SocketAddr; 4]>, ConnectionError> {
        if let Ok(ip_addr) = self.host.parse::<IpAddr>() {
            let mut addrs = SmallVec::new();
            addrs.push(SocketAddr::new(ip_addr, self.port));
            return Ok(addrs);
        }
        let resolver = dns_resolver()?;
        let lookup = resolver
            .lookup_ip(self.host.as_str())
            .await
            .map_err(|error| {
                ConnectionError::dns_lookup(
                    &self.name,
                    format!("{}:{}", self.host, self.port),
                    error,
                )
            })?;
        let mut addrs: SmallVec<[SocketAddr; 4]> = SmallVec::new();
        addrs.extend(
            lookup
                .iter()
                .map(|ip_addr| SocketAddr::new(ip_addr, self.port)),
        );
        if addrs.is_empty() {
            return Err(ConnectionError::DnsNoAddresses {
                address: format!("{}:{}", self.host, self.port),
            });
        }

        let ipv4_positions: SmallVec<[usize; 4]> = addrs
            .iter()
            .enumerate()
            .filter_map(|(idx, addr)| addr.is_ipv4().then_some(idx))
            .collect();
        if ipv4_positions.len() > 1 {
            let shift = NEXT_IPV4_ROTATION.fetch_add(1, Ordering::Relaxed) % ipv4_positions.len();
            if shift != 0 {
                let mut rotated_ipv4: SmallVec<[SocketAddr; 4]> =
                    ipv4_positions.iter().map(|&idx| addrs[idx]).collect();
                rotated_ipv4.rotate_left(shift);
                for (idx, addr) in ipv4_positions.into_iter().zip(rotated_ipv4) {
                    addrs[idx] = addr;
                }
            }
        }

        Ok(addrs)
    }

    fn is_ipv6_network_unreachable(socket_addr: SocketAddr, error: &ConnectionError) -> bool {
        socket_addr.is_ipv6()
            && matches!(
                error,
                ConnectionError::IoError(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NetworkUnreachable | io::ErrorKind::HostUnreachable
                    )
            )
    }

    /// Create a new `TcpManager` with optional TLS configuration
    ///
    /// If `options.tls_config` is `Some` with `use_tls = true`, the TLS manager is
    /// pre-initialized (certificates loaded). If `None` or `use_tls = false`,
    /// plain TCP connections are used.
    ///
    /// # Errors
    /// Returns any TLS initialization error when TLS is enabled for the manager.
    pub fn new(
        host: String,
        port: u16,
        name: String,
        options: TcpManagerOptions,
    ) -> Result<Self, ConnectionError> {
        let tls_policy = options
            .tls_config
            .as_ref()
            .map_or(TlsPolicy::Plain, TlsConfig::policy);
        let tls_manager = match options.tls_config {
            Some(cfg) if tls_policy.is_enabled() => {
                let mgr =
                    Arc::new(
                        TlsManager::new(cfg).map_err(|e| ConnectionError::TlsHandshake {
                            backend: name.clone(),
                            source: e.into(),
                        })?,
                    );
                Some(mgr)
            }
            Some(_) | None => None,
        };
        let compression = CompressionPolicy::from_raw(options.compress, options.compress_level)?;
        let events = Arc::new(SetupEvents::default());
        spawn_setup_event_settler(&events, name.clone());

        Ok(Self {
            host,
            port,
            name,
            username: options.username,
            password: options.password,
            tls_policy,
            tls_manager,
            recv_buffer_size: options.recv_buffer_size,
            send_buffer_size: options.send_buffer_size,
            compression,
            compression_support: Arc::new(OnceCell::new()),
            send_mode_reader: options.send_mode_reader,
            login_backoff: Arc::default(),
            churn: Arc::default(),
            address_load: Arc::default(),
            events,
        })
    }

    async fn connect_socket_addr(
        &self,
        socket_addr: SocketAddr,
    ) -> Result<TcpStream, ConnectionError> {
        // Create tokio TcpSocket (non-blocking from the start)
        let connect_socket = if socket_addr.is_ipv4() {
            tokio::net::TcpSocket::new_v4()?
        } else {
            tokio::net::TcpSocket::new_v6()?
        };

        // Pre-connect options (buffer sizes, reuse)
        if self.recv_buffer_size > 0 {
            connect_socket.set_recv_buffer_size(Self::socket_buffer_size_u32(
                self.recv_buffer_size,
                "receive",
            )?)?;
        }
        if self.send_buffer_size > 0 {
            connect_socket.set_send_buffer_size(Self::socket_buffer_size_u32(
                self.send_buffer_size,
                "send",
            )?)?;
        }
        connect_socket.set_reuseaddr(true)?;

        // Async connect — does NOT block the tokio worker thread
        let tcp_stream = connect_socket.connect(socket_addr).await?;

        // Post-connect options via socket2::SockRef (keepalive with params, nodelay)
        let stream_socket = socket2::SockRef::from(&tcp_stream);
        stream_socket.set_keepalive(true)?;
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(60))
            .with_interval(std::time::Duration::from_secs(10));
        stream_socket.set_tcp_keepalive(&keepalive)?;
        stream_socket.set_tcp_nodelay(true)?;

        Ok(tcp_stream)
    }

    async fn create_connected_tcp_stream(
        &self,
    ) -> Result<(TcpStream, AddressLease), ConnectionError> {
        let addrs = self.lookup_socket_addrs().await?;
        if let Ok(tcp_stream) = self.try_resolved_socket_addrs(&addrs).await {
            return Ok(tcp_stream);
        }

        tracing::debug!(
            backend = %self.name,
            host = %self.host,
            "All resolved backend socket addresses failed; refreshing DNS before final connect pass"
        );

        let resolver = dns_resolver()?;
        resolver.clear_lookup_cache(self.host.as_str(), RecordType::A);
        resolver.clear_lookup_cache(self.host.as_str(), RecordType::AAAA);
        let addrs = self.lookup_socket_addrs().await?;
        self.try_resolved_socket_addrs(&addrs)
            .await
            .map_err(|last_error| {
                last_error.unwrap_or_else(|| ConnectionError::DnsNoAddresses {
                    address: format!("{}:{}", self.host, self.port),
                })
            })
    }

    /// Connect to the resolved address with the fewest live connections from
    /// this backend, falling back to the next emptiest on failure.
    async fn try_resolved_socket_addrs(
        &self,
        addrs: &[SocketAddr],
    ) -> Result<(TcpStream, AddressLease), Option<ConnectionError>> {
        let mut last_error = None;
        let mut skip_ipv6 = false;
        let mut tried: SmallVec<[SocketAddr; 4]> = SmallVec::new();
        while let Some(lease) = self.address_load.reserve_least_loaded(addrs, |addr| {
            tried.contains(&addr) || (skip_ipv6 && addr.is_ipv6())
        }) {
            let socket_addr = lease.addr();
            tried.push(socket_addr);
            match self.connect_socket_addr(socket_addr).await {
                Ok(tcp_stream) => return Ok((tcp_stream, lease)),
                Err(error) => {
                    if Self::is_ipv6_network_unreachable(socket_addr, &error) {
                        let resolver = match dns_resolver() {
                            Ok(r) => r,
                            Err(e) => return Err(Some(e)),
                        };
                        resolver.clear_lookup_cache(self.host.as_str(), RecordType::AAAA);
                        skip_ipv6 = true;
                    }

                    tracing::debug!(
                        backend = %self.name,
                        host = %self.host,
                        socket_addr = %socket_addr,
                        error = %error,
                        "Backend socket address connect failed; trying next resolved address"
                    );
                    last_error = Some(error);
                }
            }
        }

        Err(last_error)
    }

    /// Create an optimized connection (TCP or TLS)
    pub(crate) async fn create_optimized_stream(
        &self,
    ) -> Result<(ConnectionStream, AddressLease), ConnectionError> {
        let (tcp_stream, address) = self.create_connected_tcp_stream().await?;

        // Perform TLS handshake if enabled
        if self.tls_policy.is_enabled() {
            // Use cached TLS manager to avoid re-parsing certificates
            let Some(tls_manager) = self.tls_manager.as_ref() else {
                return Err(ConnectionError::TlsHandshake {
                    backend: self.name.clone(),
                    source: "TLS enabled but TLS manager not initialized".into(),
                });
            };

            let tls_stream = tls_manager
                .handshake(tcp_stream, &self.host, &self.name)
                .await
                .map_err(|e| ConnectionError::TlsHandshake {
                    backend: self.name.clone(),
                    source: e.into(),
                })?;
            Ok((ConnectionStream::tls(tls_stream), address))
        } else {
            Ok((ConnectionStream::plain(tcp_stream), address))
        }
    }
}

// ============================================================================
// Connection setup: greeting, auth, and future negotiation hooks
// ============================================================================

impl TcpManager {
    /// Read one single-line setup reply using the same backend reply framing
    /// facade as normal commands.
    ///
    /// Connection setup commands are not on the hot article path, but they must
    /// still tolerate split TCP reads and must not open another place that
    /// reasons about response line boundaries.
    async fn read_backend_setup_reply(
        stream: &mut ConnectionStream,
        request: &RequestContext,
        buffer: &mut [u8],
    ) -> Result<String, ConnectionError> {
        match crate::session::backend::read_single_line_reply(stream, request, buffer).await {
            Ok(reply) => Ok(reply),
            Err(
                crate::session::backend::SingleLineReplyReadError::Full { bytes_read }
                | crate::session::backend::SingleLineReplyReadError::Invalid { bytes_read },
            ) => Err(ConnectionError::IoError(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "invalid or truncated backend setup reply: {}",
                    String::from_utf8_lossy(&buffer[..bytes_read]).trim_end()
                ),
            ))),
            Err(crate::session::backend::SingleLineReplyReadError::Io(err)) => Err(err.into()),
            Err(crate::session::backend::SingleLineReplyReadError::Closed) => {
                Err(ConnectionError::IoError(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "backend closed while reading setup reply",
                )))
            }
        }
    }

    /// Read and validate the NNTP server greeting.
    async fn consume_greeting(
        &self,
        stream: &mut ConnectionStream,
        buffer: &mut [u8],
    ) -> Result<(), ConnectionError> {
        let request = RequestContext::from_verb_args(b"MODE", b"READER");
        let greeting = Self::read_backend_setup_reply(stream, &request, buffer).await?;

        if !crate::protocol::StatusCode::parse(greeting.as_bytes())
            .is_some_and(|code| code.is_greeting())
        {
            return Err(ConnectionError::InvalidGreeting {
                backend: self.name.clone(),
                greeting: greeting.trim().to_string(),
            });
        }

        Ok(())
    }

    /// Send MODE READER to the backend after authentication.
    ///
    /// RFC 3977 §5.3: MODE READER switches the server into reader mode.
    /// Valid responses are 200 (posting allowed) or 201 (posting not permitted).
    /// Any other response indicates the service is unavailable for reading.
    async fn negotiate_mode_reader(
        &self,
        stream: &mut ConnectionStream,
        buffer: &mut [u8],
    ) -> Result<(), ConnectionError> {
        stream.write_all(b"MODE READER\r\n").await?;
        stream.flush().await?;

        let request = RequestContext::from_verb_args(b"MODE", b"READER");
        let response = Self::read_backend_setup_reply(stream, &request, buffer).await?;

        // RFC 3977 §5.3: 200 = reader mode + posting allowed, 201 = reader mode + posting not permitted
        if crate::protocol::StatusCode::parse(response.as_bytes())
            .is_some_and(|code| matches!(code.as_u16(), 200 | 201))
        {
            tracing::debug!(
                backend = %self.name,
                response = %response.trim(),
                "MODE READER accepted"
            );
            return Ok(());
        }

        Err(ConnectionError::ReaderModeRefused {
            backend: self.name.clone(),
            response: response.trim().to_string(),
        })
    }

    /// Negotiate COMPRESS DEFLATE (RFC 8054) with the backend server.
    ///
    /// Returns `Ok(true)` if compression was successfully negotiated,
    /// `Ok(false)` if compression was skipped or not supported.
    async fn negotiate_compression(
        &self,
        stream: &mut ConnectionStream,
        buffer: &mut [u8],
    ) -> Result<bool, ConnectionError> {
        if matches!(self.compression.mode, CompressionMode::Disabled) {
            return Ok(false);
        }

        if matches!(self.compression.mode, CompressionMode::Required) {
            return self
                .probe_compression_with_timeout(stream, buffer)
                .await
                .map(|support| matches!(support, CompressionSupport::Supported));
        }

        let mut negotiated = None;
        let support = *self
            .compression_support
            .get_or_try_init(|| async {
                let support = self.probe_compression_with_timeout(stream, buffer).await?;
                negotiated = Some(support);
                Ok::<_, ConnectionError>(support)
            })
            .await?;

        if let Some(support) = negotiated {
            return Ok(support == CompressionSupport::Supported);
        }
        match support {
            CompressionSupport::Unsupported => Ok(false),
            CompressionSupport::Supported => self
                .probe_compression_with_timeout(stream, buffer)
                .await
                .map(|support| support == CompressionSupport::Supported),
        }
    }

    async fn probe_compression_with_timeout(
        &self,
        stream: &mut ConnectionStream,
        buffer: &mut [u8],
    ) -> Result<CompressionSupport, ConnectionError> {
        tokio::time::timeout(
            crate::constants::timeout::CONNECTION,
            self.probe_compression(stream, buffer),
        )
        .await
        .map_err(|_| {
            ConnectionError::IoError(io::Error::new(
                io::ErrorKind::TimedOut,
                "timed out negotiating COMPRESS DEFLATE",
            ))
        })?
    }

    async fn probe_compression(
        &self,
        stream: &mut ConnectionStream,
        buffer: &mut [u8],
    ) -> Result<CompressionSupport, ConnectionError> {
        stream.write_all(crate::protocol::COMPRESS_DEFLATE).await?;
        stream.flush().await?;

        let request = RequestContext::from_verb_args(b"COMPRESS", b"DEFLATE");
        let response = Self::read_backend_setup_reply(stream, &request, buffer).await?;

        // 206 = Compression active
        if crate::protocol::StatusCode::parse(response.as_bytes())
            .is_some_and(|code| code.as_u16() == 206)
        {
            tracing::debug!(
                backend = %self.name,
                "COMPRESS DEFLATE negotiated successfully"
            );
            return Ok(CompressionSupport::Supported);
        }

        if matches!(self.compression.mode, CompressionMode::Required) {
            return Err(ConnectionError::CompressionRequired {
                backend: self.name.clone(),
                response: response.trim().to_string(),
            });
        }

        // Auto mode: compression not supported, continue without it
        tracing::debug!(
            backend = %self.name,
            response = %response.trim(),
            "COMPRESS DEFLATE not supported, continuing without compression"
        );
        Ok(CompressionSupport::Unsupported)
    }

    /// Perform AUTHINFO USER/PASS handshake if credentials are configured.
    async fn negotiate_auth(
        &self,
        stream: &mut ConnectionStream,
        buffer: &mut [u8],
    ) -> Result<(), ConnectionError> {
        let Some(username) = &self.username else {
            return Ok(());
        };

        authinfo_user(username).write_wire_to(stream).await?;
        let user_request = authinfo_user(username);
        let response = Self::read_backend_setup_reply(stream, &user_request, buffer).await?;

        if crate::protocol::StatusCode::parse(response.as_bytes())
            .is_some_and(|code| code.requires_auth_credentials())
        {
            // Password required
            let Some(password) = self.password.as_ref() else {
                return Err(ConnectionError::PasswordRequired {
                    backend: self.name.clone(),
                });
            };

            authinfo_pass(password).write_wire_to(stream).await?;
            let pass_request = authinfo_pass(password);
            let response = Self::read_backend_setup_reply(stream, &pass_request, buffer).await?;

            if !crate::protocol::StatusCode::parse(response.as_bytes())
                .is_some_and(|code| code.is_auth_accepted())
            {
                return Err(self.authinfo_refusal(&response, AuthinfoStep::Pass));
            }
            tracing::debug!(
                backend = %self.name,
                host = %self.host,
                port = self.port,
                "Authenticated to backend"
            );
        } else if !crate::protocol::StatusCode::parse(response.as_bytes())
            .is_some_and(|code| code.is_auth_accepted())
        {
            return Err(self.authinfo_refusal(&response, AuthinfoStep::User));
        }

        Ok(())
    }

    /// The error for a refused `AUTHINFO` reply, built from the shared
    /// classification so the error variant never contradicts
    /// [`ConnectionError::login_refusal`]: `481` is bad credentials at either
    /// step, a connection-limit reply is a limit, and anything else is a
    /// failed login (`PASS`) or an unexpected reply (`USER`). `create` logs
    /// the refusal once with its backoff, so this logs only at debug, and
    /// never the account name.
    fn authinfo_refusal(&self, response: &str, step: AuthinfoStep) -> ConnectionError {
        use crate::connection_error::{AuthinfoReply, classify_authinfo_reply};
        let backend = self.name.clone();
        let response = response.trim().to_string();
        tracing::debug!(
            backend = %self.name,
            host = %self.host,
            port = self.port,
            response = %response,
            step = ?step,
            "Backend refused AUTHINFO"
        );
        match (classify_authinfo_reply(&response), step) {
            (AuthinfoReply::ConnectionLimit, _) => {
                ConnectionError::ConnectionLimitExceeded { backend, response }
            }
            (AuthinfoReply::BadCredentials, _) | (AuthinfoReply::Other, AuthinfoStep::Pass) => {
                ConnectionError::AuthenticationFailed { backend, response }
            }
            (AuthinfoReply::Other, AuthinfoStep::User) => {
                ConnectionError::UnexpectedAuthResponse { backend, response }
            }
        }
    }
}

/// Which `AUTHINFO` command a refusal answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuthinfoStep {
    User,
    Pass,
}

impl TcpManager {
    /// Open, greet, authenticate and configure one backend session.
    async fn setup_session(&self) -> Result<ReadyBackendConnection, ConnectionError> {
        tokio::time::timeout(crate::constants::timeout::CONNECTION, async {
            // The address counts as healthy only once the whole session is
            // ready: one that accepts TCP but fails TLS, the greeting or
            // login must cool down, or it would stay the emptiest choice.
            let (mut stream, mut address) = self.create_optimized_stream().await?;
            let mut buffer = [0u8; 4096];

            self.consume_greeting(&mut stream, &mut buffer).await?;
            self.negotiate_auth(&mut stream, &mut buffer).await?;

            if self.send_mode_reader {
                self.negotiate_mode_reader(&mut stream, &mut buffer).await?;
            }

            if self.negotiate_compression(&mut stream, &mut buffer).await? {
                let level = self.compression.level;
                stream = stream.into_compressed(level)?;
            }

            address.mark_connected();
            Ok(ReadyBackendConnection::new(
                stream,
                Arc::clone(&self.churn),
                Some(address),
            ))
        })
        .await
        .map_err(|_| {
            ConnectionError::IoError(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("Backend '{}' connection setup timed out", self.name),
            ))
        })?
    }
}

// ============================================================================
// Manager trait implementation
// ============================================================================

impl managed::Manager for TcpManager {
    type Type = ReadyBackendConnection;
    type Error = ConnectionError;

    async fn create(&self) -> Result<ReadyBackendConnection, ConnectionError> {
        let attempt = match self.login_backoff.admit() {
            Admission::Proceed(attempt) => attempt,
            Admission::Wait(retry_in) => {
                self.churn.note_connect(ConnectOutcome::BackoffDenied);
                return Err(ConnectionError::LoginBackoff {
                    backend: self.name.clone(),
                    retry_in,
                });
            }
        };

        let result = self.setup_session().await;
        match &result {
            Ok(_) => {
                self.churn.note_connect(ConnectOutcome::Created);
                attempt.succeeded();
                self.events.login_succeeded();
                self.events.settle(&self.name);
            }
            Err(error) if error.is_login_refusal() => {
                self.churn.note_connect(ConnectOutcome::LoginRefused);
                // Zero means a late refusal from before a successful login:
                // the gate stays open and new connections are not paused.
                let retry_in = attempt.refused();
                self.events.failed(self, error, retry_in);
            }
            // A network failure or timeout says nothing about the account;
            // dropping the attempt hands any probe slot to the next caller.
            Err(error) => {
                self.churn.note_connect(ConnectOutcome::Failed);
                drop(attempt);
                self.events.failed(self, error, Duration::ZERO);
            }
        }
        result
    }

    async fn recycle(
        &self,
        conn: &mut ReadyBackendConnection,
        _metrics: &managed::Metrics,
    ) -> managed::RecycleResult<ConnectionError> {
        use super::health_check::check_tcp_alive;
        match check_tcp_alive(&mut conn.0) {
            Ok(()) => Ok(()),
            Err(e) => {
                conn.tag_close_reason(CloseReason::Stale);
                // Shut down TCP immediately so backend releases the slot
                // before deadpool drops this and creates a replacement.
                let _ = socket2::SockRef::from(conn.underlying_tcp_stream())
                    .shutdown(std::net::Shutdown::Both);
                Err(e)
            }
        }
    }

    fn detach(&self, _conn: &mut ReadyBackendConnection) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manager_debug_redacts_credentials() {
        let options = TcpManagerOptions {
            username: Some("account-name-must-not-leak".to_string()),
            password: Some("password-must-not-leak".to_string()),
            ..TcpManagerOptions::default()
        };
        let manager = TcpManager::new(
            "news.example.com".to_string(),
            563,
            "backend-a".to_string(),
            options.clone(),
        )
        .unwrap();
        for text in [format!("{options:?}"), format!("{manager:?}")] {
            assert!(!text.contains("must-not-leak"), "{text}");
            assert!(text.contains("<redacted>"), "{text}");
        }
    }
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn cancelled_compression_probe_allows_the_next_connection_to_probe() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let manager = TcpManager::new(
            addr.ip().to_string(),
            addr.port(),
            "cancelled-probe".into(),
            TcpManagerOptions::default(),
        )
        .unwrap();
        let tcp = TcpStream::connect(addr).await.unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        let mut stream = ConnectionStream::plain(tcp);
        let mut buffer = [0; 128];
        let mut probe = Box::pin(manager.negotiate_compression(&mut stream, &mut buffer));
        let mut command = [0; 18];
        tokio::select! {
            result = peer.read_exact(&mut command) => { result.unwrap(); },
            result = &mut probe => panic!("probe completed without reply: {result:?}"),
        }
        assert_eq!(&command, crate::protocol::COMPRESS_DEFLATE);
        // Dropping the scope containing the pinned future cancels its ownership.
        drop(probe);
        drop(stream);
        drop(peer);
        let tcp = TcpStream::connect(addr).await.unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        let reply = tokio::spawn(async move {
            let mut command = [0; 18];
            peer.read_exact(&mut command).await.unwrap();
            peer.write_all(b"500 unsupported\r\n").await.unwrap();
        });
        let mut stream = ConnectionStream::plain(tcp);
        let mut buffer = [0; 128];
        assert!(
            !tokio::time::timeout(
                std::time::Duration::from_secs(1),
                manager.negotiate_compression(&mut stream, &mut buffer)
            )
            .await
            .expect("cancelled probe stranded shared initialization")
            .unwrap()
        );
        reply.await.unwrap();
    }

    #[tokio::test]
    async fn cached_supported_compression_negotiates_each_new_session_in_parallel() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received = Arc::new(tokio::sync::Semaphore::new(0));
        let replies = Arc::new(tokio::sync::Semaphore::new(0));
        let server_received = received.clone();
        let server_replies = replies.clone();
        tokio::spawn(async move {
            for index in 0..3 {
                let (mut peer, _) = listener.accept().await.unwrap();
                let received = server_received.clone();
                let replies = server_replies.clone();
                tokio::spawn(async move {
                    let mut command = [0; 18];
                    peer.read_exact(&mut command).await.unwrap();
                    assert_eq!(&command, crate::protocol::COMPRESS_DEFLATE);
                    if index != 0 {
                        // Hold the reply until both later sessions have sent
                        // their probe, so a serialized probe would stall here.
                        received.add_permits(1);
                        replies.acquire().await.unwrap().forget();
                    }
                    peer.write_all(b"206 Compression active\r\n").await.unwrap();
                    std::future::pending::<()>().await;
                });
            }
        });
        let manager = TcpManager::new(
            addr.ip().to_string(),
            addr.port(),
            "supported-parallel".into(),
            TcpManagerOptions {
                compress: None,
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();

        let negotiate = |manager: TcpManager| async move {
            let tcp = TcpStream::connect(addr).await.unwrap();
            let mut stream = ConnectionStream::plain(tcp);
            let mut buffer = [0; 128];
            manager
                .negotiate_compression(&mut stream, &mut buffer)
                .await
                .unwrap()
        };
        assert!(negotiate(manager.clone()).await);
        assert_eq!(
            manager.compression_support.get(),
            Some(&CompressionSupport::Supported)
        );

        let second = tokio::spawn(negotiate(manager.clone()));
        let third = tokio::spawn(negotiate(manager.clone()));
        tokio::time::timeout(std::time::Duration::from_secs(1), received.acquire_many(2))
            .await
            .expect("cached support must let each new session probe without waiting on another")
            .unwrap()
            .forget();
        replies.add_permits(2);
        assert!(
            second.await.unwrap(),
            "a cached Supported backend must still negotiate compression on a new session"
        );
        assert!(third.await.unwrap());
    }

    #[tokio::test]
    async fn failed_compression_probe_leaves_discovery_open_for_the_next_session() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let probes = Arc::new(AtomicUsize::new(0));
        let server_probes = probes.clone();
        tokio::spawn(async move {
            for index in 0..3 {
                let (mut peer, _) = listener.accept().await.unwrap();
                let probes = server_probes.clone();
                tokio::spawn(async move {
                    let mut command = [0; 18];
                    if tokio::time::timeout(
                        std::time::Duration::from_millis(200),
                        peer.read_exact(&mut command),
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                    probes.fetch_add(1, Ordering::SeqCst);
                    if index == 0 {
                        // Close without replying: the probe fails with an error.
                        return;
                    }
                    peer.write_all(b"500 Unsupported\r\n").await.unwrap();
                    std::future::pending::<()>().await;
                });
            }
        });
        let manager = TcpManager::new(
            addr.ip().to_string(),
            addr.port(),
            "failed-discovery".into(),
            TcpManagerOptions {
                compress: None,
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();

        let negotiate = |manager: TcpManager| async move {
            let tcp = TcpStream::connect(addr).await.unwrap();
            let mut stream = ConnectionStream::plain(tcp);
            let mut buffer = [0; 128];
            manager
                .negotiate_compression(&mut stream, &mut buffer)
                .await
        };
        assert!(negotiate(manager.clone()).await.is_err());
        assert_eq!(
            manager.compression_support.get(),
            None,
            "a failed probe must not cache a support answer"
        );

        assert!(!negotiate(manager.clone()).await.unwrap());
        assert_eq!(
            manager.compression_support.get(),
            Some(&CompressionSupport::Unsupported)
        );
        assert!(!negotiate(manager.clone()).await.unwrap());
        assert_eq!(
            probes.load(Ordering::SeqCst),
            2,
            "the failed probe is retried once, then Unsupported is remembered"
        );
    }

    #[tokio::test]
    async fn backend_setup_has_a_deadline_when_greeting_stalls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let manager = TcpManager::new(
            addr.ip().to_string(),
            addr.port(),
            "stalled".into(),
            TcpManagerOptions {
                compress: Some(false),
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();
        let create = managed::Manager::create(&manager);
        tokio::pin!(create);
        let (socket, _) = tokio::select! {
            accepted = listener.accept() => accepted.unwrap(),
            result = &mut create => panic!("setup completed before greeting: {result:?}"),
        };
        tokio::time::pause();
        let result = tokio::time::timeout(crate::constants::timeout::CONNECTION * 2, &mut create)
            .await
            .expect("backend setup did not enforce its deadline");
        assert!(
            matches!(result, Err(ConnectionError::IoError(error)) if error.kind() == std::io::ErrorKind::TimedOut)
        );
        drop(socket);
    }

    #[test]
    fn test_socket_buffer_size_u32_rejects_oversized_values() {
        let result = TcpManager::socket_buffer_size_u32(u32::MAX as usize + 1, "receive");

        assert!(matches!(
            result,
            Err(ConnectionError::IoError(ref error))
                if error.kind() == io::ErrorKind::InvalidInput
                    && error.to_string().contains("receive socket buffer size")
        ));
    }

    #[test]
    fn test_tcp_manager_new_rejects_invalid_compression_level() {
        let result = TcpManager::new(
            "news.example.com".to_string(),
            119,
            "TestServer".to_string(),
            TcpManagerOptions {
                compress_level: Some(10),
                ..TcpManagerOptions::default()
            },
        );

        assert!(matches!(
            result,
            Err(ConnectionError::IoError(ref error))
                if error.kind() == io::ErrorKind::InvalidInput
                    && error.to_string().contains("compression level")
        ));
    }

    #[test]
    fn test_tcp_manager_new_plain() {
        let manager = TcpManager::new(
            "news.example.com".to_string(),
            119,
            "TestServer".to_string(),
            TcpManagerOptions {
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();

        assert_eq!(manager.host, "news.example.com");
        assert_eq!(manager.port, 119);
        assert_eq!(manager.name, "TestServer");
        assert_eq!(manager.username, Some("user".to_string()));
        assert_eq!(manager.password, Some("pass".to_string()));
        assert!(!manager.tls_policy.is_enabled());
        assert!(manager.tls_manager.is_none());
    }

    #[test]
    fn test_tcp_manager_new_without_auth() {
        let manager = TcpManager::new(
            "news.example.com".to_string(),
            563,
            "SecureServer".to_string(),
            TcpManagerOptions::default(),
        )
        .unwrap();

        assert_eq!(manager.host, "news.example.com");
        assert_eq!(manager.port, 563);
        assert_eq!(manager.name, "SecureServer");
        assert!(manager.username.is_none());
        assert!(manager.password.is_none());
    }

    #[test]
    fn ipv6_network_unreachable_matches_error_kind_only_for_ipv6() {
        let ipv6_addr = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 563));
        let ipv4_addr = SocketAddr::from(([127, 0, 0, 1], 563));
        let error = ConnectionError::IoError(io::Error::new(
            io::ErrorKind::NetworkUnreachable,
            "network unreachable",
        ));

        assert!(TcpManager::is_ipv6_network_unreachable(ipv6_addr, &error));
        assert!(!TcpManager::is_ipv6_network_unreachable(ipv4_addr, &error));
    }

    #[tokio::test]
    async fn create_connected_tcp_stream_resolves_hostname() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = listener.local_addr().unwrap();

        let manager = TcpManager::new(
            "localhost".to_string(),
            live_addr.port(),
            "DnsBackend".to_string(),
            TcpManagerOptions::default(),
        )
        .unwrap();

        let accept = tokio::spawn(async move { listener.accept().await.unwrap() });
        let (stream, _address) = manager.create_connected_tcp_stream().await.unwrap();
        let _accepted = accept.await.unwrap();

        assert_eq!(stream.peer_addr().unwrap(), live_addr);
    }

    #[test]
    fn missing_resolv_conf_is_a_not_found_io_error() {
        let missing = hickory_resolver::net::NetError::Io(std::sync::Arc::new(io::Error::new(
            io::ErrorKind::NotFound,
            "missing",
        )));
        let denied = hickory_resolver::net::NetError::Io(std::sync::Arc::new(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "denied",
        )));

        assert!(super::system_resolv_conf_missing(&missing));
        assert!(!super::system_resolv_conf_missing(&denied));
    }

    #[test]
    fn missing_resolv_conf_is_retried_once_after_five_minutes() {
        let now = Instant::now();
        let delay = super::RESOLV_CONF_RETRY;
        assert_eq!(delay, Duration::from_secs(5 * 60));

        let first = super::hosts_cache_after_miss(None, now, delay);
        let super::CacheKind::HostsPending(retry_at) = first else {
            panic!("first miss should schedule one retry");
        };
        assert_eq!(retry_at, now + delay);

        let waiting =
            super::hosts_cache_after_miss(Some(first), now + Duration::from_secs(30), delay);
        assert!(matches!(waiting, super::CacheKind::HostsPending(at) if at == retry_at));

        let second = super::hosts_cache_after_miss(Some(first), retry_at, delay);
        assert!(matches!(second, super::CacheKind::HostsFinal));

        let later = super::hosts_cache_after_miss(Some(second), retry_at + delay, delay);
        assert!(matches!(later, super::CacheKind::HostsFinal));
    }

    #[tokio::test]
    async fn hosts_file_resolver_maps_localhost_without_a_nameserver() {
        let resolver = super::hosts_file_resolver().unwrap();
        let lookup = resolver.lookup_ip("localhost").await.unwrap();
        let addresses: Vec<_> = lookup.iter().collect();

        assert!(
            addresses.iter().any(IpAddr::is_loopback),
            "localhost should come from the hosts file, got {addresses:?}"
        );
    }

    #[tokio::test]
    async fn create_connected_tcp_stream_ip_literal_skips_dns() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = listener.local_addr().unwrap();

        let manager = TcpManager::new(
            live_addr.ip().to_string(),
            live_addr.port(),
            "IpBackend".to_string(),
            TcpManagerOptions::default(),
        )
        .unwrap();

        let accept = tokio::spawn(async move { listener.accept().await.unwrap() });
        let (stream, _address) = manager.create_connected_tcp_stream().await.unwrap();
        let _accepted = accept.await.unwrap();

        assert_eq!(stream.peer_addr().unwrap(), live_addr);
    }

    #[test]
    fn test_tcp_manager_new_with_tls_disabled() {
        let tls_config = TlsConfig::default(); // use_tls = false
        let manager = TcpManager::new(
            "news.example.com".to_string(),
            119,
            "PlainServer".to_string(),
            TcpManagerOptions {
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                tls_config: Some(tls_config),
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();

        assert_eq!(manager.host, "news.example.com");
        assert_eq!(manager.port, 119);
        assert!(!manager.tls_policy.is_enabled());
        assert!(manager.tls_manager.is_none());
    }

    #[test]
    fn test_tcp_manager_new_with_tls_enabled() {
        let tls_config = TlsConfig {
            use_tls: true,
            tls_verify_cert: true,
            tls_cert_path: None,
        };
        let manager = TcpManager::new(
            "secure.example.com".to_string(),
            563,
            "SecureServer".to_string(),
            TcpManagerOptions {
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                tls_config: Some(tls_config),
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();

        assert_eq!(manager.host, "secure.example.com");
        assert_eq!(manager.port, 563);
        assert!(manager.tls_policy.is_enabled());
        assert!(manager.tls_manager.is_some());
    }

    #[test]
    fn test_tcp_manager_clone() {
        let manager = TcpManager::new(
            "news.example.com".to_string(),
            119,
            "TestServer".to_string(),
            TcpManagerOptions {
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();

        let cloned = manager.clone();
        assert_eq!(cloned.host, manager.host);
        assert_eq!(cloned.port, manager.port);
        assert_eq!(cloned.name, manager.name);
        assert_eq!(cloned.username, manager.username);
        assert_eq!(cloned.password, manager.password);
    }

    #[test]
    fn test_tcp_manager_debug_format() {
        let manager = TcpManager::new(
            "news.example.com".to_string(),
            119,
            "TestServer".to_string(),
            TcpManagerOptions {
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();

        let debug_output = format!("{manager:?}");
        assert!(debug_output.contains("TcpManager"));
        assert!(debug_output.contains("news.example.com"));
        assert!(debug_output.contains("119"));
    }

    #[tokio::test]
    async fn try_resolved_socket_addrs_tries_next_address_after_connect_error() {
        let unavailable_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let unavailable_addr = unavailable_listener.local_addr().unwrap();
        drop(unavailable_listener);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let available_addr = listener.local_addr().unwrap();
        let accept_task = tokio::spawn(async move {
            let (_stream, _) = listener.accept().await.unwrap();
        });

        let manager = TcpManager::new(
            "test.example.com".to_string(),
            available_addr.port(),
            "FallbackBackend".to_string(),
            TcpManagerOptions::default(),
        )
        .unwrap();

        let (stream, lease) = manager
            .try_resolved_socket_addrs(&[unavailable_addr, available_addr])
            .await
            .expect("second resolved address should be tried after first connect error");

        assert_eq!(stream.peer_addr().unwrap(), available_addr);
        assert_eq!(lease.addr(), available_addr);
        assert_eq!(
            manager.address_load.live_count(unavailable_addr),
            0,
            "the failed attempt released its reservation"
        );
        accept_task.await.unwrap();
    }

    /// An address that accepts TCP but never completes session setup cools
    /// down; a completed setup leaves its address healthy.
    #[tokio::test]
    async fn failed_session_setup_cools_the_address_down() {
        let broken = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let broken_addr = broken.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((stream, _)) = broken.accept().await {
                drop(stream); // accepts, then closes before any greeting
            }
        });
        let manager = TcpManager::new(
            broken_addr.ip().to_string(),
            broken_addr.port(),
            "broken".to_string(),
            TcpManagerOptions {
                compress: Some(false),
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();
        assert!(manager.setup_session().await.is_err());
        assert!(manager.address_load.is_cooling(broken_addr));

        let (healthy_addr, _accepted) = spawn_login_server(|_| "281 ok\r\n").await;
        let healthy = login_manager(healthy_addr);
        let connection = healthy.setup_session().await.unwrap();
        assert!(!healthy.address_load.is_cooling(healthy_addr));
        assert_eq!(healthy.address_load.live_count(healthy_addr), 1);
        drop(connection);
        assert_eq!(healthy.address_load.live_count(healthy_addr), 0);
        assert!(!healthy.address_load.is_cooling(healthy_addr));
    }

    /// Connections from one backend spread evenly over its addresses, and a
    /// closed connection frees its address for the next one.
    #[tokio::test]
    async fn connections_spread_evenly_over_resolved_addresses() {
        let mut addrs = Vec::new();
        for _ in 0..2 {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            addrs.push(listener.local_addr().unwrap());
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    // Hold each accepted socket open for the rest of the test.
                    tokio::spawn(async move {
                        let _stream = stream;
                        std::future::pending::<()>().await;
                    });
                }
            });
        }
        let manager = TcpManager::new(
            "test.example.com".to_string(),
            addrs[0].port(),
            "SpreadBackend".to_string(),
            TcpManagerOptions::default(),
        )
        .unwrap();

        let mut connections = Vec::new();
        for _ in 0..10 {
            let (stream, mut lease) = manager.try_resolved_socket_addrs(&addrs).await.unwrap();
            lease.mark_connected(); // as a completed session setup does
            connections.push((stream, lease));
        }
        assert_eq!(manager.address_load.live_count(addrs[0]), 5);
        assert_eq!(manager.address_load.live_count(addrs[1]), 5);

        connections.retain(|(_, lease)| lease.addr() != addrs[1]);
        assert_eq!(manager.address_load.live_count(addrs[1]), 0);
        let (_, lease) = manager.try_resolved_socket_addrs(&addrs).await.unwrap();
        assert_eq!(
            lease.addr(),
            addrs[1],
            "the emptied address is refilled first"
        );
    }

    #[test]
    fn test_tcp_manager_with_tls_manager_is_some() {
        let tls_config = TlsConfig {
            use_tls: true,
            tls_verify_cert: false,
            tls_cert_path: None,
        };
        let manager = TcpManager::new(
            "secure.example.com".to_string(),
            563,
            "SecureServer".to_string(),
            TcpManagerOptions {
                tls_config: Some(tls_config),
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();

        assert!(manager.tls_manager.is_some());

        // Verify TLS manager is an Arc (cheap clone)
        let cloned_tls_manager = manager.tls_manager.as_ref().unwrap().clone();
        assert!(Arc::ptr_eq(
            manager.tls_manager.as_ref().unwrap(),
            &cloned_tls_manager
        ));
    }

    #[test]
    fn test_tcp_manager_with_tls_cert_path() {
        let tls_config = TlsConfig {
            use_tls: true,
            tls_verify_cert: true,
            tls_cert_path: Some("/path/to/ca.pem".to_string()),
        };

        // This will fail due to missing file, but tests the construction path
        let result = TcpManager::new(
            "secure.example.com".to_string(),
            563,
            "SecureServer".to_string(),
            TcpManagerOptions {
                tls_config: Some(tls_config),
                ..TcpManagerOptions::default()
            },
        );

        // Should fail because cert file doesn't exist
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn mode_reader_negotiation_reads_split_setup_reply() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut command = [0u8; 13];
            stream.read_exact(&mut command).await.unwrap();
            assert_eq!(&command, b"MODE READER\r\n");
            stream.write_all(b"20").await.unwrap();
            stream.write_all(b"0 Posting allowed\r\n").await.unwrap();
        });

        let manager = TcpManager::new(
            addr.ip().to_string(),
            addr.port(),
            "SplitSetup".to_string(),
            TcpManagerOptions::default(),
        )
        .unwrap();
        let tcp_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut stream = ConnectionStream::plain(tcp_stream);
        let mut buffer = [0u8; 64];

        manager
            .negotiate_mode_reader(&mut stream, &mut buffer)
            .await
            .expect("split MODE READER setup reply should be accepted");
    }

    #[tokio::test]
    async fn compression_negotiation_reads_split_unsupported_reply() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut command = [0u8; 18];
            stream.read_exact(&mut command).await.unwrap();
            assert_eq!(&command, crate::protocol::COMPRESS_DEFLATE);
            stream.write_all(b"50").await.unwrap();
            stream.write_all(b"0 Not supported\r\n").await.unwrap();
        });

        let manager = TcpManager::new(
            addr.ip().to_string(),
            addr.port(),
            "SplitCompression".to_string(),
            TcpManagerOptions {
                compress: None,
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();
        let tcp_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut stream = ConnectionStream::plain(tcp_stream);
        let mut buffer = [0u8; 64];

        let enabled = manager
            .negotiate_compression(&mut stream, &mut buffer)
            .await
            .expect("split unsupported compression reply should be accepted");

        assert!(!enabled);
    }

    #[tokio::test]
    async fn auto_compression_serializes_and_remembers_unsupported_backend() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let compress_commands = Arc::new(AtomicUsize::new(0));
        let server_commands = compress_commands.clone();

        tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let server_commands = server_commands.clone();
                tokio::spawn(async move {
                    let mut command = [0u8; 18];
                    let read = tokio::time::timeout(
                        std::time::Duration::from_millis(100),
                        stream.read_exact(&mut command),
                    )
                    .await;
                    if read.is_err() {
                        return;
                    }
                    read.unwrap().unwrap();
                    if command == crate::protocol::COMPRESS_DEFLATE {
                        server_commands.fetch_add(1, Ordering::SeqCst);
                        stream.write_all(b"500 Not supported\r\n").await.unwrap();
                    }
                });
            }
        });

        let manager = TcpManager::new(
            addr.ip().to_string(),
            addr.port(),
            "CachedUnsupportedCompression".to_string(),
            TcpManagerOptions {
                compress: None,
                ..TcpManagerOptions::default()
            },
        )
        .unwrap();

        let mut tasks = Vec::new();
        for _ in 0..2 {
            let manager = manager.clone();
            tasks.push(tokio::spawn(async move {
                let tcp_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
                let mut stream = ConnectionStream::plain(tcp_stream);
                let mut buffer = [0u8; 64];

                let enabled = manager
                    .negotiate_compression(&mut stream, &mut buffer)
                    .await
                    .expect("unsupported compression should fall back in auto mode");

                assert!(!enabled);
            }));
        }

        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(
            compress_commands.load(Ordering::SeqCst),
            1,
            "auto mode should remember unsupported COMPRESS DEFLATE"
        );
    }

    /// Mock backend whose `AUTHINFO PASS` reply depends on the connection's
    /// arrival order, so a test can script refusals followed by success.
    async fn spawn_login_server(
        pass_reply: fn(usize) -> &'static str,
    ) -> (SocketAddr, Arc<AtomicUsize>) {
        use tokio::io::{AsyncBufReadExt, BufReader};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counter = accepted.clone();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let connection = counter.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut lines = BufReader::new(read).lines();
                    write.write_all(b"200 ready\r\n").await.unwrap();
                    // Setup order: AUTHINFO USER, AUTHINFO PASS, MODE READER.
                    let mut index = 0;
                    while let Ok(Some(_line)) = lines.next_line().await {
                        let reply = match index {
                            0 => "381 password required\r\n",
                            1 => pass_reply(connection),
                            _ => "200 ok\r\n",
                        };
                        index += 1;
                        if write.write_all(reply.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        (addr, accepted)
    }

    fn login_manager(addr: SocketAddr) -> TcpManager {
        TcpManager::new(
            addr.ip().to_string(),
            addr.port(),
            "login-backoff".to_string(),
            TcpManagerOptions {
                username: Some("user".to_string()),
                password: Some("pass".to_string()),
                compress: Some(false),
                ..TcpManagerOptions::default()
            },
        )
        .unwrap()
    }

    const LIMIT_REACHED: &str = "502 connection limit (10) reached\r\n";

    #[tokio::test]
    async fn refused_login_backs_off_without_reconnecting() {
        let (addr, accepted) = spawn_login_server(|_| LIMIT_REACHED).await;
        let manager = login_manager(addr);

        let first = managed::Manager::create(&manager).await;
        // "502 connection limit ... reached" is a limit refusal.
        assert!(matches!(
            first,
            Err(ConnectionError::ConnectionLimitExceeded { .. })
        ));
        for _ in 0..5 {
            let again = managed::Manager::create(&manager).await;
            assert!(matches!(again, Err(ConnectionError::LoginBackoff { .. })));
        }
        assert_eq!(
            accepted.load(Ordering::SeqCst),
            1,
            "no session may be opened while the backend is in login backoff"
        );
    }

    /// Bad credentials are not a connection limit, but retrying them at once
    /// gets the same answer, so they back off like any other refusal.
    #[tokio::test]
    async fn bad_credentials_back_off_as_credential_failures() {
        let (addr, accepted) = spawn_login_server(|_| "481 Authentication failed\r\n").await;
        let manager = login_manager(addr);

        let first = managed::Manager::create(&manager).await;
        let Err(error) = first else {
            panic!("a 481 must refuse the login");
        };
        assert!(
            matches!(error, ConnectionError::AuthenticationFailed { .. }),
            "{error:?}"
        );
        assert_eq!(
            error.login_refusal(),
            Some(crate::connection_error::LoginRefusal::Credentials)
        );
        assert!(!error.is_connection_limit_refusal());
        assert!(matches!(
            managed::Manager::create(&manager).await,
            Err(ConnectionError::LoginBackoff { .. })
        ));
        assert_eq!(accepted.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn successful_probe_resumes_logins_and_resets_backoff() {
        let (addr, accepted) = spawn_login_server(|n| {
            if n == 0 {
                LIMIT_REACHED
            } else {
                "281 welcome\r\n"
            }
        })
        .await;
        let manager = login_manager(addr);

        assert!(managed::Manager::create(&manager).await.is_err());
        manager.login_backoff.expire_window();

        managed::Manager::create(&manager)
            .await
            .expect("probe login should be attempted and succeed");
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
        let after_success = managed::Manager::create(&manager).await;
        assert!(
            after_success.is_ok(),
            "logins must be open again after a successful probe: {after_success:?}"
        );
    }

    #[tokio::test]
    async fn network_errors_do_not_start_login_backoff() {
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = closed.local_addr().unwrap();
        drop(closed);
        let manager = login_manager(addr);

        for _ in 0..3 {
            let result = managed::Manager::create(&manager).await;
            assert!(matches!(result, Err(ConnectionError::IoError(_))));
        }
    }

    /// The backend account name and password must never appear in an
    /// error, a log line at any level, or `Debug` output.
    #[tokio::test]
    async fn refused_login_never_exposes_credentials() {
        const ACCOUNT: &str = "account-name-must-not-leak";
        const SECRET: &str = "password-must-not-leak";
        let (output, guard) = crate::logging::test_support::capture_json();
        let refusals: [fn(usize) -> &'static str; 2] = [
            |_| "481 authentication rejected\r\n",
            |_| "482 too many connections\r\n",
        ];
        for refusal in refusals {
            let (addr, _accepted) = spawn_login_server(refusal).await;
            let manager = TcpManager::new(
                addr.ip().to_string(),
                addr.port(),
                "backend-a".to_string(),
                TcpManagerOptions {
                    username: Some(ACCOUNT.to_string()),
                    password: Some(SECRET.to_string()),
                    compress: Some(false),
                    ..TcpManagerOptions::default()
                },
            )
            .unwrap();
            let error = managed::Manager::create(&manager).await.unwrap_err();
            for text in [error.to_string(), format!("{error:?}")] {
                assert!(!text.contains(ACCOUNT) && !text.contains(SECRET), "{text}");
            }
        }
        drop(guard);
        let logged = output.text();
        assert!(!logged.is_empty());
        assert!(
            !logged.contains(ACCOUNT) && !logged.contains(SECRET),
            "{logged}"
        );
    }

    #[test]
    fn pool_side_setup_timeouts_count_as_failed_connects() {
        let manager = login_manager("127.0.0.1:9".parse().unwrap());
        let timeout = ConnectionError::IoError(io::Error::from(io::ErrorKind::TimedOut));
        manager.report_setup_failure(&timeout);
        assert_eq!(manager.churn.snapshot().failed, 1);
    }

    #[test]
    fn authinfo_user_and_pass_share_the_481_precedence() {
        let manager = login_manager("127.0.0.1:9".parse().unwrap());
        for step in [AuthinfoStep::User, AuthinfoStep::Pass] {
            let error = manager.authinfo_refusal("481 too many connections", step);
            assert!(
                matches!(error, ConnectionError::AuthenticationFailed { .. }),
                "{step:?}: {error:?}"
            );
            assert_eq!(
                SetupFailure::of(&error),
                SetupFailure::LoginRefused,
                "{step:?}"
            );
            let limit = manager.authinfo_refusal("502 Too many connections", step);
            assert!(matches!(
                limit,
                ConnectionError::ConnectionLimitExceeded { .. }
            ));
        }
        assert!(matches!(
            manager.authinfo_refusal("500 what?", AuthinfoStep::User),
            ConnectionError::UnexpectedAuthResponse { .. }
        ));
    }

    #[test]
    fn setup_failures_are_classified_for_operators() {
        let backend = || "backend-a".to_string();
        let response = || "481 rejected".to_string();
        assert_eq!(
            SetupFailure::of(&ConnectionError::AuthenticationFailed {
                backend: backend(),
                response: response(),
            }),
            SetupFailure::LoginRefused
        );
        assert_eq!(
            SetupFailure::of(&ConnectionError::ConnectionLimitExceeded {
                backend: backend(),
                response: "482 too many connections".to_string(),
            }),
            SetupFailure::ConnectionLimit
        );
        assert_eq!(
            SetupFailure::of(&ConnectionError::PasswordRequired { backend: backend() }),
            SetupFailure::Misconfigured
        );
        assert_eq!(
            SetupFailure::of(&ConnectionError::IoError(io::Error::from(
                io::ErrorKind::ConnectionRefused
            ))),
            SetupFailure::Unreachable
        );
        assert_eq!(
            SetupFailure::of(&ConnectionError::ReaderModeRefused {
                backend: backend(),
                response: "502 reading not permitted".to_string(),
            }),
            SetupFailure::ReaderModeRefused
        );
        assert_eq!(
            SetupFailure::of(&ConnectionError::AuthenticationFailed {
                backend: backend(),
                response: "502 too many connections".to_string(),
            }),
            SetupFailure::ConnectionLimit,
            "a limit-style refusal must not look like bad credentials"
        );
        for (greeting, expected) in [
            ("400 too many connections", SetupFailure::ConnectionLimit),
            (
                "400 service temporarily unavailable",
                SetupFailure::Unavailable,
            ),
            ("502 service unavailable", SetupFailure::Unavailable),
        ] {
            assert_eq!(
                SetupFailure::of(&ConnectionError::InvalidGreeting {
                    backend: backend(),
                    greeting: greeting.to_string(),
                }),
                expected,
                "{greeting}"
            );
        }
    }

    /// The pool fills with a successful login inside the quiet window and
    /// then stays full: no further create or idle connection ever happens,
    /// yet the periodic settler clears the episode once the window passes.
    #[tokio::test(start_paused = true)]
    async fn episode_clears_after_window_when_pool_fills_and_stays_full() {
        let (output, guard) = crate::logging::test_support::capture_json();
        let manager = login_manager("127.0.0.1:9".parse().unwrap());
        let limit = ConnectionError::ConnectionLimitExceeded {
            backend: "login-backoff".to_string(),
            response: "482 too many connections".to_string(),
        };
        manager.events.failed(&manager, &limit, Duration::ZERO);

        tokio::time::sleep(Duration::from_secs(10)).await;
        // The create that filled the pool.
        manager.events.login_succeeded();
        manager.events.settle(&manager.name);
        assert!(output.events("backend_connection_limit_cleared").is_empty());

        tokio::time::sleep(Duration::from_secs(80)).await;
        drop(guard);
        let cleared = output.events("backend_connection_limit_cleared");
        assert_eq!(cleared.len(), 1, "{}", output.text());
        assert_eq!(cleared[0]["occurrences"], 1);
    }

    /// A fully busy pool has no idle connections, so health checks skip it;
    /// the connect that succeeded after the failure is enough evidence and
    /// the settler clears `backend_unreachable` without any health cycle.
    #[tokio::test(start_paused = true)]
    async fn unreachable_clears_with_a_fully_busy_pool() {
        let (output, guard) = crate::logging::test_support::capture_json();
        let manager = login_manager("127.0.0.1:9".parse().unwrap());
        let unreachable = ConnectionError::IoError(io::Error::from(io::ErrorKind::TimedOut));
        manager
            .events
            .failed(&manager, &unreachable, Duration::ZERO);
        tokio::time::sleep(Duration::from_secs(5)).await;
        manager.events.login_succeeded();

        tokio::time::sleep(Duration::from_secs(80)).await;
        drop(guard);
        assert_eq!(
            output.events("backend_unreachable_cleared").len(),
            1,
            "{}",
            output.text()
        );
    }

    /// Health checks passing on established connections clear
    /// `backend_unreachable` once the window has passed.
    #[tokio::test(start_paused = true)]
    async fn unreachable_clears_from_health_checks_without_new_sessions() {
        let (output, guard) = crate::logging::test_support::capture_json();
        let manager = login_manager("127.0.0.1:9".parse().unwrap());
        let unreachable = ConnectionError::IoError(io::Error::from(io::ErrorKind::TimedOut));
        manager
            .events
            .failed(&manager, &unreachable, Duration::ZERO);

        tokio::time::sleep(Duration::from_secs(30)).await;
        manager.note_healthy_connections();
        assert!(output.events("backend_unreachable_cleared").is_empty());

        tokio::time::sleep(Duration::from_secs(50)).await;
        drop(guard);
        assert_eq!(
            output.events("backend_unreachable_cleared").len(),
            1,
            "{}",
            output.text()
        );
    }

    /// Credentials revoked while the pool is full: established sessions
    /// keep passing health checks, but nothing proves a new login would be
    /// accepted, so the refusal must stay open until a login succeeds.
    #[tokio::test(start_paused = true)]
    async fn revoked_credentials_with_full_pool_do_not_clear_without_a_login() {
        let (output, guard) = crate::logging::test_support::capture_json();
        let manager = login_manager("127.0.0.1:9".parse().unwrap());
        let refused = ConnectionError::AuthenticationFailed {
            backend: "login-backoff".to_string(),
            response: "481 authentication rejected".to_string(),
        };
        manager
            .events
            .failed(&manager, &refused, Duration::from_secs(5));

        for _ in 0..10 {
            tokio::time::sleep(Duration::from_secs(60)).await;
            manager.note_healthy_connections();
        }
        assert!(
            output.events("backend_login_refused_cleared").is_empty(),
            "{}",
            output.text()
        );

        manager.events.login_succeeded();
        manager.events.settle(&manager.name);
        drop(guard);
        assert_eq!(output.events("backend_login_refused_cleared").len(), 1);
    }

    /// Some logins hit the account limit while others succeed: the log must
    /// stay at one line per window and only report recovery once failures
    /// have stopped for a full window.
    #[tokio::test(start_paused = true)]
    async fn partial_failure_does_not_alternate_failure_and_cleared_lines() {
        let (output, guard) = crate::logging::test_support::capture_json();
        let manager = login_manager("127.0.0.1:9".parse().unwrap());
        let limit = ConnectionError::ConnectionLimitExceeded {
            backend: "login-backoff".to_string(),
            response: "482 too many connections".to_string(),
        };

        // A refusal and a success every 10 s for two minutes.
        for _ in 0..12 {
            manager.events.failed(&manager, &limit, Duration::ZERO);
            manager.events.login_succeeded();
            manager.events.settle(&manager.name);
            tokio::time::advance(Duration::from_secs(10)).await;
        }
        assert!(output.events("backend_connection_limit_cleared").is_empty());

        tokio::time::advance(Duration::from_secs(60)).await;
        manager.events.settle(&manager.name);
        manager.events.settle(&manager.name);
        drop(guard);

        let failures = output.events("backend_connection_limit");
        assert_eq!(failures.len(), 2, "{}", output.text());
        assert_eq!(failures[0]["count"], 1);
        assert_eq!(failures[1]["count"], 6);
        let cleared = output.events("backend_connection_limit_cleared");
        assert_eq!(cleared.len(), 1, "{}", output.text());
        assert_eq!(cleared[0]["occurrences"], 12);
    }

    #[tokio::test]
    async fn refused_login_logs_one_event_and_no_premature_cleared_line() {
        let (output, guard) = crate::logging::test_support::capture_json();
        let (addr, _accepted) = spawn_login_server(|n| {
            if n == 0 {
                "481 authentication rejected\r\n"
            } else {
                "281 welcome\r\n"
            }
        })
        .await;
        let manager = login_manager(addr);

        assert!(managed::Manager::create(&manager).await.is_err());
        for _ in 0..3 {
            let gated = managed::Manager::create(&manager).await;
            assert!(matches!(gated, Err(ConnectionError::LoginBackoff { .. })));
        }
        manager.login_backoff.expire_window();
        managed::Manager::create(&manager).await.unwrap();
        managed::Manager::create(&manager).await.unwrap();
        drop(guard);

        let refused = output.events("backend_login_refused");
        assert_eq!(refused.len(), 1, "{}", output.text());
        assert_eq!(refused[0]["level"], "ERROR");
        assert_eq!(refused[0]["backend"], "login-backoff");
        assert_eq!(refused[0]["count"], 1);
        assert!(refused[0]["retry_in_secs"].as_u64().unwrap() > 0);

        // The refusal was moments ago, so the successes do not yet prove
        // recovery; see the partial-failure test for the cleared line.
        assert!(output.events("backend_login_refused_cleared").is_empty());
    }

    #[tokio::test]
    async fn repeated_connect_failures_log_one_unreachable_line() {
        let (output, guard) = crate::logging::test_support::capture_json();
        let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = closed.local_addr().unwrap();
        drop(closed);
        let manager = login_manager(addr);

        for _ in 0..5 {
            assert!(managed::Manager::create(&manager).await.is_err());
        }
        drop(guard);

        let lines = output.events("backend_unreachable");
        assert_eq!(lines.len(), 1, "{}", output.text());
        assert_eq!(lines[0]["level"], "WARN");
        assert_eq!(lines[0]["host"], "127.0.0.1");
        assert_eq!(lines[0]["count"], 1);
    }

    #[tokio::test]
    async fn one_probe_login_when_window_closes_with_many_waiters() {
        let (addr, accepted) = spawn_login_server(|_| LIMIT_REACHED).await;
        let manager = login_manager(addr);
        assert!(managed::Manager::create(&manager).await.is_err());
        manager.login_backoff.expire_window();

        let attempts: Vec<_> = (0..16)
            .map(|_| {
                let manager = manager.clone();
                tokio::spawn(async move { managed::Manager::create(&manager).await })
            })
            .collect();
        let (mut refused, mut gated) = (0, 0);
        for attempt in attempts {
            match attempt.await.unwrap() {
                Err(ConnectionError::ConnectionLimitExceeded { .. }) => refused += 1,
                Err(ConnectionError::LoginBackoff { .. }) => gated += 1,
                other => panic!("unexpected result: {other:?}"),
            }
        }

        assert_eq!(refused, 1, "exactly one caller may probe");
        assert_eq!(gated, 15);
        assert_eq!(accepted.load(Ordering::SeqCst), 2);
    }
}
