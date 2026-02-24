use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use libp2p::{PeerId, swarm::ConnectionId};

#[derive(Debug, Clone)]
pub(super) struct HolePunchAttemptStats {
    pub(super) attempt_id: u64,
    started_at: Instant,
    pub(super) dials_started: u32,
    pub(super) dials_failed: u32,
    pub(super) last_dial_error: Option<String>,
}

impl HolePunchAttemptStats {
    fn new(attempt_id: u64) -> Self {
        Self {
            attempt_id,
            started_at: Instant::now(),
            dials_started: 0,
            dials_failed: 0,
            last_dial_error: None,
        }
    }

    pub(super) fn elapsed_ms(&self) -> u128 {
        self.started_at.elapsed().as_millis()
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct HolePunchDeadline {
    pub(super) attempt_id: u64,
    pub(super) deadline: tokio::time::Instant,
}

#[derive(Debug, Clone)]
pub(super) struct ConnectionEstablishedUpdate {
    pub(super) started_attempt_id: Option<u64>,
    pub(super) completed_attempt: Option<HolePunchAttemptStats>,
    pub(super) relayed_connections_to_close: Vec<ConnectionId>,
    pub(super) direct_connection_established: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ConnectionClosedUpdate {
    pub(super) clear_direct_registry: bool,
    pub(super) peer_fully_disconnected: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum DialingUpdate {
    Tracked { attempt_id: u64, attempt: u32 },
    RelayOnlyWithoutActiveAttempt,
    Ignored,
}

#[derive(Debug, Clone)]
pub(super) enum OutgoingErrorUpdate {
    Tracked {
        peer: PeerId,
        attempt_id: u64,
        attempt: u32,
        failures: u32,
    },
    StaleTracked {
        peer: PeerId,
        expected_attempt: u64,
        received_attempt: u64,
    },
    RelayOnlyUntracked {
        peer: PeerId,
    },
    Ignored,
}

#[derive(Debug, Default)]
pub(super) struct ConnectionStateMachine {
    relayed_connections: HashMap<PeerId, HashSet<ConnectionId>>,
    direct_connections: HashMap<PeerId, HashSet<ConnectionId>>,
    hole_punch_deadlines: HashMap<PeerId, HolePunchDeadline>,
    hole_punch_attempts: HashMap<PeerId, HolePunchAttemptStats>,
    hole_punch_attempt_seq: HashMap<PeerId, u64>,
    pending_hole_punch_dials: HashMap<ConnectionId, (PeerId, u64)>,
}

impl ConnectionStateMachine {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn on_connection_established(
        &mut self,
        peer: PeerId,
        connection_id: ConnectionId,
        relayed: bool,
        hole_punch_timeout: Duration,
    ) -> ConnectionEstablishedUpdate {
        self.pending_hole_punch_dials.remove(&connection_id);

        match relayed {
            true => {
                let relayed_ids = self.relayed_connections.entry(peer).or_default();
                let first_relayed_connection = relayed_ids.is_empty();
                relayed_ids.insert(connection_id);

                let started_attempt_id =
                    match first_relayed_connection && !self.has_direct_connection(&peer) {
                        true => {
                            let attempt_id = self.next_hole_punch_attempt_id(peer);
                            self.hole_punch_attempts
                                .insert(peer, HolePunchAttemptStats::new(attempt_id));
                            self.hole_punch_deadlines.insert(
                                peer,
                                HolePunchDeadline {
                                    attempt_id,
                                    deadline: tokio::time::Instant::now() + hole_punch_timeout,
                                },
                            );
                            Some(attempt_id)
                        }
                        false => None,
                    };

                ConnectionEstablishedUpdate {
                    started_attempt_id,
                    completed_attempt: None,
                    relayed_connections_to_close: Vec::new(),
                    direct_connection_established: false,
                }
            }
            false => {
                self.direct_connections
                    .entry(peer)
                    .or_default()
                    .insert(connection_id);

                let completed_attempt = self.complete_hole_punch_attempt(&peer);
                let relayed_connections_to_close = self
                    .relayed_connections
                    .remove(&peer)
                    .map(|ids| ids.into_iter().collect())
                    .unwrap_or_default();

                ConnectionEstablishedUpdate {
                    started_attempt_id: None,
                    completed_attempt,
                    relayed_connections_to_close,
                    direct_connection_established: true,
                }
            }
        }
    }

    pub(super) fn on_connection_closed(
        &mut self,
        peer: PeerId,
        connection_id: ConnectionId,
        num_established: u32,
    ) -> ConnectionClosedUpdate {
        self.pending_hole_punch_dials.remove(&connection_id);
        let _ = Self::remove_connection_id(&mut self.relayed_connections, &peer, &connection_id);
        let direct_exhausted =
            Self::remove_connection_id(&mut self.direct_connections, &peer, &connection_id);
        let tracked_connection_alive =
            self.has_relayed_connection(&peer) || self.has_direct_connection(&peer);
        let peer_fully_disconnected = num_established == 0 && !tracked_connection_alive;

        if peer_fully_disconnected {
            self.complete_hole_punch_attempt(&peer);
            self.relayed_connections.remove(&peer);
            self.direct_connections.remove(&peer);
        }

        ConnectionClosedUpdate {
            clear_direct_registry: direct_exhausted || peer_fully_disconnected,
            peer_fully_disconnected,
        }
    }

    pub(super) fn on_dialing(
        &mut self,
        peer: PeerId,
        connection_id: ConnectionId,
    ) -> DialingUpdate {
        if !self.peer_is_relay_only(&peer) {
            return DialingUpdate::Ignored;
        }

        match self.hole_punch_attempts.get_mut(&peer) {
            Some(stats) => {
                stats.dials_started = stats.dials_started.saturating_add(1);
                let attempt = stats.dials_started;
                self.pending_hole_punch_dials
                    .insert(connection_id, (peer, stats.attempt_id));
                DialingUpdate::Tracked {
                    attempt_id: stats.attempt_id,
                    attempt,
                }
            }
            None => DialingUpdate::RelayOnlyWithoutActiveAttempt,
        }
    }

    pub(super) fn on_outgoing_connection_error(
        &mut self,
        peer_id: Option<PeerId>,
        connection_id: ConnectionId,
        error_text: String,
    ) -> OutgoingErrorUpdate {
        let tracked_attempt = self.pending_hole_punch_dials.get(&connection_id).copied();
        let update = match tracked_attempt {
            Some((peer, attempt_id)) if self.peer_is_relay_only(&peer) => {
                match self.hole_punch_attempts.get_mut(&peer) {
                    Some(stats) if stats.attempt_id == attempt_id => {
                        stats.dials_failed = stats.dials_failed.saturating_add(1);
                        stats.last_dial_error = Some(error_text);
                        OutgoingErrorUpdate::Tracked {
                            peer,
                            attempt_id,
                            attempt: stats.dials_started,
                            failures: stats.dials_failed,
                        }
                    }
                    Some(stats) => OutgoingErrorUpdate::StaleTracked {
                        peer,
                        expected_attempt: stats.attempt_id,
                        received_attempt: attempt_id,
                    },
                    None => OutgoingErrorUpdate::RelayOnlyUntracked { peer },
                }
            }
            Some((..)) => OutgoingErrorUpdate::Ignored,
            None => match peer_id.filter(|peer| self.peer_is_relay_only(peer)) {
                Some(peer) => OutgoingErrorUpdate::RelayOnlyUntracked { peer },
                None => OutgoingErrorUpdate::Ignored,
            },
        };

        self.pending_hole_punch_dials.remove(&connection_id);
        update
    }

    pub(super) fn complete_hole_punch_attempt(
        &mut self,
        peer: &PeerId,
    ) -> Option<HolePunchAttemptStats> {
        let completed = self.hole_punch_attempts.remove(peer);
        self.hole_punch_deadlines.remove(peer);
        self.remove_pending_hole_punch_dials_for_peer(peer);
        completed
    }

    pub(super) fn has_relayed_connection(&self, peer: &PeerId) -> bool {
        self.relayed_connections.contains_key(peer)
    }

    pub(super) fn peer_is_relay_only(&self, peer: &PeerId) -> bool {
        self.has_relayed_connection(peer) && !self.has_direct_connection(peer)
    }

    pub(super) fn direct_connection_count(&self, peer: &PeerId) -> usize {
        self.connection_count_for_peer(&self.direct_connections, peer)
    }

    pub(super) fn relayed_connection_count(&self, peer: &PeerId) -> usize {
        self.connection_count_for_peer(&self.relayed_connections, peer)
    }

    pub(super) fn active_attempt_stats(&self, peer: &PeerId) -> Option<&HolePunchAttemptStats> {
        self.hole_punch_attempts.get(peer)
    }

    pub(super) fn next_hole_punch_deadline(&self) -> Option<tokio::time::Instant> {
        self.hole_punch_deadlines
            .values()
            .map(|entry| entry.deadline)
            .min()
    }

    pub(super) fn take_due_hole_punch_timeouts(
        &mut self,
        now: tokio::time::Instant,
    ) -> Vec<(PeerId, u64)> {
        let due: Vec<(PeerId, u64)> = self
            .hole_punch_deadlines
            .iter()
            .filter(|(_, entry)| entry.deadline <= now)
            .map(|(peer, entry)| (*peer, entry.attempt_id))
            .collect();
        for (peer, _) in &due {
            self.hole_punch_deadlines.remove(peer);
        }
        due
    }

    fn has_direct_connection(&self, peer: &PeerId) -> bool {
        self.direct_connections.contains_key(peer)
    }

    fn connection_count_for_peer(
        &self,
        by_peer: &HashMap<PeerId, HashSet<ConnectionId>>,
        peer: &PeerId,
    ) -> usize {
        by_peer.get(peer).map_or(0, HashSet::len)
    }

    fn remove_connection_id(
        by_peer: &mut HashMap<PeerId, HashSet<ConnectionId>>,
        peer: &PeerId,
        connection_id: &ConnectionId,
    ) -> bool {
        match by_peer.get_mut(peer) {
            Some(ids) => {
                ids.remove(connection_id);
                match ids.is_empty() {
                    true => {
                        by_peer.remove(peer);
                        true
                    }
                    false => false,
                }
            }
            None => false,
        }
    }

    fn next_hole_punch_attempt_id(&mut self, peer: PeerId) -> u64 {
        let entry = self.hole_punch_attempt_seq.entry(peer).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    }

    fn remove_pending_hole_punch_dials_for_peer(&mut self, peer: &PeerId) {
        self.pending_hole_punch_dials
            .retain(|_, (tracked_peer, _)| tracked_peer != peer);
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn cid(id: usize) -> ConnectionId {
        ConnectionId::new_unchecked(id)
    }

    #[test]
    fn stale_zero_count_disconnect_does_not_drop_new_relay_connection() {
        let peer = PeerId::random();
        let mut machine = ConnectionStateMachine::new();
        let _ = machine.on_connection_established(peer, cid(1), true, Duration::from_secs(15));
        let _ = machine.on_connection_established(peer, cid(2), true, Duration::from_secs(15));

        let update = machine.on_connection_closed(peer, cid(1), 0);

        assert!(!update.peer_fully_disconnected);
        assert_eq!(machine.relayed_connection_count(&peer), 1);
        assert!(machine.active_attempt_stats(&peer).is_some());
    }

    proptest! {
        #[test]
        fn rapid_relay_disconnect_reconnect_keeps_state_consistent(cycles in 1usize..64usize) {
            let peer = PeerId::random();
            let mut machine = ConnectionStateMachine::new();
            let timeout = Duration::from_secs(15);

            for i in 0..cycles {
                let established = machine.on_connection_established(peer, cid(i * 2 + 1), true, timeout);
                prop_assert!(established.started_attempt_id.is_some());
                let closed = machine.on_connection_closed(peer, cid(i * 2 + 1), 0);
                prop_assert!(closed.peer_fully_disconnected);
                prop_assert_eq!(machine.relayed_connection_count(&peer), 0);
                prop_assert_eq!(machine.direct_connection_count(&peer), 0);
                prop_assert!(machine.active_attempt_stats(&peer).is_none());
                prop_assert!(machine.next_hole_punch_deadline().is_none());
            }
        }
    }
}
