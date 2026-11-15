//! Spreading one backend's connections evenly over its resolved addresses.
//!
//! Some providers limit throughput per server address rather than per
//! account: one address serving 100 connections delivered about 75 MB/s, while
//! two addresses with 50 each delivered 99.5 MB/s. The busiest address then
//! sets the pace for the whole pool, so new connections go to the address with
//! the fewest live connections from this backend. A process-wide rotation
//! counter shared by every backend left one provider's four addresses at
//! 46/22/47/25 connections.

use smallvec::SmallVec;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

/// How long an address that failed to connect is passed over. An address
/// that silently drops connection attempts has no live connections, so
/// without this it would look emptiest and be chosen on every attempt, and
/// the setup deadline would expire before a healthy address was tried.
pub(crate) const FAILED_ADDRESS_COOLDOWN: Duration = Duration::from_secs(30);

/// Live connections per address for one backend.
#[derive(Debug, Default)]
pub(crate) struct AddressLoad {
    live: Mutex<SmallVec<[(SocketAddr, usize); 4]>>,
    /// Addresses whose last connect attempt failed or was abandoned, and
    /// until when they are passed over.
    cooling: Mutex<SmallVec<[(SocketAddr, Instant); 4]>>,
}

impl AddressLoad {
    /// Reserve the candidate with the fewest live connections, skipping any
    /// that `skip` rejects. Addresses cooling down after a failed connect are
    /// chosen only when no other candidate is left. Ties go to the earlier
    /// candidate. The reservation
    /// counts immediately, so concurrent connects spread out instead of all
    /// choosing the same idle address; dropping the lease releases it.
    pub(crate) fn reserve_least_loaded(
        self: &Arc<Self>,
        candidates: &[SocketAddr],
        mut skip: impl FnMut(SocketAddr) -> bool,
    ) -> Option<AddressLease> {
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count_of = |live: &SmallVec<[(SocketAddr, usize); 4]>, addr: SocketAddr| {
            live.iter()
                .find(|(known, _)| *known == addr)
                .map_or(0, |(_, count)| *count)
        };
        let now = Instant::now();
        let cooling = {
            let mut cooling = self
                .cooling
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            cooling.retain(|(_, until)| *until > now);
            cooling
                .iter()
                .map(|(addr, _)| *addr)
                .collect::<SmallVec<[_; 4]>>()
        };
        let chosen = candidates
            .iter()
            .copied()
            .filter(|addr| !skip(*addr))
            .min_by_key(|addr| (cooling.contains(addr), count_of(&live, *addr)))?;
        match live.iter_mut().find(|(known, _)| *known == chosen) {
            Some((_, count)) => *count += 1,
            None => live.push((chosen, 1)),
        }
        Some(AddressLease {
            load: Arc::clone(self),
            addr: chosen,
            connected: false,
        })
    }

    fn start_cooldown(&self, addr: SocketAddr) {
        let until = Instant::now() + FAILED_ADDRESS_COOLDOWN;
        let mut cooling = self
            .cooling
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match cooling.iter_mut().find(|(known, _)| *known == addr) {
            Some((_, existing)) => *existing = until,
            None => cooling.push((addr, until)),
        }
    }

