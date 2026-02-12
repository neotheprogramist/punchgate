use super::{
    command::Command,
    event::Event,
    peer::{PeerState, Phase},
    tunnel::TunnelState,
};
use crate::traits::MealyMachine;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppState {
    pub peer: PeerState,
    pub tunnel: TunnelState,
}

impl AppState {
    pub fn new(peer: PeerState, tunnel: TunnelState) -> Self {
        Self { peer, tunnel }
    }

    pub fn phase(&self) -> Phase {
        self.peer.phase
    }
}

impl MealyMachine for AppState {
    type Event = Event;
    type Command = Command;

    fn transition(self, event: Event) -> (Self, Vec<Command>) {
        let old_phase = self.peer.phase;
        let (peer, peer_cmds) = self.peer.transition(event.clone());

        let bridge = match old_phase == peer.phase {
            true => None,
            false => Some(Event::PhaseChanged {
                old: old_phase,
                new: peer.phase,
            }),
        };

        let (tunnel, tunnel_cmds) = bridge.into_iter().chain(std::iter::once(event)).fold(
            (self.tunnel, Vec::new()),
            |(state, mut cmds), evt| {
                let (s, c) = state.transition(evt);
                cmds.extend(c);
                (s, cmds)
            },
        );

        let cmds = peer_cmds.into_iter().chain(tunnel_cmds).collect();
        (Self { peer, tunnel }, cmds)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;
    use crate::{
        state::peer::NatStatus,
        test_utils::{arb_multiaddr, arb_peer_id},
    };

    fn new_app() -> AppState {
        AppState::new(PeerState::new(), TunnelState::new())
    }

    proptest! {
        #[test]
        fn composition_publishes_on_participating_entry(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let app = new_app();
            let (app, _) = app.transition(Event::BootstrapConnected { peer, addr });
            prop_assert_eq!(app.phase(), Phase::Discovering);

            let (app, commands) = app.transition(Event::KademliaBootstrapOk);
            prop_assert_eq!(app.phase(), Phase::Participating);
            prop_assert!(commands.contains(&Command::PublishServices));
        }

        #[test]
        fn composition_publishes_on_relay_accepted(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let app = new_app();
            let (app, _) = app.transition(Event::BootstrapConnected { peer, addr });
            let (app, _) = app.transition(Event::KademliaBootstrapOk);

            let relay_peer = app.peer.relay.clone();
            match relay_peer {
                crate::state::RelayState::Requesting { relay_peer } => {
                    let (_, commands) = app.transition(Event::RelayReservationAccepted { relay_peer });
                    prop_assert!(commands.contains(&Command::PublishServices));
                }
                _ => {
                    // No relay was requested (no non-loopback addresses)
                }
            }
        }

        #[test]
        fn composition_preserves_peer_commands(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let app = new_app();
            let (_, commands) = app.transition(Event::BootstrapConnected {
                peer, addr: addr.clone(),
            });
            prop_assert!(commands.contains(&Command::KademliaBootstrap));
            let has_kad_add = commands.contains(&Command::KademliaAddAddress {
                peer, addr,
            });
            prop_assert!(has_kad_add);
        }

        #[test]
        fn composition_shutdown(phase in arb_peer_id()) {
            let _ = phase; // unused, just for proptest variety
            let app = new_app();
            let (app, commands) = app.transition(Event::ShutdownRequested);
            prop_assert_eq!(app.phase(), Phase::ShuttingDown);
            prop_assert!(commands.contains(&Command::Shutdown));
        }

        #[test]
        fn no_bootstrap_publishes_directly(
            nat_status in prop_oneof![
                Just(NatStatus::Unknown),
                Just(NatStatus::Public),
                Just(NatStatus::Private),
            ],
        ) {
            let _ = nat_status;
            let app = new_app();
            let (app, commands) = app.transition(Event::NoBootstrapPeers);
            prop_assert_eq!(app.phase(), Phase::Participating);
            prop_assert!(commands.contains(&Command::PublishServices));
        }

        #[test]
        fn peer_cmds_before_tunnel_cmds(
            peer in arb_peer_id(),
            addr in arb_multiaddr(),
        ) {
            let app = new_app();
            let (app, _) = app.transition(Event::BootstrapConnected { peer, addr });
            let (_, commands) = app.transition(Event::KademliaBootstrapOk);

            // Peer commands (KademliaBootstrap, RequestRelayReservation) come
            // before tunnel commands (PublishServices) in the output
            let publish_idx = commands.iter().position(|c| matches!(c, Command::PublishServices));
            let relay_idx = commands.iter().position(|c| matches!(c, Command::RequestRelayReservation { .. }));

            if let (Some(relay), Some(publish)) = (relay_idx, publish_idx) {
                prop_assert!(relay < publish, "peer commands must precede tunnel commands");
            }
        }
    }
}
