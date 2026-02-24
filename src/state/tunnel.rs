use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use libp2p::{Multiaddr, PeerId, multiaddr::Protocol};

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
    service_provider_candidates: HashMap<ServiceName, Vec<PeerId>>,
    awaiting_holepunch: HashMap<PeerId, Vec<(ServiceName, ServiceAddr)>>,
    relayed_peers: HashSet<PeerId>,
    holepunch_retry_attempts: HashMap<PeerId, u32>,
    pending_retry_redial: HashSet<PeerId>,
    holepunch_sessions: HashMap<PeerId, HolePunchSession>,
    address_oracle: HashMap<PeerId, PeerAddressOracle>,
    address_last_seen: HashMap<PeerId, HashMap<Multiaddr, Instant>>,
    nat_mapping: NatMapping,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HolePunchSession {
    attempt_id: u64,
    phase: HolePunchPhase,
    relay_redial_addrs: Vec<Multiaddr>,
    direct_snapshot: Vec<Multiaddr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HolePunchPhase {
    Punching,
    WaitingDisconnect,
    WaitingRedial,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct PeerAddressOracle {
    relay_addrs: Vec<Multiaddr>,
    direct_addrs: Vec<Multiaddr>,
}

const ADDRESS_CANDIDATE_TTL: Duration = Duration::from_secs(45);
const MAX_DIRECT_CANDIDATES: usize = 8;
const MAX_RELAY_CANDIDATES: usize = 4;
const MAX_REDIAL_ADDRESSES: usize = 12;
// Keep retries bounded, but allow one additional reconnect cycle to improve
// direct-upgrade success when external UDP mappings churn mid-attempt.
const MAX_RELAY_RECONNECT_RETRIES: u32 = 3;

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
            service_provider_candidates: HashMap::new(),
            awaiting_holepunch: HashMap::new(),
            relayed_peers: HashSet::new(),
            holepunch_retry_attempts: HashMap::new(),
            pending_retry_redial: HashSet::new(),
            holepunch_sessions: HashMap::new(),
            address_oracle: HashMap::new(),
            address_last_seen: HashMap::new(),
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

    fn store_provider_candidates(&mut self, service_name: &ServiceName, providers: Vec<PeerId>) {
        let providers = dedup_peers(providers);
        if providers.is_empty() {
            self.service_provider_candidates.remove(service_name);
        } else {
            self.service_provider_candidates
                .insert(service_name.clone(), providers);
        }
    }

    fn next_fallback_provider(
        &mut self,
        service_name: &ServiceName,
        exclude_peer: PeerId,
    ) -> Option<PeerId> {
        let providers = self.service_provider_candidates.get_mut(service_name)?;
        while let Some(next_peer) = providers.first().copied() {
            providers.remove(0);
            if next_peer != exclude_peer {
                if providers.is_empty() {
                    self.service_provider_candidates.remove(service_name);
                }
                return Some(next_peer);
            }
        }
        self.service_provider_candidates.remove(service_name);
        None
    }

    fn reroute_specs_after_dial_failure(&mut self, peer: PeerId, commands: &mut Vec<Command>) {
        let Some(specs) = self.pending_by_peer.remove(&peer) else {
            return;
        };

        let mut reassign_to_peer: HashMap<PeerId, Vec<(ServiceName, ServiceAddr)>> = HashMap::new();
        let mut refresh_services: HashSet<ServiceName> = HashSet::new();

        for (service_name, bind_addr) in specs {
            if let Some(next_peer) = self.next_fallback_provider(&service_name, peer) {
                reassign_to_peer
                    .entry(next_peer)
                    .or_default()
                    .push((service_name, bind_addr));
            } else {
                self.pending_by_service
                    .entry(service_name.clone())
                    .or_default()
                    .push(bind_addr);
                refresh_services.insert(service_name);
            }
        }

        for (next_peer, next_specs) in reassign_to_peer {
            self.pending_by_peer
                .entry(next_peer)
                .or_default()
                .extend(next_specs);
            commands.push(Command::DhtLookupPeer { peer: next_peer });
        }

        for service_name in refresh_services {
            commands.push(Command::DhtGetProviders {
                key: KademliaKey::for_service(&service_name),
                service_name,
            });
        }
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
        self.holepunch_sessions.remove(peer);
    }

    fn schedule_holepunch_retry(&mut self, peer: PeerId) -> u32 {
        let attempts = self.holepunch_retry_attempts.entry(peer).or_insert(0);
        *attempts = attempts.saturating_add(1);
        self.pending_retry_redial.insert(peer);
        *attempts
    }

    fn record_address_candidates(
        &mut self,
        peer: PeerId,
        listen_addrs: &[Multiaddr],
        now: Instant,
    ) {
        let should_prune_entry = {
            let last_seen = self.address_last_seen.entry(peer).or_default();
            for addr in listen_addrs {
                if has_p2p_circuit(addr) || is_supported_direct_candidate(addr) {
                    last_seen.insert(addr.clone(), now);
                }
            }
            last_seen.retain(|_, seen_at| {
                now.saturating_duration_since(*seen_at) <= ADDRESS_CANDIDATE_TTL
            });
            last_seen.is_empty()
        };
        if should_prune_entry {
            self.address_last_seen.remove(&peer);
        }
    }

    fn fresh_candidates(
        &self,
        peer: &PeerId,
        now: Instant,
        predicate: impl Fn(&Multiaddr) -> bool,
        limit: usize,
    ) -> Vec<Multiaddr> {
        let Some(last_seen) = self.address_last_seen.get(peer) else {
            return Vec::new();
        };
        let mut ranked: Vec<(Multiaddr, Instant)> = last_seen
            .iter()
            .filter_map(|(addr, seen_at)| {
                (now.saturating_duration_since(*seen_at) <= ADDRESS_CANDIDATE_TTL
                    && predicate(addr))
                .then_some((addr.clone(), *seen_at))
            })
            .collect();
        ranked.sort_by(|(a_addr, a_seen), (b_addr, b_seen)| {
            b_seen
                .cmp(a_seen)
                .then_with(|| a_addr.to_string().cmp(&b_addr.to_string()))
        });
        ranked.truncate(limit);
        ranked.into_iter().map(|(addr, _)| addr).collect()
    }

    fn prioritized_redial_addrs(
        &self,
        peer: &PeerId,
        session: &HolePunchSession,
    ) -> Vec<Multiaddr> {
        let now = Instant::now();
        let oracle = self.address_oracle.get(peer).cloned().unwrap_or_default();
        let fresh_direct = self.fresh_candidates(
            peer,
            now,
            is_supported_direct_candidate,
            MAX_DIRECT_CANDIDATES,
        );
        let fresh_relay = self.fresh_candidates(peer, now, has_p2p_circuit, MAX_RELAY_CANDIDATES);

        let mut addrs = Vec::new();
        // Try most likely direct paths first, then fall back to relay paths.
        addrs.extend(session.direct_snapshot.iter().cloned());
        addrs.extend(oracle.direct_addrs);
        addrs.extend(fresh_direct);
        addrs.extend(session.relay_redial_addrs.iter().cloned());
        addrs.extend(oracle.relay_addrs);
        addrs.extend(fresh_relay);
        let mut addrs = dedup_addrs(addrs);
        addrs.truncate(MAX_REDIAL_ADDRESSES);
        addrs
    }

    fn update_address_oracle(&mut self, peer: PeerId, listen_addrs: Vec<Multiaddr>) {
        let now = Instant::now();
        self.record_address_candidates(peer, &listen_addrs, now);
        let relay_addrs = self.fresh_candidates(&peer, now, has_p2p_circuit, MAX_RELAY_CANDIDATES);
        let direct_addrs = self.fresh_candidates(
            &peer,
            now,
            is_supported_direct_candidate,
            MAX_DIRECT_CANDIDATES,
        );
        let previous = self.address_oracle.get(&peer).cloned();
        let relay_changed = previous
            .as_ref()
            .is_none_or(|oracle| oracle.relay_addrs != relay_addrs);
        let direct_changed = previous
            .as_ref()
            .is_none_or(|oracle| oracle.direct_addrs != direct_addrs);
        if relay_changed || direct_changed {
            tracing::debug!(
                %peer,
                relay_addrs = ?relay_addrs,
                direct_addrs = ?direct_addrs,
                "hole-punch address oracle updated"
            );
        }
        if let Some(session) = self.holepunch_sessions.get_mut(&peer) {
            let backfilled_relay = session.relay_redial_addrs.is_empty() && !relay_addrs.is_empty();
            let backfilled_direct = session.direct_snapshot.is_empty() && !direct_addrs.is_empty();
            let refreshed_relay = !session.relay_redial_addrs.is_empty()
                && !relay_addrs.is_empty()
                && session.relay_redial_addrs != relay_addrs;
            let refreshed_direct = !session.direct_snapshot.is_empty()
                && !direct_addrs.is_empty()
                && session.direct_snapshot != direct_addrs;
            if backfilled_relay || refreshed_relay {
                session.relay_redial_addrs = relay_addrs.clone();
            }
            if backfilled_direct || refreshed_direct {
                session.direct_snapshot = direct_addrs.clone();
            }
            if backfilled_relay || backfilled_direct {
                tracing::debug!(
                    %peer,
                    attempt_id = session.attempt_id,
                    backfilled_relay,
                    backfilled_direct,
                    relay_snapshot = ?session.relay_redial_addrs,
                    direct_snapshot = ?session.direct_snapshot,
                    "hole-punch attempt snapshots backfilled from identify"
                );
            } else if refreshed_relay || refreshed_direct {
                tracing::debug!(
                    %peer,
                    attempt_id = session.attempt_id,
                    phase = ?session.phase,
                    refreshed_relay,
                    refreshed_direct,
                    relay_snapshot = ?session.relay_redial_addrs,
                    direct_snapshot = ?session.direct_snapshot,
                    "hole-punch attempt snapshots refreshed from newer identify data"
                );
            }
        }
        self.address_oracle.insert(
            peer,
            PeerAddressOracle {
                relay_addrs,
                direct_addrs,
            },
        );
    }

    fn begin_holepunch_attempt(&mut self, peer: PeerId, attempt_id: u64) {
        let oracle = self.address_oracle.get(&peer).cloned().unwrap_or_default();
        tracing::debug!(
            %peer,
            attempt_id,
            relay_snapshot = ?oracle.relay_addrs,
            direct_snapshot = ?oracle.direct_addrs,
            "hole-punch attempt snapshots initialized"
        );
        self.holepunch_sessions.insert(
            peer,
            HolePunchSession {
                attempt_id,
                phase: HolePunchPhase::Punching,
                relay_redial_addrs: oracle.relay_addrs,
                direct_snapshot: oracle.direct_addrs,
            },
        );
    }

    fn retry_holepunch_attempt(
        &mut self,
        peer: PeerId,
        attempt_id: u64,
        reason: &str,
        commands: &mut Vec<Command>,
    ) {
        let Some(session) = self.holepunch_sessions.get(&peer) else {
            tracing::debug!(
                %peer,
                attempt_id,
                %reason,
                "ignoring hole-punch retry event without active session"
            );
            return;
        };
        let expected_attempt = session.attempt_id;
        if expected_attempt != attempt_id {
            tracing::debug!(
                %peer,
                expected_attempt,
                received_attempt = attempt_id,
                %reason,
                "ignoring stale hole-punch retry event"
            );
            return;
        }
        if !matches!(session.phase, HolePunchPhase::Punching) {
            tracing::debug!(
                %peer,
                attempt_id,
                %reason,
                "ignoring duplicate hole-punch retry event for non-punching phase"
            );
            return;
        }
        if !self.awaiting_holepunch.contains_key(&peer) {
            tracing::debug!(
                %peer,
                attempt_id,
                %reason,
                "ignoring hole-punch retry event without pending direct-only tunnels"
            );
            return;
        }
        let relay_snapshot = session.relay_redial_addrs.clone();
        let direct_snapshot = session.direct_snapshot.clone();
        let retries_so_far = self
            .holepunch_retry_attempts
            .get(&peer)
            .copied()
            .unwrap_or(0);
        if retries_so_far >= MAX_RELAY_RECONNECT_RETRIES {
            let fallback_specs = self.awaiting_holepunch.remove(&peer).unwrap_or_default();
            let fallback_tunnels = fallback_specs.len();
            commands.extend(fallback_specs.into_iter().map(|(service, bind)| {
                Command::SpawnTunnel {
                    peer,
                    service,
                    bind,
                }
            }));
            self.clear_retry_state(&peer);
            tracing::warn!(
                %peer,
                attempt_id,
                retries_so_far,
                %reason,
                relay_snapshot = ?relay_snapshot,
                direct_snapshot = ?direct_snapshot,
                fallback_tunnels,
                retry_policy = "bounded reconnect; retain relay and fallback",
                "hole punch did not succeed; continuing with relayed tunnel path"
            );
            return;
        }
        let attempt = self.schedule_holepunch_retry(peer);
        if let Some(session) = self.holepunch_sessions.get_mut(&peer) {
            session.phase = HolePunchPhase::WaitingDisconnect;
        }
        let oracle = self.address_oracle.get(&peer).cloned().unwrap_or_default();
        tracing::info!(
            %peer,
            attempt_id,
            attempt,
            %reason,
            relay_snapshot = ?relay_snapshot,
            direct_snapshot = ?direct_snapshot,
            oracle_relay = ?oracle.relay_addrs,
            oracle_direct = ?oracle.direct_addrs,
            retry_policy = "bounded reconnect",
            "hole punch retry requested, reconnecting relayed path"
        );
        commands.push(Command::DisconnectPeer { peer });
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
                        commands.push(Command::DialPeer { peer });
                    }
                }
            },

            Event::DhtServiceResolved {
                service_name,
                providers,
            } => {
                if let Some(bind_addrs) = self.pending_by_service.remove(&service_name) {
                    if providers.is_empty() {
                        self.pending_by_service
                            .insert(service_name.clone(), bind_addrs);
                        commands.push(Command::DhtGetProviders {
                            key: KademliaKey::for_service(&service_name),
                            service_name,
                        });
                        return (self, commands);
                    }

                    let mut providers = dedup_provider_candidates(providers);
                    let (provider, connected) = providers.remove(0);
                    let fallback_providers: Vec<PeerId> =
                        providers.into_iter().map(|(peer, _)| peer).collect();
                    self.store_provider_candidates(&service_name, fallback_providers);

                    let specs: Vec<(ServiceName, ServiceAddr)> = bind_addrs
                        .into_iter()
                        .map(|addr| (service_name.clone(), addr))
                        .collect();

                    if connected {
                        commands.extend(self.route_connected_peer(provider, specs));
                    } else {
                        self.pending_by_peer
                            .entry(provider)
                            .or_default()
                            .extend(specs);
                        commands.push(Command::DhtLookupPeer { peer: provider });
                    }
                }
            }

            Event::DhtServiceFailed { service_name, .. } => {
                if self.pending_by_service.contains_key(&service_name) {
                    commands.push(Command::DhtGetProviders {
                        key: KademliaKey::for_service(&service_name),
                        service_name,
                    });
                }
            }

            Event::TunnelDialFailed { peer, reason } => {
                tracing::warn!(%peer, %reason, "tunnel dial failed");
                if self.awaiting_holepunch.contains_key(&peer)
                    && let Some(session) = self.holepunch_sessions.get(&peer).cloned()
                {
                    let addrs = self.prioritized_redial_addrs(&peer, &session);
                    if addrs.is_empty() {
                        commands.push(Command::DialPeer { peer });
                    } else {
                        commands.push(Command::DialPeerWithAddrs {
                            peer,
                            addrs,
                            attempt_id: session.attempt_id,
                        });
                    }
                } else {
                    self.reroute_specs_after_dial_failure(peer, &mut commands);
                }
            }

            Event::PeerIdentified {
                peer, listen_addrs, ..
            } => {
                self.update_address_oracle(peer, listen_addrs);
            }

            Event::TunnelPeerConnected { peer, relayed } => match relayed {
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
            },

            Event::HolePunchAttemptStarted { peer, attempt_id } => {
                self.begin_holepunch_attempt(peer, attempt_id);
            }

            Event::HolePunchFailed { remote_peer, .. } => {
                tracing::debug!(
                    peer = %remote_peer,
                    "ignoring legacy hole-punch failure event"
                );
            }

            Event::HolePunchAttemptFailed {
                remote_peer,
                attempt_id,
                reason,
            } => {
                self.retry_holepunch_attempt(remote_peer, attempt_id, &reason, &mut commands);
            }

            Event::HolePunchAttemptTimeout { peer, attempt_id } => {
                self.retry_holepunch_attempt(peer, attempt_id, "hole punch timeout", &mut commands);
            }

            Event::ConnectionLost {
                peer,
                remaining_connections: 0,
            } => {
                self.relayed_peers.remove(&peer);

                if self.pending_retry_redial.remove(&peer) {
                    let session_snapshot = self.holepunch_sessions.get(&peer).cloned();
                    if let Some(session) = self.holepunch_sessions.get_mut(&peer) {
                        session.phase = HolePunchPhase::WaitingRedial;
                    }
                    let command = match session_snapshot {
                        Some(session) => {
                            let addrs = self.prioritized_redial_addrs(&peer, &session);
                            if addrs.is_empty() {
                                Command::DialPeer { peer }
                            } else {
                                Command::DialPeerWithAddrs {
                                    peer,
                                    addrs,
                                    attempt_id: session.attempt_id,
                                }
                            }
                        }
                        None => Command::DialPeer { peer },
                    };
                    commands.push(command);
                    tracing::debug!(%peer, "redialing peer for hole-punch retry");
                } else if let Some(waiting_specs) = self.awaiting_holepunch.remove(&peer) {
                    self.clear_retry_state(&peer);
                    self.pending_by_peer
                        .entry(peer)
                        .or_default()
                        .extend(waiting_specs);
                    commands.push(Command::DialPeer { peer });
                    tracing::info!(
                        %peer,
                        "all connections lost while waiting for direct path; redialing peer"
                    );
                } else {
                    self.clear_retry_state(&peer);
                }
            }

            Event::PhaseChanged { .. }
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

