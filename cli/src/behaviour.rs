use anyhow::Result;
#[cfg(feature = "autonat")]
use libp2p::autonat;
#[cfg(feature = "mdns")]
use libp2p::mdns;
use libp2p::{
    PeerId, dcutr, identify, identity::Keypair, kad, ping, relay, swarm::NetworkBehaviour,
};

use crate::protocol;

#[derive(NetworkBehaviour)]
pub struct Behaviour {
    pub kademlia: kad::Behaviour<kad::store::MemoryStore>,
    #[cfg(feature = "mdns")]
    pub mdns: mdns::tokio::Behaviour,
    pub relay_server: relay::Behaviour,
    pub relay_client: relay::client::Behaviour,
    pub dcutr: dcutr::Behaviour,
    pub identify: identify::Behaviour,
    #[cfg(feature = "autonat")]
    pub autonat: autonat::Behaviour,
    pub ping: ping::Behaviour,
    pub stream: libp2p_stream::Behaviour,
}

impl Behaviour {
    pub fn new(keypair: &Keypair, relay_client: relay::client::Behaviour) -> Result<Self> {
        let peer_id = PeerId::from(keypair.public());

        let kad_config = kad::Config::new(protocol::kad_protocol());
        let store = kad::store::MemoryStore::new(peer_id);
        let kademlia = kad::Behaviour::with_config(peer_id, store, kad_config);

        #[cfg(feature = "mdns")]
        let mdns = mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id)?;

        let relay_server = relay::Behaviour::new(peer_id, Default::default());

        let identify = identify::Behaviour::new(identify::Config::new(
            protocol::IDENTIFY_PROTOCOL.to_string(),
            keypair.public(),
        ));

        #[cfg(feature = "autonat")]
        let autonat = autonat::Behaviour::new(peer_id, autonat::Config::default());

        Ok(Self {
            kademlia,
            #[cfg(feature = "mdns")]
            mdns,
            relay_server,
            relay_client,
            dcutr: dcutr::Behaviour::new(peer_id),
            identify,
            #[cfg(feature = "autonat")]
            autonat,
            ping: ping::Behaviour::default(),
            stream: libp2p_stream::Behaviour::new(),
        })
    }
}
