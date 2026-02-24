use std::{collections::HashMap, time::Duration};

use libp2p::{Multiaddr, PeerId};

pub const BACKOFF_INITIAL: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);

pub struct ReconnectBackoff {
    delays: HashMap<PeerId, Duration>,
    pub addrs: HashMap<PeerId, Multiaddr>,
}

impl ReconnectBackoff {
    pub fn new(bootstrap: &[(PeerId, Multiaddr)]) -> Self {
        Self {
            delays: HashMap::new(),
            addrs: bootstrap.iter().cloned().collect(),
        }
    }

    pub fn schedule_reconnect(&mut self, peer: PeerId) -> Option<(Multiaddr, Duration)> {
        let addr = self.addrs.get(&peer)?;
        let delay = self.delays.entry(peer).or_insert(BACKOFF_INITIAL);
        let current = *delay;
        *delay = delay.saturating_mul(2).min(BACKOFF_MAX);
        Some((addr.clone(), current))
    }

    pub fn reset(&mut self, peer: &PeerId) {
        self.delays.remove(peer);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::test_utils::{arb_multiaddr, arb_peer_id};

    proptest! {
        #[test]
        fn first_call_returns_initial(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut backoff = ReconnectBackoff::new(&[(peer, addr.clone())]);
            let result = backoff.schedule_reconnect(peer);
            let (returned_addr, delay) = result.expect("known peer returns Some");
            prop_assert_eq!(returned_addr, addr);
            prop_assert_eq!(delay, BACKOFF_INITIAL);
        }

        #[test]
        fn exponential_growth_sequence(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
            n in 0usize..10,
        ) {
            let mut backoff = ReconnectBackoff::new(&[(peer, addr)]);
            for k in 0..=n {
                let (_, delay) = backoff.schedule_reconnect(peer)
                    .expect("known peer returns Some");
                let expected = BACKOFF_INITIAL.saturating_mul(1u32.checked_shl(u32::try_from(k)
                    .expect("k < 10 fits in u32")).unwrap_or(u32::MAX)).min(BACKOFF_MAX);
                prop_assert_eq!(delay, expected, "mismatch at step {}", k);
            }
        }

        #[test]
        fn delay_never_exceeds_max(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
            n in 1usize..20,
        ) {
            let mut backoff = ReconnectBackoff::new(&[(peer, addr)]);
            for _ in 0..n {
                let (_, delay) = backoff.schedule_reconnect(peer)
                    .expect("known peer returns Some");
                prop_assert!(delay <= BACKOFF_MAX, "delay {} exceeded max {}", delay.as_secs(), BACKOFF_MAX.as_secs());
            }
        }

        #[test]
        fn cap_is_stable(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let mut backoff = ReconnectBackoff::new(&[(peer, addr)]);
            for _ in 0..10 {
                backoff.schedule_reconnect(peer);
            }
            for _ in 0..5 {
                let (_, delay) = backoff.schedule_reconnect(peer)
                    .expect("known peer returns Some");
                prop_assert_eq!(delay, BACKOFF_MAX);
            }
        }

        #[test]
        fn reset_clears_delay(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
            n in 1usize..10,
        ) {
            let mut backoff = ReconnectBackoff::new(&[(peer, addr)]);
            for _ in 0..n {
                backoff.schedule_reconnect(peer);
            }
            backoff.reset(&peer);
            let (_, delay) = backoff.schedule_reconnect(peer)
                .expect("known peer returns Some after reset");
            prop_assert_eq!(delay, BACKOFF_INITIAL);
        }

        #[test]
        fn unknown_peer_returns_none(
            known_peer in arb_peer_id(),
            unknown_peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            prop_assume!(known_peer != unknown_peer);
            let mut backoff = ReconnectBackoff::new(&[(known_peer, addr)]);
            let result = backoff.schedule_reconnect(unknown_peer);
            prop_assert!(result.is_none());
        }

        #[test]
        fn independent_peer_delays(
            peer_a in arb_peer_id(),
            peer_b in arb_peer_id(),
            addr_a in arb_multiaddr(),
            addr_b in arb_multiaddr(),
        ) {
            prop_assume!(peer_a != peer_b);
            let mut backoff = ReconnectBackoff::new(&[(peer_a, addr_a), (peer_b, addr_b)]);

            for _ in 0..5 {
                backoff.schedule_reconnect(peer_a);
            }

            let (_, delay_b) = backoff.schedule_reconnect(peer_b)
                .expect("peer_b known");
            prop_assert_eq!(delay_b, BACKOFF_INITIAL);
        }
    }
}
