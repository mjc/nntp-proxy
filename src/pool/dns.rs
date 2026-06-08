use hickory_resolver::TokioResolver;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

use crate::connection_error::ConnectionError;

#[derive(Debug, Clone)]
pub(super) struct DnsCacheEntry {
    pub(super) addrs: Arc<[SocketAddr]>,
    pub(super) valid_until: Instant,
}

impl DnsCacheEntry {
    fn is_fresh(&self, now: Instant) -> bool {
        now < self.valid_until
    }
}

#[derive(Debug, Clone)]
pub(super) struct DnsCache {
    host: String,
    port: u16,
    backend_name: String,
    literal_socket_addrs: Option<Arc<[SocketAddr]>>,
    dns_resolver: Option<Arc<TokioResolver>>,
    cached_socket_addrs: Arc<RwLock<Option<DnsCacheEntry>>>,
}

impl DnsCache {
    pub(super) fn new(
        host: String,
        port: u16,
        backend_name: String,
    ) -> Result<Self, ConnectionError> {
        let literal_socket_addrs = host
            .parse::<IpAddr>()
            .ok()
            .map(|ip| Arc::from([SocketAddr::new(ip, port)]));
        let dns_resolver = if literal_socket_addrs.is_some() {
            None
        } else {
            Some(Arc::new(
                TokioResolver::builder_tokio()
                    .map_err(|error| ConnectionError::dns_resolver_init(&backend_name, error))?
                    .build()
                    .map_err(|error| ConnectionError::dns_resolver_build(&backend_name, error))?,
            ))
        };

        Ok(Self {
            host,
            port,
            backend_name,
            literal_socket_addrs,
            dns_resolver,
            cached_socket_addrs: Arc::new(RwLock::new(None)),
        })
    }

    fn dns_address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    fn literal_socket_addrs(&self) -> Option<Arc<[SocketAddr]>> {
        self.literal_socket_addrs.clone()
    }

    pub(super) fn is_ip_literal(&self) -> bool {
        self.literal_socket_addrs.is_some()
    }

    async fn lookup_socket_addrs(&self) -> Result<DnsCacheEntry, ConnectionError> {
        let Some(dns_resolver) = self.dns_resolver.as_ref() else {
            return Err(ConnectionError::DnsNoAddresses {
                address: self.dns_address(),
            });
        };

        let lookup = dns_resolver
            .lookup_ip(self.host.as_str())
            .await
            .map_err(|error| {
                ConnectionError::dns_lookup(&self.backend_name, self.dns_address(), error)
            })?;
        let addrs = lookup
            .iter()
            .map(|ip_addr| SocketAddr::new(ip_addr, self.port))
            .collect::<Vec<_>>();
        if addrs.is_empty() {
            return Err(ConnectionError::DnsNoAddresses {
                address: self.dns_address(),
            });
        }

        Ok(DnsCacheEntry {
            addrs: Arc::from(addrs),
            valid_until: lookup.valid_until(),
        })
    }

    pub(super) async fn resolve_socket_addrs(&self) -> Result<Arc<[SocketAddr]>, ConnectionError> {
        if let Some(addrs) = self.literal_socket_addrs() {
            return Ok(addrs);
        }

        let cached = self.cached_entry().await;
        if let Some(cache_entry) = cached.as_ref()
            && cache_entry.is_fresh(Instant::now())
        {
            return Ok(cache_entry.addrs.clone());
        }

        self.refresh_socket_addrs_with_fallback(cached).await
    }

    pub(super) async fn refresh_socket_addrs(&self) -> Result<Arc<[SocketAddr]>, ConnectionError> {
        if let Some(addrs) = self.literal_socket_addrs() {
            return Ok(addrs);
        }

        self.refresh_socket_addrs_with_fallback(self.cached_entry().await)
            .await
    }

    async fn cached_entry(&self) -> Option<DnsCacheEntry> {
        self.cached_socket_addrs.read().await.as_ref().cloned()
    }

