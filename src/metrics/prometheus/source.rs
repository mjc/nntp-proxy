//! Gathering a [`Scrape`] from a running proxy.

use super::process::ProcessStats;
use super::scrape::{BackendScrape, BuildInfo, CacheScrape, LoginGate, PoolScrape, Scrape};
use crate::NntpProxy;
use crate::types::BackendId;

/// Anything that can produce a complete exposition document on demand.
///
/// Rendering runs on a blocking thread, so implementations may take short
/// locks or read `/proc` without stalling the async workers.
pub trait ScrapeSource: Send + Sync + 'static {
    fn render(&self) -> String;
}

impl ScrapeSource for NntpProxy {
    fn render(&self) -> String {
        super::scrape::render(&gather(self))
    }
}

/// Read every exported value from the proxy's existing collectors.
#[must_use]
pub fn gather(proxy: &NntpProxy) -> Scrape {
    let metrics = proxy.metrics();
    let router = proxy.router();
    let cache = proxy.cache();
    let (hits, misses) = cache.lookup_counts();

    let mut seen_names = std::collections::HashSet::new();
    let backends = proxy
        .servers()
        .iter()
        .zip(proxy.connection_providers())
        .enumerate()
        .map(|(index, (server, provider))| {
            let backend_id = BackendId::from_index(index);
            let status = provider.status_counts();
            let health = provider.health_check_metrics();
            BackendScrape {
                name: unique_name(&mut seen_names, server.name.as_str(), index),
                tier: server.tier.to_string(),
                counters: metrics.backend_counters(backend_id).unwrap_or_default(),
                pool: PoolScrape {
                    size: status.size as u64,
                    idle: status.available as u64,
                    max_size: status.max_size as u64,
                    waiting: status.waiting as u64,
                    cooldowns: provider.active_cooldowns() as u64,
                },
                pending_commands: router
                    .backend_load(backend_id)
                    .map_or(0, |load| load.get() as u64),
                stateful_sessions: router
                    .stateful_count(backend_id)
                    .map_or(0, |count| count.get() as u64),
                churn: provider.connection_churn(),
                login_gate: if provider.login_backoff_active() {
                    LoginGate::BackingOff
                } else {
                    LoginGate::Open
                },
                health_checks: health.connections_checked(),
                health_check_failures: health.connections_failed(),
                first_byte: metrics.first_byte_histogram(backend_id),
                exchange_duration: provider.load().exchange_duration(),
            }
        })
        .collect();

    Scrape {
        build: BuildInfo::current(),
        process: ProcessStats::read(),
        proxy: metrics.proxy_counters(),
        cache: CacheScrape {
            entries: cache.entry_count(),
            size_bytes: cache.weighted_size(),
            capacity_bytes: cache.capacity(),
            hits,
            misses,
            disk: cache.disk_stats(),
        },
        backends,
    }
}

/// Two servers with the same name would export the same series twice, which
/// makes a scraper reject the whole response. The later one gets its index
/// appended instead.
fn unique_name(seen: &mut std::collections::HashSet<String>, name: &str, index: usize) -> String {
    if seen.insert(name.to_string()) {
        name.to_string()
    } else {
        let renamed = format!("{name}#{index}");
        seen.insert(renamed.clone());
        renamed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_server_names_get_their_index() {
        let mut seen = std::collections::HashSet::new();
        assert_eq!(unique_name(&mut seen, "backend-a", 0), "backend-a");
        assert_eq!(unique_name(&mut seen, "backend-b", 1), "backend-b");
        assert_eq!(unique_name(&mut seen, "backend-a", 2), "backend-a#2");
    }
}