    fn clear_cooldown(&self, addr: SocketAddr) {
        self.cooling
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(known, _)| *known != addr);
    }

    fn release(&self, addr: SocketAddr) {
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = live.iter().position(|(known, _)| *known == addr) {
            live[index].1 = live[index].1.saturating_sub(1);
            if live[index].1 == 0 {
                live.swap_remove(index);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn is_cooling(&self, addr: SocketAddr) -> bool {
        let now = Instant::now();
        self.cooling
            .lock()
            .unwrap()
            .iter()
            .any(|(known, until)| *known == addr && *until > now)
    }

    #[cfg(test)]
    pub(crate) fn live_count(&self, addr: SocketAddr) -> usize {
        self.live
            .lock()
            .unwrap()
            .iter()
            .find(|(known, _)| *known == addr)
            .map_or(0, |(_, count)| *count)
    }
}

/// One live (or connecting) connection's claim on its address.
///
/// A lease dropped before [`Self::mark_connected`] means the connection never
/// became a ready session (TCP, TLS, greeting or login failed, or the setup
/// deadline cancelled it), and puts the address on cooldown.
#[derive(Debug)]
pub(crate) struct AddressLease {
    load: Arc<AddressLoad>,
    addr: SocketAddr,
    connected: bool,
}

impl AddressLease {
    pub(crate) const fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Session setup completed; the address is healthy again.
    pub(crate) fn mark_connected(&mut self) {
        self.connected = true;
        self.load.clear_cooldown(self.addr);
    }
}

impl Drop for AddressLease {
    fn drop(&mut self) {
        if !self.connected {
            self.load.start_cooldown(self.addr);
        }
        self.load.release(self.addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addrs(n: u8) -> Vec<SocketAddr> {
        (1..=n)
            .map(|i| SocketAddr::from(([10, 0, 0, i], 563)))
            .collect()
    }

    #[test]
    fn connections_spread_evenly_and_refill_the_emptiest_address() {
        let load = Arc::new(AddressLoad::default());
        let candidates = addrs(4);
        let mut leases: Vec<_> = (0..140)
            .map(|_| connected(load.reserve_least_loaded(&candidates, |_| false).unwrap()))
            .collect();
        for addr in &candidates {
            assert_eq!(load.live_count(*addr), 35);
        }

        // Ten connections on the third address close; the next ten refill it.
        leases.retain({
            let mut dropped = 0;
            move |lease| {
                let drop_it = lease.addr() == candidates[2] && dropped < 10;
                dropped += usize::from(drop_it);
                !drop_it
            }
        });
        let candidates = addrs(4);
        assert_eq!(load.live_count(candidates[2]), 25);
        for _ in 0..10 {
            let lease = connected(load.reserve_least_loaded(&candidates, |_| false).unwrap());
            assert_eq!(lease.addr(), candidates[2]);
            leases.push(lease);
        }
        assert_eq!(leases.len(), 140);
    }

    fn connected(mut lease: AddressLease) -> AddressLease {
        lease.mark_connected();
        lease
    }

    #[test]
    fn a_closed_connection_frees_its_address_without_cooldown() {
        let load = Arc::new(AddressLoad::default());
        let candidates = addrs(2);
        let first = connected(load.reserve_least_loaded(&candidates, |_| false).unwrap());
        assert_eq!(first.addr(), candidates[0]);
        drop(first);
        assert_eq!(load.live_count(candidates[0]), 0);
        assert_eq!(
            load.reserve_least_loaded(&candidates, |_| false)
                .unwrap()
                .addr(),
            candidates[0],
            "a released address is the emptiest again"
        );
    }

    /// An address that silently drops connects has no live connections, but
    /// a failed or abandoned attempt must not make it the next choice: the
    /// setup deadline would expire on it every time.
    #[tokio::test(start_paused = true)]
    async fn a_failed_or_abandoned_connect_cools_the_address_down() {
        let load = Arc::new(AddressLoad::default());
        let candidates = addrs(2);
        let _healthy: Vec<_> = (0..4)
            .map(|_| {
                connected(
                    load.reserve_least_loaded(&candidates[1..], |_| false)
                        .unwrap(),
                )
            })
            .collect();

        let attempt = load.reserve_least_loaded(&candidates, |_| false).unwrap();
        assert_eq!(
            attempt.addr(),
            candidates[0],
            "the empty address is tried first"
        );
        drop(attempt); // connect failed or the setup deadline cancelled it

        let next = load.reserve_least_loaded(&candidates, |_| false).unwrap();
        assert_eq!(
            next.addr(),
            candidates[1],
            "the failed address is passed over"
        );
        drop(connected(next));
        assert_eq!(
            load.reserve_least_loaded(&candidates[..1], |_| false)
                .map(|lease| lease.addr()),
            Some(candidates[0]),
            "a cooling address is still used when it is the only one left"
        );

        tokio::time::advance(FAILED_ADDRESS_COOLDOWN).await;
        let retry = load.reserve_least_loaded(&candidates, |_| false).unwrap();
        assert_eq!(retry.addr(), candidates[0], "retried after the cooldown");
        drop(connected(retry));
        let again = load.reserve_least_loaded(&candidates, |_| false).unwrap();
        assert_eq!(
            again.addr(),
            candidates[0],
            "a successful connect clears the cooldown"
        );
    }

    #[test]
    fn skipped_addresses_are_never_chosen() {
        let load = Arc::new(AddressLoad::default());
        let candidates = addrs(3);
        let lease = load
            .reserve_least_loaded(&candidates, |addr| addr != candidates[2])
            .unwrap();
        assert_eq!(lease.addr(), candidates[2]);
        assert!(
            load.reserve_least_loaded(&candidates, |_| true).is_none(),
            "nothing left to try"
        );
    }

    #[test]
    fn addresses_gone_from_dns_keep_counting_until_their_connections_close() {
        let load = Arc::new(AddressLoad::default());
        let old = addrs(2);
        let held = connected(load.reserve_least_loaded(&old[1..], |_| false).unwrap());
        // DNS now returns only the first address; the second still has a live connection.
        let fresh = connected(load.reserve_least_loaded(&old[..1], |_| false).unwrap());
        assert_eq!(fresh.addr(), old[0]);
        assert_eq!(load.live_count(old[1]), 1);
        drop(held);
        assert_eq!(load.live_count(old[1]), 0);
    }
}