    async fn refresh_socket_addrs_with_fallback(
        &self,
        cached: Option<DnsCacheEntry>,
    ) -> Result<Arc<[SocketAddr]>, ConnectionError> {
        match self.lookup_socket_addrs().await {
            Ok(cache_entry) => Ok(self.cache_entry(cache_entry).await),
            Err(error) => {
                if let Some(cache_entry) = cached {
                    tracing::debug!(
                        backend = %self.backend_name,
                        host = %self.host,
                        error = %error,
                        "DNS refresh failed; reusing cached backend socket addresses"
                    );
                    Ok(cache_entry.addrs)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn cache_entry(&self, cache_entry: DnsCacheEntry) -> Arc<[SocketAddr]> {
        let addrs = cache_entry.addrs.clone();
        *self.cached_socket_addrs.write().await = Some(cache_entry);
        addrs
    }

    pub(super) async fn remove_cached_ipv6_socket_addrs(&self) {
        let mut cached_addrs = self.cached_socket_addrs.write().await;
        let Some(cache_entry) = cached_addrs.as_ref() else {
            return;
        };

        let ipv4_addrs = cache_entry
            .addrs
            .iter()
            .copied()
            .filter(SocketAddr::is_ipv4)
            .collect::<Vec<_>>();
        if ipv4_addrs.len() == cache_entry.addrs.len() {
            return;
        }

        *cached_addrs = if ipv4_addrs.is_empty() {
            None
        } else {
            Some(DnsCacheEntry {
                addrs: Arc::<[SocketAddr]>::from(ipv4_addrs),
                valid_until: cache_entry.valid_until,
            })
        };
    }

    #[cfg(test)]
    pub(super) async fn set_cached_entry_for_test(&self, entry: Option<DnsCacheEntry>) {
        *self.cached_socket_addrs.write().await = entry;
    }

    #[cfg(test)]
    pub(super) async fn cached_entry_for_test(&self) -> Option<DnsCacheEntry> {
        self.cached_entry().await
    }

    #[cfg(test)]
    pub(super) fn shares_cache_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.cached_socket_addrs, &other.cached_socket_addrs)
    }
}

#[cfg(test)]
pub(super) fn dns_cache_entry(addrs: Arc<[SocketAddr]>, valid_until: Instant) -> DnsCacheEntry {
    DnsCacheEntry { addrs, valid_until }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;

    fn test_dns_cache(host: &str, port: u16) -> DnsCache {
        DnsCache::new(host.to_string(), port, "DnsBackend".to_string()).unwrap()
    }

    async fn seed_cache(dns_cache: &DnsCache, addrs: Arc<[SocketAddr]>, valid_until: Instant) {
        dns_cache
            .set_cached_entry_for_test(Some(dns_cache_entry(addrs, valid_until)))
            .await;
    }

    #[tokio::test]
    async fn remove_cached_ipv6_socket_addrs_keeps_only_ipv4_addresses() {
        let dns_cache = test_dns_cache("test.example.com", 563);
        let ipv6_addr = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 563));
        let ipv4_addr = SocketAddr::from(([127, 0, 0, 1], 563));
        seed_cache(
            &dns_cache,
            Arc::from([ipv6_addr, ipv4_addr]),
            Instant::now() + Duration::from_secs(60),
        )
        .await;

        dns_cache.remove_cached_ipv6_socket_addrs().await;

        let cached_addrs = dns_cache
            .cached_entry_for_test()
            .await
            .expect("cached addresses should remain initialized")
            .addrs;
        assert_eq!(&*cached_addrs, &[ipv4_addr]);
    }

