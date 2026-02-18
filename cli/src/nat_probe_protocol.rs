use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NatProbeRequest {
    pub mapped_addr: SocketAddr,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NatProbeResponse {
    pub probe_sent: bool,
}
