use std::time::Duration;

use libp2p::StreamProtocol;

pub const KAD_PROTOCOL: &str = "/punchgate/kad/1.0.0";
pub const TUNNEL_PROTOCOL: &str = "/punchgate/tunnel/1.0.0";
pub const IDENTIFY_PROTOCOL: &str = "/punchgate/id/1.0.0";
pub const SERVICE_KEY_PREFIX: &str = "/punchgate/svc/";

pub const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(30);
pub const IDLE_CONNECTION_TIMEOUT: Duration = Duration::from_secs(3600);
pub const SERVICE_REPUBLISH_INTERVAL: Duration = Duration::from_secs(300);
pub const HOLEPUNCH_TIMEOUT: Duration = Duration::from_secs(15);
pub const MAX_CIRCUIT_DURATION: Duration = Duration::from_secs(3600);

pub fn kad_protocol() -> StreamProtocol {
    // Infallible: KAD_PROTOCOL is a compile-time constant starting with '/' as required by StreamProtocol
    StreamProtocol::try_from_owned(KAD_PROTOCOL.to_string())
        .expect("KAD_PROTOCOL is a valid compile-time constant protocol string")
}

pub fn tunnel_protocol() -> StreamProtocol {
    // Infallible: TUNNEL_PROTOCOL is a compile-time constant starting with '/' as required by StreamProtocol
    StreamProtocol::try_from_owned(TUNNEL_PROTOCOL.to_string())
        .expect("TUNNEL_PROTOCOL is a valid compile-time constant protocol string")
}