fn dedup_addrs(addrs: impl IntoIterator<Item = Multiaddr>) -> Vec<Multiaddr> {
    let mut seen = HashSet::new();
    addrs
        .into_iter()
        .filter(|addr| seen.insert(addr.clone()))
        .collect()
}

fn dedup_peers(peers: impl IntoIterator<Item = PeerId>) -> Vec<PeerId> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for peer in peers {
        if seen.insert(peer) {
            deduped.push(peer);
        }
    }
    deduped
}

fn dedup_provider_candidates(
    providers: impl IntoIterator<Item = (PeerId, bool)>,
) -> Vec<(PeerId, bool)> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for (peer, connected) in providers {
        if seen.insert(peer) {
            deduped.push((peer, connected));
        }
    }
    deduped
}

fn has_p2p_circuit(addr: &Multiaddr) -> bool {
    addr.iter()
        .any(|proto| matches!(proto, Protocol::P2pCircuit))
}

fn has_udp_and_quic(addr: &Multiaddr) -> bool {
    let mut has_udp = false;
    let mut has_quic = false;
    for proto in addr.iter() {
        match proto {
            Protocol::Udp(_) => has_udp = true,
            Protocol::QuicV1 => has_quic = true,
            Protocol::Quic => has_quic = true,
            _ => {}
        }
    }
    has_udp && has_quic
}

