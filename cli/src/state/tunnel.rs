use std::collections::{HashMap, HashSet};

use libp2p::PeerId;

use super::{command::Command, event::Event, peer::Phase};
use crate::{
    nat_probe::NatMapping,
    specs::ServiceAddr,
    traits::MealyMachine,
    types::{KademliaKey, ServiceName},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelState {
    pending_by_peer: HashMap<PeerId, Vec<(ServiceName, ServiceAddr)>>,
    pending_by_service: HashMap<ServiceName, Vec<ServiceAddr>>,
    dialing: HashSet<PeerId>,
    awaiting_holepunch: HashMap<PeerId, Vec<(ServiceName, ServiceAddr)>>,
    relayed_peers: HashSet<PeerId>,
    holepunch_retry_attempts: HashMap<PeerId, u32>,
    pending_retry_redial: HashSet<PeerId>,
    nat_mapping: NatMapping,
}

impl Default for TunnelState {
    fn default() -> Self {
        Self::new()
    }
}

impl TunnelState {
    pub fn new() -> Self {
        Self {
            pending_by_peer: HashMap::new(),
            pending_by_service: HashMap::new(),
            dialing: HashSet::new(),
            awaiting_holepunch: HashMap::new(),
            relayed_peers: HashSet::new(),
            holepunch_retry_attempts: HashMap::new(),
            pending_retry_redial: HashSet::new(),
            nat_mapping: NatMapping::Unknown,
        }
    }

    pub fn set_nat_mapping(&mut self, mapping: NatMapping) {
        self.nat_mapping = mapping;
    }

    pub fn add_peer_tunnel(&mut self, peer: PeerId, service: ServiceName, bind: ServiceAddr) {
        self.pending_by_peer
            .entry(peer)
            .or_default()
            .push((service, bind));
    }

    pub fn add_service_tunnel(&mut self, service: ServiceName, bind: ServiceAddr) {
        self.pending_by_service
            .entry(service)
            .or_default()
            .push(bind);
    }

    pub fn has_pending_for(&self, peer: &PeerId) -> bool {
        self.pending_by_peer.contains_key(peer)
    }

    fn initiate_lookups(&self) -> Vec<Command> {
        self.pending_by_peer
            .keys()
            .map(|&peer| Command::DhtLookupPeer { peer })
            .chain(
                self.pending_by_service
                    .keys()
                    .map(|service_name| Command::DhtGetProviders {
                        service_name: service_name.clone(),
                        key: KademliaKey::for_service(service_name),
                    }),
            )
            .collect()
    }

    fn route_connected_peer(
        &mut self,
        peer: PeerId,
        specs: Vec<(ServiceName, ServiceAddr)>,
    ) -> Vec<Command> {
        if self.relayed_peers.contains(&peer) {
            self.queue_for_direct(peer, specs);
            Vec::new()
        } else {
            specs
                .into_iter()
                .map(|(service, bind)| Command::SpawnTunnel {
                    peer,
                    service,
                    bind,
                })
                .collect()
        }
    }

    fn queue_for_direct(&mut self, peer: PeerId, specs: Vec<(ServiceName, ServiceAddr)>) {
        self.awaiting_holepunch
            .entry(peer)
            .or_default()
            .extend(specs);
    }

    fn clear_retry_state(&mut self, peer: &PeerId) {
        self.holepunch_retry_attempts.remove(peer);
        self.pending_retry_redial.remove(peer);
    }

    fn schedule_holepunch_retry(&mut self, peer: PeerId) -> u32 {
        let attempts = self.holepunch_retry_attempts.entry(peer).or_insert(0);
        *attempts = attempts.saturating_add(1);
        self.pending_retry_redial.insert(peer);
        *attempts
    }
}

impl MealyMachine for TunnelState {
    type Event = Event;
    type Command = Command;

    fn transition(mut self, event: Event) -> (Self, Vec<Command>) {
        let mut commands = Vec::new();

        match event {
            Event::PhaseChanged {
                new: Phase::Ready, ..
            } => {
                commands.push(Command::PublishServices);
                commands.extend(self.initiate_lookups());
            }

            Event::RelayReservationAccepted { .. } => {
                commands.push(Command::PublishServices);
            }

            Event::DhtPeerLookupComplete { peer, connected } => match connected {
                true => {
                    if let Some(specs) = self.pending_by_peer.remove(&peer) {
                        commands.extend(self.route_connected_peer(peer, specs));
                    }
                }
                false => {
                    if self.pending_by_peer.contains_key(&peer) {
                        self.dialing.insert(peer);
                        commands.push(Command::DialPeer { peer });
                    }
                }
            },

            Event::DhtServiceResolved {
                service_name,
                provider,
                connected,
            } => {
                if let Some(bind_addrs) = self.pending_by_service.remove(&service_name) {
                    let specs: Vec<(ServiceName, ServiceAddr)> = bind_addrs
                        .into_iter()
                        .map(|addr| (service_name.clone(), addr))
                        .collect();

                    match connected {
                        true => {
                            commands.extend(self.route_connected_peer(provider, specs));
                        }
                        false => {
                            self.pending_by_peer
                                .entry(provider)
                                .or_default()
                                .extend(specs);
                            commands.push(Command::DhtLookupPeer { peer: provider });
                        }
                    }
                }
            }

            Event::DhtServiceFailed { service_name, .. } => {
                self.pending_by_service.remove(&service_name);
            }

            Event::TunnelPeerConnected { peer, relayed } => {
                self.dialing.remove(&peer);
                match relayed {
                    true => {
                        self.relayed_peers.insert(peer);

                        if let Some(specs) = self.pending_by_peer.remove(&peer) {
                            if !self.nat_mapping.is_holepunch_viable() {
                                tracing::warn!(
                                    peer = %peer,
                                    nat = %self.nat_mapping,
                                    "direct tunnel blocked: relayed-only connection and hole punching not viable"
                                );
                            }
                            self.queue_for_direct(peer, specs);
                        }
                    }
                    false => {
                        self.relayed_peers.remove(&peer);
                        self.clear_retry_state(&peer);
                        commands.extend(
                            self.pending_by_peer
                                .remove(&peer)
                                .into_iter()
                                .chain(self.awaiting_holepunch.remove(&peer))
                                .flatten()
                                .map(|(service, bind)| Command::SpawnTunnel {
                                    peer,
                                    service,
                                    bind,
                                }),
                        );
                    }
                }
            }

            Event::HolePunchSucceeded { .. } => {}

            Event::HolePunchFailed {
                remote_peer,
                ref reason,
            } => {
                if self.awaiting_holepunch.contains_key(&remote_peer) {
                    let attempt = self.schedule_holepunch_retry(remote_peer);
                    tracing::warn!(
                        peer = %remote_peer,
                        %reason,
                        attempt,
                        retry_policy = "unbounded event-driven",
                        "hole punch failed, reconnecting relayed path for retry"
                    );
                    commands.push(Command::DisconnectPeer { peer: remote_peer });
                }
            }

            Event::HolePunchTimeout { peer } => {
                if self.awaiting_holepunch.contains_key(&peer) {
                    let attempt = self.schedule_holepunch_retry(peer);
                    tracing::warn!(
                        peer = %peer,
                        attempt,
                        retry_policy = "unbounded event-driven",
                        "hole punch timeout, reconnecting relayed path for retry"
                    );
                    commands.push(Command::DisconnectPeer { peer });
                }
            }

            Event::ConnectionLost {
                peer,
                remaining_connections: 0,
            } => {
                self.relayed_peers.remove(&peer);
                self.dialing.remove(&peer);

                if self.pending_retry_redial.remove(&peer) {
                    self.dialing.insert(peer);
                    commands.push(Command::DialPeer { peer });
                    tracing::info!(%peer, "redialing peer for hole-punch retry");
                } else if self.awaiting_holepunch.contains_key(&peer) {
                    tracing::debug!(
                        %peer,
                        "all connections lost while waiting for direct path; awaiting next reconnect event"
                    );
                } else {
                    self.clear_retry_state(&peer);
                }
            }

            Event::PeerIdentified { .. }
            | Event::PhaseChanged { .. }
            | Event::ListeningOn { .. }
            | Event::BootstrapConnected { .. }
            | Event::ShutdownRequested
            | Event::KademliaBootstrapOk
            | Event::KademliaBootstrapFailed { .. }
            | Event::MdnsDiscovered { .. }
            | Event::MdnsExpired { .. }
            | Event::NatStatusChanged(_)
            | Event::DiscoveryTimeout
            | Event::RelayReservationFailed { .. }
            | Event::ConnectionLost { .. }
            | Event::NoBootstrapPeers
            | Event::ExternalAddrConfirmed { .. }
            | Event::ExternalAddrExpired { .. } => {}
        }

        (self, commands)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::test_utils::{
        arb_multiaddr, arb_nat_status, arb_peer_id, arb_phase, arb_service_addr, arb_service_name,
    };

    proptest! {
        #[test]
        fn phase_changed_to_ready_publishes(
            old in arb_phase(),
        ) {
            prop_assume!(old != Phase::Ready);
            let state = TunnelState::new();
            let (_, commands) = state.transition(Event::PhaseChanged {
                old,
                new: Phase::Ready,
            });
            prop_assert!(commands.contains(&Command::PublishServices));
        }

        #[test]
        fn phase_changed_to_ready_initiates_lookups(
            old in arb_phase(),
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
            svc_name in arb_service_name(),
            svc_bind in arb_service_addr(),
        ) {
            prop_assume!(old != Phase::Ready);
            let mut state = TunnelState::new();
            state.add_peer_tunnel(peer, service, bind);
            state.add_service_tunnel(svc_name.clone(), svc_bind);

            let (_, commands) = state.transition(Event::PhaseChanged {
                old,
                new: Phase::Ready,
            });

            prop_assert!(commands.contains(&Command::PublishServices));
            let has_peer_lookup = commands.iter().any(|c| matches!(
                c, Command::DhtLookupPeer { peer: p } if *p == peer
            ));
            prop_assert!(has_peer_lookup);
            let has_svc_lookup = commands.iter().any(|c| matches!(
                c, Command::DhtGetProviders { .. }
            ));
            prop_assert!(has_svc_lookup);
        }

        #[test]
        fn phase_changed_non_ready_is_noop(
            old in arb_phase(),
            new in arb_phase(),
        ) {
            prop_assume!(new != Phase::Ready);
            let state = TunnelState::new();
            let (_, commands) = state.transition(Event::PhaseChanged { old, new });
            prop_assert!(commands.is_empty());
        }

        #[test]
        fn relay_accepted_publishes(relay_peer in arb_peer_id()) {
            let state = TunnelState::new();
            let (_, commands) = state.transition(Event::RelayReservationAccepted { relay_peer });
            prop_assert!(commands.contains(&Command::PublishServices));
        }

        #[test]
        fn dht_peer_lookup_spawns_when_connected(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.add_peer_tunnel(peer, service.clone(), bind.clone());

            let (state, commands) = state.transition(Event::DhtPeerLookupComplete {
                peer, connected: true,
            });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind,
            });
            prop_assert!(has_spawn);
            prop_assert!(!state.has_pending_for(&peer));
        }

        #[test]
        fn dht_peer_lookup_dials_when_disconnected(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.add_peer_tunnel(peer, service, bind);

            let (state, commands) = state.transition(Event::DhtPeerLookupComplete {
                peer, connected: false,
            });

            let has_dial = commands.contains(&Command::DialPeer { peer });
            prop_assert!(has_dial);
            prop_assert!(state.dialing.contains(&peer));
            prop_assert!(state.has_pending_for(&peer));
        }

        #[test]
        fn dht_service_resolved_spawns_when_connected(
            service in arb_service_name(),
            bind in arb_service_addr(),
            provider in arb_peer_id(),
        ) {
            let mut state = TunnelState::new();
            state.add_service_tunnel(service.clone(), bind.clone());

            let (_, commands) = state.transition(Event::DhtServiceResolved {
                service_name: service.clone(),
                provider,
                connected: true,
            });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer: provider, service, bind,
            });
            prop_assert!(has_spawn);
        }

        #[test]
        fn dht_service_resolved_initiates_peer_lookup_when_disconnected(
            service in arb_service_name(),
            bind in arb_service_addr(),
            provider in arb_peer_id(),
        ) {
            let mut state = TunnelState::new();
            state.add_service_tunnel(service.clone(), bind);

            let (state, commands) = state.transition(Event::DhtServiceResolved {
                service_name: service,
                provider,
                connected: false,
            });

            let has_lookup = commands.contains(&Command::DhtLookupPeer { peer: provider });
            prop_assert!(has_lookup);
            prop_assert!(state.has_pending_for(&provider));
        }

        #[test]
        fn tunnel_peer_connected_direct_spawns_immediately(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.add_peer_tunnel(peer, service.clone(), bind.clone());

            let (_, commands) = state.transition(Event::TunnelPeerConnected {
                peer,
                relayed: false,
            });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer, service, bind,
            });
            prop_assert!(has_spawn);
        }

        #[test]
        fn tunnel_peer_connected_relayed_waits_for_holepunch(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.add_peer_tunnel(peer, service, bind);

            let (state, commands) = state.transition(Event::TunnelPeerConnected {
                peer, relayed: true,
            });

            prop_assert!(commands.is_empty());
            prop_assert!(state.awaiting_holepunch.contains_key(&peer));
        }

        #[test]
        fn hole_punch_succeeded_keeps_waiting_tunnels(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.awaiting_holepunch.insert(peer, vec![(service.clone(), bind.clone())]);

            let (state, commands) = state.transition(Event::HolePunchSucceeded {
                remote_peer: peer,
            });

            let _ = (service, bind);
            prop_assert!(commands.is_empty());
            prop_assert!(state.awaiting_holepunch.contains_key(&peer));
        }

        #[test]
        fn hole_punch_failed_triggers_disconnect_retry(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
            reason in "[a-z ]{1,20}",
        ) {
            let mut state = TunnelState::new();
            state.awaiting_holepunch.insert(peer, vec![(service.clone(), bind.clone())]);

            let (state, commands) = state.transition(Event::HolePunchFailed {
                remote_peer: peer, reason,
            });

            let has_disconnect = commands.contains(&Command::DisconnectPeer { peer });
            prop_assert!(has_disconnect);
            prop_assert!(state.awaiting_holepunch.contains_key(&peer));
            prop_assert!(state.pending_retry_redial.contains(&peer));
            prop_assert_eq!(state.holepunch_retry_attempts.get(&peer).copied(), Some(1));
        }

        #[test]
        fn peer_lifecycle_events_are_noop(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
            nat_status in arb_nat_status(),
        ) {
            let state = TunnelState::new();

            let (_, c) = state.clone().transition(Event::ListeningOn { addr: addr.clone() });
            prop_assert!(c.is_empty());

            let (_, c) = state.clone().transition(Event::BootstrapConnected {
                peer, addr,
            });
            prop_assert!(c.is_empty());

            let (_, c) = state.clone().transition(Event::NatStatusChanged(nat_status));
            prop_assert!(c.is_empty());

            let (_, c) = state.clone().transition(Event::KademliaBootstrapOk);
            prop_assert!(c.is_empty());

            let (_, c) = state.clone().transition(Event::DiscoveryTimeout);
            prop_assert!(c.is_empty());

            let (_, c) = state.clone().transition(Event::ShutdownRequested);
            prop_assert!(c.is_empty());
        }

        // ─── Relay-aware tunnel tests ────────────────────────────────────

        #[test]
        fn dht_peer_lookup_relayed_awaits_holepunch(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);
            state.add_peer_tunnel(peer, service.clone(), bind.clone());

            let (state, commands) = state.transition(Event::DhtPeerLookupComplete {
                peer, connected: true,
            });

            prop_assert!(commands.is_empty());
            prop_assert!(!state.pending_by_peer.contains_key(&peer));
            prop_assert!(state.awaiting_holepunch.contains_key(&peer));
            let waiting = state.awaiting_holepunch
                .get(&peer)
                .expect("peer should be in awaiting_holepunch after relayed lookup");
            prop_assert!(waiting.contains(&(service, bind)));
        }

        #[test]
        fn dht_service_resolved_relayed_awaits_holepunch(
            service in arb_service_name(),
            bind in arb_service_addr(),
            provider in arb_peer_id(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(provider);
            state.add_service_tunnel(service.clone(), bind.clone());

            let (state, commands) = state.transition(Event::DhtServiceResolved {
                service_name: service.clone(),
                provider,
                connected: true,
            });

            prop_assert!(commands.is_empty());
            prop_assert!(state.awaiting_holepunch.contains_key(&provider));
            let waiting = state.awaiting_holepunch
                .get(&provider)
                .expect("provider should be in awaiting_holepunch after relayed resolve");
            prop_assert!(waiting.contains(&(service, bind)));
        }

        #[test]
        fn holepunch_failed_then_connection_lost_redials(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
            reason in "[a-z ]{1,20}",
        ) {
            let mut state = TunnelState::new();
            state.awaiting_holepunch
                .insert(peer, vec![(service.clone(), bind.clone())]);

            let (state, fail_cmds) = state.transition(Event::HolePunchFailed {
                remote_peer: peer,
                reason,
            });
            let has_disconnect = fail_cmds.contains(&Command::DisconnectPeer { peer });
            prop_assert!(has_disconnect);

            let (state, reconnect_cmds) = state.transition(Event::ConnectionLost {
                peer,
                remaining_connections: 0,
            });

            let has_redial = reconnect_cmds.contains(&Command::DialPeer { peer });
            prop_assert!(has_redial);
            prop_assert!(state.awaiting_holepunch.contains_key(&peer));
            prop_assert!(!state.pending_retry_redial.contains(&peer));
            prop_assert_eq!(state.holepunch_retry_attempts.get(&peer).copied(), Some(1));
        }

        #[test]
        fn holepunch_failed_keeps_retrying_after_many_attempts(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
            reason in "[a-z ]{1,20}",
        ) {
            let mut state = TunnelState::new();
            state.awaiting_holepunch
                .insert(peer, vec![(service, bind)]);
            state
                .holepunch_retry_attempts
                .insert(peer, 1_000);

            let (state, commands) = state.transition(Event::HolePunchFailed {
                remote_peer: peer,
                reason,
            });

            let has_disconnect = commands.contains(&Command::DisconnectPeer { peer });
            prop_assert!(has_disconnect);
            prop_assert!(state.pending_retry_redial.contains(&peer));
            prop_assert_eq!(
                state.holepunch_retry_attempts.get(&peer).copied(),
                Some(1_001)
            );
        }

        #[test]
        fn holepunch_timeout_keeps_waiting_tunnels(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.awaiting_holepunch
                .insert(peer, vec![(service.clone(), bind.clone())]);

            let (state, commands) = state.transition(Event::HolePunchTimeout { peer });

            let has_disconnect = commands.contains(&Command::DisconnectPeer { peer });
            prop_assert!(has_disconnect);
            let waiting = state
                .awaiting_holepunch
                .get(&peer)
                .expect("peer remains queued after hole punch timeout");
            prop_assert!(waiting.contains(&(service, bind)));
            prop_assert!(state.pending_retry_redial.contains(&peer));
        }

        #[test]
        fn holepunch_succeeded_waits_for_direct_connection(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);
            state.awaiting_holepunch
                .insert(peer, vec![(service.clone(), bind.clone())]);

            let (state, commands) = state.transition(Event::HolePunchSucceeded {
                remote_peer: peer,
            });

            prop_assert!(commands.is_empty());
            prop_assert!(state.awaiting_holepunch.contains_key(&peer));
        }

        #[test]
        fn direct_connection_spawns_from_awaiting_holepunch(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);
            state.awaiting_holepunch.insert(peer, vec![(service.clone(), bind.clone())]);

            let (state, commands) = state.transition(Event::TunnelPeerConnected {
                peer,
                relayed: false,
            });

            let has_spawn = commands.contains(&Command::SpawnTunnel {
                peer,
                service,
                bind,
            });
            prop_assert!(has_spawn);
            prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
            prop_assert!(!state.relayed_peers.contains(&peer));
        }

        #[test]
        fn connection_lost_preserves_waiting_tunnels_without_retry_signal(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);
            state.awaiting_holepunch.insert(peer, vec![(service, bind)]);

            let (state, commands) = state.transition(Event::ConnectionLost {
                peer,
                remaining_connections: 0,
            });

            prop_assert!(commands.is_empty());
            prop_assert!(!state.relayed_peers.contains(&peer));
            prop_assert!(state.awaiting_holepunch.contains_key(&peer));
        }

        #[test]
        fn direct_connection_clears_relayed_after_retry(peer in arb_peer_id()) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);
            state.holepunch_retry_attempts.insert(peer, 2);
            state.pending_retry_redial.insert(peer);

            let (state, _) = state.transition(Event::TunnelPeerConnected {
                peer,
                relayed: false,
            });

            prop_assert!(!state.relayed_peers.contains(&peer));
            prop_assert!(!state.pending_retry_redial.contains(&peer));
            prop_assert!(!state.holepunch_retry_attempts.contains_key(&peer));
        }

        // ─── Symmetric NAT gating tests ────────────────────────────────

        #[test]
        fn relayed_peer_queues_when_symmetric_nat(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.set_nat_mapping(NatMapping::AddressDependent);
            state.relayed_peers.insert(peer);
            state.add_peer_tunnel(peer, service.clone(), bind.clone());

            let (state, commands) = state.transition(Event::DhtPeerLookupComplete {
                peer,
                connected: true,
            });

            prop_assert!(commands.is_empty());
            prop_assert!(state.awaiting_holepunch.contains_key(&peer));
        }

        #[test]
        fn relayed_peer_awaits_holepunch_when_cone_nat(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.set_nat_mapping(NatMapping::EndpointIndependent);
            state.relayed_peers.insert(peer);
            state.add_peer_tunnel(peer, service.clone(), bind.clone());

            let (state, commands) = state.transition(Event::DhtPeerLookupComplete {
                peer,
                connected: true,
            });

            prop_assert!(commands.is_empty());
            prop_assert!(state.awaiting_holepunch.contains_key(&peer));
        }

        #[test]
        fn tunnel_connected_relayed_queues_when_symmetric(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.set_nat_mapping(NatMapping::AddressDependent);
            state.add_peer_tunnel(peer, service.clone(), bind.clone());

            let (state, commands) = state.transition(Event::TunnelPeerConnected {
                peer,
                relayed: true,
            });

            prop_assert!(commands.is_empty());
            let waiting = state
                .awaiting_holepunch
                .get(&peer)
                .expect("peer should be queued after relayed connection");
            prop_assert!(waiting.contains(&(service, bind)));
        }

    }
}