    #[tokio::test]
    async fn remove_cached_ipv6_socket_addrs_clears_ipv6_only_cache() {
        let dns_cache = test_dns_cache("test.example.com", 563);
        let ipv6_addr = SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], 563));
        seed_cache(
            &dns_cache,
            Arc::from([ipv6_addr]),
            Instant::now() + Duration::from_secs(60),
        )
        .await;

        dns_cache.remove_cached_ipv6_socket_addrs().await;

        assert!(
            dns_cache.cached_entry_for_test().await.is_none(),
            "IPv6-only cached address list should be cleared instead of retained as an empty cache"
        );
    }

    #[tokio::test]
    async fn resolve_socket_addrs_reuses_unexpired_cache_entry() {
        let stale_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stale_addr = stale_listener.local_addr().unwrap();
        drop(stale_listener);

        let dns_cache = test_dns_cache("localhost", stale_addr.port());
        seed_cache(
            &dns_cache,
            Arc::from([stale_addr]),
            Instant::now() + Duration::from_secs(60),
        )
        .await;

        let resolved = dns_cache.resolve_socket_addrs().await.unwrap();

        assert_eq!(&*resolved, &[stale_addr]);
    }

    #[tokio::test]
    async fn resolve_socket_addrs_refreshes_expired_cache_entry() {
        let live_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = live_listener.local_addr().unwrap();
        drop(live_listener);

        let stale_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stale_addr = stale_listener.local_addr().unwrap();
        drop(stale_listener);

        let dns_cache = test_dns_cache("localhost", live_addr.port());
        seed_cache(
            &dns_cache,
            Arc::from([stale_addr]),
            Instant::now() - Duration::from_secs(1),
        )
        .await;

        let resolved = dns_cache.resolve_socket_addrs().await.unwrap();

        assert!(
            resolved.contains(&live_addr),
            "expired cache entries should trigger a fresh DNS lookup"
        );
        let cache_entry = dns_cache
            .cached_entry_for_test()
            .await
            .expect("refreshed DNS result should be cached");
        assert!(cache_entry.valid_until > Instant::now());
    }

    #[tokio::test]
    async fn resolve_socket_addrs_uses_unexpired_cache_when_dns_is_unavailable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = listener.local_addr().unwrap();
        drop(listener);

        let dns_cache = test_dns_cache("ttl-test.invalid", live_addr.port());
        seed_cache(
            &dns_cache,
            Arc::from([live_addr]),
            Instant::now() + Duration::from_secs(60),
        )
        .await;

        let resolved = dns_cache.resolve_socket_addrs().await.unwrap();

        assert_eq!(&*resolved, &[live_addr]);
    }

    #[tokio::test]
    async fn resolve_socket_addrs_uses_expired_cache_when_dns_is_unavailable() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = listener.local_addr().unwrap();
        drop(listener);

        let dns_cache = test_dns_cache("ttl-test.invalid", live_addr.port());
        seed_cache(
            &dns_cache,
            Arc::from([live_addr]),
            Instant::now() - Duration::from_secs(1),
        )
        .await;

        let resolved = dns_cache.resolve_socket_addrs().await.unwrap();

        assert_eq!(&*resolved, &[live_addr]);
    }

    #[tokio::test]
    async fn resolve_socket_addrs_keeps_expired_fallback_cached_for_future_refresh_attempts() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_addr = listener.local_addr().unwrap();
        drop(listener);

        let expired_deadline = Instant::now() - Duration::from_secs(1);
        let dns_cache = test_dns_cache("ttl-test.invalid", live_addr.port());
        seed_cache(&dns_cache, Arc::from([live_addr]), expired_deadline).await;

        let resolved_once = dns_cache.resolve_socket_addrs().await.unwrap();
        let cached_entry = dns_cache
            .cached_entry_for_test()
            .await
            .expect("expired fallback should remain cached");
        let resolved_twice = dns_cache.resolve_socket_addrs().await.unwrap();

        assert_eq!(&*resolved_once, &[live_addr]);
        assert_eq!(&*resolved_twice, &[live_addr]);
        assert_eq!(
            cached_entry.valid_until, expired_deadline,
            "expired fallback should remain expired so the next reuse attempts DNS refresh again"
        );
    }

    #[tokio::test]
    async fn resolve_socket_addrs_returns_literal_addresses_without_dns_state() {
        let dns_cache = test_dns_cache("127.0.0.1", 8119);

        let resolved = dns_cache.resolve_socket_addrs().await.unwrap();
        let refreshed = dns_cache.refresh_socket_addrs().await.unwrap();

        assert_eq!(&*resolved, &[SocketAddr::from(([127, 0, 0, 1], 8119))]);
        assert_eq!(&*refreshed, &[SocketAddr::from(([127, 0, 0, 1], 8119))]);
        assert!(dns_cache.cached_entry_for_test().await.is_none());
    }
}
