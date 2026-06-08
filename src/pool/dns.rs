use hickory_resolver::TokioResolver;
use hickory_resolver::proto::rr::RecordType;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use crate::connection_error::ConnectionError;

#[derive(Debug, Clone)]
pub(super) struct DnsCache {
    host: String,
    port: u16,
    backend_name: String,
    literal_socket_addrs: Option<Arc<[SocketAddr]>>,
    dns_resolver: Option<Arc<TokioResolver>>,
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
        })
    }

    pub(super) fn is_ip_literal(&self) -> bool {
        self.literal_socket_addrs.is_some()
    }

    pub(super) async fn resolve_socket_addrs(&self) -> Result<Arc<[SocketAddr]>, ConnectionError> {
        if let Some(addrs) = self.literal_socket_addrs() {
            return Ok(addrs);
        }

        self.lookup_socket_addrs().await
    }

    pub(super) async fn refresh_socket_addrs(&self) -> Result<Arc<[SocketAddr]>, ConnectionError> {
        if let Some(addrs) = self.literal_socket_addrs() {
            return Ok(addrs);
        }

        self.clear_lookup_cache();
        self.lookup_socket_addrs().await
    }

    pub(super) fn remove_cached_ipv6_socket_addrs(&self) {
        self.clear_lookup_cache();
    }

    fn literal_socket_addrs(&self) -> Option<Arc<[SocketAddr]>> {
        self.literal_socket_addrs.clone()
    }

    async fn lookup_socket_addrs(&self) -> Result<Arc<[SocketAddr]>, ConnectionError> {
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

        Ok(Arc::from(addrs))
    }

    fn clear_lookup_cache(&self) {
        let Some(dns_resolver) = self.dns_resolver.as_ref() else {
            return;
        };

        dns_resolver.clear_lookup_cache(self.host.as_str(), RecordType::A);
        dns_resolver.clear_lookup_cache(self.host.as_str(), RecordType::AAAA);
    }

    fn dns_address(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    #[cfg(test)]
    pub(super) fn shares_resolver_with(&self, other: &Self) -> bool {
        match (&self.dns_resolver, &other.dns_resolver) {
            (Some(lhs), Some(rhs)) => Arc::ptr_eq(lhs, rhs),
            (None, None) => self.literal_socket_addrs == other.literal_socket_addrs,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dns_cache(host: &str, port: u16) -> DnsCache {
        DnsCache::new(host.to_string(), port, "DnsBackend".to_string()).unwrap()
    }

    fn assert_contains_loopback(addrs: &[SocketAddr]) {
        assert!(
            addrs.iter().any(|addr| addr.ip().is_loopback()),
            "expected at least one loopback address, got {addrs:?}"
        );
    }

    #[tokio::test]
    async fn resolve_socket_addrs_returns_literal_addresses_without_dns() {
        let dns_cache = test_dns_cache("127.0.0.1", 8119);

        let resolved = dns_cache.resolve_socket_addrs().await.unwrap();

        assert_eq!(&*resolved, &[SocketAddr::from(([127, 0, 0, 1], 8119))]);
    }

    #[tokio::test]
    async fn refresh_socket_addrs_returns_literal_addresses_without_dns() {
        let dns_cache = test_dns_cache("127.0.0.1", 8119);

        let refreshed = dns_cache.refresh_socket_addrs().await.unwrap();

        assert_eq!(&*refreshed, &[SocketAddr::from(([127, 0, 0, 1], 8119))]);
    }

    #[tokio::test]
    async fn resolve_socket_addrs_resolves_localhost() {
        let dns_cache = test_dns_cache("localhost", 8119);

        let resolved = dns_cache.resolve_socket_addrs().await.unwrap();

        assert_contains_loopback(&resolved);
    }

    #[tokio::test]
    async fn refresh_socket_addrs_resolves_localhost() {
        let dns_cache = test_dns_cache("localhost", 8119);

        let refreshed = dns_cache.refresh_socket_addrs().await.unwrap();

        assert_contains_loopback(&refreshed);
    }

    #[test]
    fn is_ip_literal_matches_host_kind() {
        assert!(test_dns_cache("127.0.0.1", 119).is_ip_literal());
        assert!(!test_dns_cache("localhost", 119).is_ip_literal());
    }
}