fn is_supported_direct_candidate(addr: &Multiaddr) -> bool {
    !has_p2p_circuit(addr) && has_udp_and_quic(addr) && crate::external_addr::has_public_ip(addr)
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use libp2p::{Multiaddr, PeerId};
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
                providers: vec![(provider, true)],
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
                providers: vec![(provider, false)],
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
        fn hole_punch_failed_legacy_event_is_noop(
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

            prop_assert!(commands.is_empty());
            prop_assert!(state.awaiting_holepunch.contains_key(&peer));
            prop_assert!(!state.pending_retry_redial.contains(&peer));
            prop_assert_eq!(state.holepunch_retry_attempts.get(&peer).copied(), None);
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
                providers: vec![(provider, true)],
            });

            prop_assert!(commands.is_empty());
            prop_assert!(state.awaiting_holepunch.contains_key(&provider));
            let waiting = state.awaiting_holepunch
                .get(&provider)
                .expect("provider should be in awaiting_holepunch after relayed resolve");
            prop_assert!(waiting.contains(&(service, bind)));
        }

        #[test]
        fn holepunch_failed_legacy_then_connection_lost_recovers_with_redial(
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
            prop_assert!(fail_cmds.is_empty());

            let (state, reconnect_cmds) = state.transition(Event::ConnectionLost {
                peer,
                remaining_connections: 0,
            });

            let has_redial = reconnect_cmds
                .iter()
                .any(|c| matches!(c, Command::DialPeer { peer: p } if *p == peer));
            prop_assert!(has_redial);
            prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
            let pending = state
                .pending_by_peer
                .get(&peer)
                .expect("peer should be requeued for redial");
            prop_assert!(pending.contains(&(service, bind)));
            prop_assert!(!state.pending_retry_redial.contains(&peer));
            prop_assert_eq!(state.holepunch_retry_attempts.get(&peer).copied(), None);
        }

        #[test]
        fn holepunch_failed_legacy_ignores_existing_retry_counter(
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

            prop_assert!(commands.is_empty());
            prop_assert!(!state.pending_retry_redial.contains(&peer));
            prop_assert_eq!(
                state.holepunch_retry_attempts.get(&peer).copied(),
                Some(1_000)
            );
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
        fn connection_lost_requeues_waiting_tunnels_and_redials(
            peer in arb_peer_id(),
            service in arb_service_name(),
            bind in arb_service_addr(),
        ) {
            let mut state = TunnelState::new();
            state.relayed_peers.insert(peer);
            state.awaiting_holepunch.insert(peer, vec![(service.clone(), bind.clone())]);

            let (state, commands) = state.transition(Event::ConnectionLost {
                peer,
                remaining_connections: 0,
            });

            let has_redial = commands
                .iter()
                .any(|c| matches!(c, Command::DialPeer { peer: p } if *p == peer));
            prop_assert!(has_redial);
            prop_assert!(!state.relayed_peers.contains(&peer));
            prop_assert!(!state.awaiting_holepunch.contains_key(&peer));
            let pending = state
                .pending_by_peer
                .get(&peer)
                .expect("peer should be requeued for redial");
            prop_assert!(pending.contains(&(service, bind)));
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

    #[test]
    fn stale_attempt_failure_is_ignored() {
        let peer = PeerId::random();
        let service = ServiceName::new("svc");
        let bind: ServiceAddr = "localhost:2222".parse().expect("valid service addr");

        let mut state = TunnelState::new();
        state.awaiting_holepunch.insert(peer, vec![(service, bind)]);
        let (state, _) = state.transition(Event::HolePunchAttemptStarted {
            peer,
            attempt_id: 2,
        });

        let (state, commands) = state.transition(Event::HolePunchAttemptFailed {
            remote_peer: peer,
            attempt_id: 1,
            reason: "stale".to_string(),
        });

        assert!(commands.is_empty());
        assert!(!state.pending_retry_redial.contains(&peer));
    }

    #[test]
    fn current_attempt_timeout_requests_disconnect() {
        let peer = PeerId::random();
        let service = ServiceName::new("svc");
        let bind: ServiceAddr = "localhost:2222".parse().expect("valid service addr");

        let mut state = TunnelState::new();
        state.awaiting_holepunch.insert(peer, vec![(service, bind)]);
        let (state, _) = state.transition(Event::HolePunchAttemptStarted {
            peer,
            attempt_id: 7,
        });

        let (state, commands) = state.transition(Event::HolePunchAttemptTimeout {
            peer,
            attempt_id: 7,
        });

        assert!(commands.contains(&Command::DisconnectPeer { peer }));
        assert!(state.pending_retry_redial.contains(&peer));
    }

    #[test]
    fn dht_service_failed_retries_lookup_when_intent_is_pending() {
        let service = ServiceName::new("svc");
        let bind: ServiceAddr = "localhost:2222".parse().expect("valid service addr");
        let mut state = TunnelState::new();
        state.add_service_tunnel(service.clone(), bind);

        let (_, commands) = state.transition(Event::DhtServiceFailed {
            service_name: service.clone(),
            reason: "timeout".to_string(),
        });

        assert!(commands.contains(&Command::DhtGetProviders {
            service_name: service.clone(),
            key: KademliaKey::for_service(&service),
        }));
    }

    #[test]
    fn tunnel_dial_failed_reroutes_to_fallback_provider() {
        let service = ServiceName::new("svc");
        let bind: ServiceAddr = "localhost:2222".parse().expect("valid service addr");
        let primary = PeerId::random();
        let fallback = PeerId::random();
        assert_ne!(primary, fallback);

        let mut state = TunnelState::new();
        state.add_service_tunnel(service.clone(), bind.clone());

        let (state, first_commands) = state.transition(Event::DhtServiceResolved {
            service_name: service.clone(),
            providers: vec![(primary, false), (fallback, false)],
        });
        assert!(first_commands.contains(&Command::DhtLookupPeer { peer: primary }));
        assert!(state.has_pending_for(&primary));

        let (state, fail_commands) = state.transition(Event::TunnelDialFailed {
            peer: primary,
            reason: "dial refused".to_string(),
        });
        assert!(fail_commands.contains(&Command::DhtLookupPeer { peer: fallback }));
        assert!(!state.has_pending_for(&primary));
        assert!(state.has_pending_for(&fallback));
        let pending = state
            .pending_by_peer
            .get(&fallback)
            .expect("fallback provider should receive pending tunnel spec");
        assert!(pending.contains(&(service, bind)));
    }

    #[test]
    fn tunnel_dial_failed_requeries_service_when_no_fallback_provider() {
        let service = ServiceName::new("svc");
        let bind: ServiceAddr = "localhost:2222".parse().expect("valid service addr");
        let primary = PeerId::random();

        let mut state = TunnelState::new();
        state.add_service_tunnel(service.clone(), bind.clone());

        let (state, first_commands) = state.transition(Event::DhtServiceResolved {
            service_name: service.clone(),
            providers: vec![(primary, false)],
        });
        assert!(first_commands.contains(&Command::DhtLookupPeer { peer: primary }));
        assert!(state.has_pending_for(&primary));

        let (state, fail_commands) = state.transition(Event::TunnelDialFailed {
            peer: primary,
            reason: "timeout".to_string(),
        });
        assert!(!state.has_pending_for(&primary));
        assert!(fail_commands.contains(&Command::DhtGetProviders {
            service_name: service.clone(),
            key: KademliaKey::for_service(&service),
        }));
        let pending_binds = state
            .pending_by_service
            .get(&service)
            .expect("service tunnel intent should be preserved for retry");
        assert_eq!(pending_binds.as_slice(), &[bind]);
    }

    #[test]
    fn tunnel_dial_failed_during_holepunch_redials_with_attempt_addresses() {
        let peer = PeerId::random();
        let service = ServiceName::new("svc");
        let bind: ServiceAddr = "localhost:2222".parse().expect("valid service addr");
        let direct_addr: Multiaddr = "/ip4/81.219.135.162/udp/47207/quic-v1"
            .parse()
            .expect("valid direct address");

        let mut state = TunnelState::new();
        state.awaiting_holepunch.insert(peer, vec![(service, bind)]);
        let (state, _) = state.transition(Event::HolePunchAttemptStarted {
            peer,
            attempt_id: 9,
        });
        let (state, _) = state.transition(Event::PeerIdentified {
            peer,
            listen_addrs: vec![direct_addr.clone()],
            observed_addr: direct_addr.clone(),
        });

        let (_, commands) = state.transition(Event::TunnelDialFailed {
            peer,
            reason: "temporary network failure".to_string(),
        });

        assert!(commands.contains(&Command::DialPeerWithAddrs {
            peer,
            addrs: vec![direct_addr],
            attempt_id: 9,
        }));
    }

    #[test]
    fn retries_exhausted_fall_back_to_relay_tunnel() {
        let peer = PeerId::random();
        let service = ServiceName::new("svc");
        let bind: ServiceAddr = "localhost:2222".parse().expect("valid service addr");

        let mut state = TunnelState::new();
        state
            .awaiting_holepunch
            .insert(peer, vec![(service.clone(), bind.clone())]);

        let (state, _) = state.transition(Event::HolePunchAttemptStarted {
            peer,
            attempt_id: 1,
        });
        let (state, first_retry) = state.transition(Event::HolePunchAttemptTimeout {
            peer,
            attempt_id: 1,
        });
        assert!(first_retry.contains(&Command::DisconnectPeer { peer }));
        let (state, _) = state.transition(Event::ConnectionLost {
            peer,
            remaining_connections: 0,
        });

        let (state, _) = state.transition(Event::HolePunchAttemptStarted {
            peer,
            attempt_id: 2,
        });
        let (state, second_retry) = state.transition(Event::HolePunchAttemptTimeout {
            peer,
            attempt_id: 2,
        });
        assert!(second_retry.contains(&Command::DisconnectPeer { peer }));
        let (state, _) = state.transition(Event::ConnectionLost {
            peer,
            remaining_connections: 0,
        });

        let (state, _) = state.transition(Event::HolePunchAttemptStarted {
            peer,
            attempt_id: 3,
        });
        let (state, third_retry) = state.transition(Event::HolePunchAttemptTimeout {
            peer,
            attempt_id: 3,
        });
        assert!(third_retry.contains(&Command::DisconnectPeer { peer }));
        let (state, _) = state.transition(Event::ConnectionLost {
            peer,
            remaining_connections: 0,
        });

        let (state, _) = state.transition(Event::HolePunchAttemptStarted {
            peer,
            attempt_id: 4,
        });
        let (state, fourth_retry) = state.transition(Event::HolePunchAttemptTimeout {
            peer,
            attempt_id: 4,
        });

        assert!(fourth_retry.contains(&Command::SpawnTunnel {
            peer,
            service,
            bind,
        }));
        assert!(!fourth_retry.contains(&Command::DisconnectPeer { peer }));
        assert!(!state.awaiting_holepunch.contains_key(&peer));
        assert!(!state.pending_retry_redial.contains(&peer));
        assert!(!state.holepunch_retry_attempts.contains_key(&peer));
        assert!(!state.holepunch_sessions.contains_key(&peer));
    }

    #[test]
    fn retry_redial_uses_relay_snapshot_addresses() {
        let peer = PeerId::random();
        let service = ServiceName::new("svc");
        let bind: ServiceAddr = "localhost:2222".parse().expect("valid service addr");
        let relay_addr: Multiaddr = "/ip4/81.219.135.164/udp/4001/quic-v1/p2p/12D3KooWD8ZvJvEZ2aEXxonzcrbtgBw8ZK7NfuVLrHvX2dJQs6W3/p2p-circuit/p2p/12D3KooWJcJEadnZQzkChrKNkczWjB9grYasXYvQ4LMGmreqEPrw"
            .parse()
            .expect("valid relay multiaddr");

        let mut state = TunnelState::new();
        state.awaiting_holepunch.insert(peer, vec![(service, bind)]);
        let (state, _) = state.transition(Event::PeerIdentified {
            peer,
            listen_addrs: vec![relay_addr.clone()],
            observed_addr: relay_addr.clone(),
        });
        let (state, _) = state.transition(Event::HolePunchAttemptStarted {
            peer,
            attempt_id: 3,
        });
        let (state, _) = state.transition(Event::HolePunchAttemptTimeout {
            peer,
            attempt_id: 3,
        });
        let (_, commands) = state.transition(Event::ConnectionLost {
            peer,
            remaining_connections: 0,
        });

        assert!(commands.contains(&Command::DialPeerWithAddrs {
            peer,
            addrs: vec![relay_addr],
            attempt_id: 3,
        }));
    }

    #[test]
    fn retry_redial_prioritizes_direct_before_relay_candidates() {
        let peer = PeerId::random();
        let service = ServiceName::new("svc");
        let bind: ServiceAddr = "localhost:2222".parse().expect("valid service addr");
        let relay_addr: Multiaddr = format!(
            "/ip4/81.219.135.164/udp/4001/quic-v1/p2p/12D3KooWD8ZvJvEZ2aEXxonzcrbtgBw8ZK7NfuVLrHvX2dJQs6W3/p2p-circuit/p2p/{peer}"
        )
        .parse()
        .expect("valid relay multiaddr");
        let direct_addr: Multiaddr = format!("/ip4/81.219.135.162/udp/41613/quic-v1/p2p/{peer}")
            .parse()
            .expect("valid direct multiaddr");

        let mut state = TunnelState::new();
        state.awaiting_holepunch.insert(peer, vec![(service, bind)]);
        let (state, _) = state.transition(Event::PeerIdentified {
            peer,
            listen_addrs: vec![relay_addr.clone(), direct_addr.clone()],
            observed_addr: relay_addr.clone(),
        });
        let (state, _) = state.transition(Event::HolePunchAttemptStarted {
            peer,
            attempt_id: 1,
        });
        let (state, _) = state.transition(Event::HolePunchAttemptTimeout {
            peer,
            attempt_id: 1,
        });
        let (_, commands) = state.transition(Event::ConnectionLost {
            peer,
            remaining_connections: 0,
        });

        let addrs = commands
            .iter()
            .find_map(|command| match command {
                Command::DialPeerWithAddrs {
                    peer: dial_peer,
                    addrs,
                    attempt_id,
                } if *dial_peer == peer && *attempt_id == 1 => Some(addrs),
                _ => None,
            })
            .expect("expected redial with explicit addresses");
        assert_eq!(addrs.first(), Some(&direct_addr));
        assert!(addrs.contains(&relay_addr));
    }
}
