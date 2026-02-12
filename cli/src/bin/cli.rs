use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use cli::{
    service::{ExposedService, TunnelByNameSpec},
    tunnel::TunnelSpec,
};
use libp2p::Multiaddr;
use tracing_subscriber::EnvFilter;

/// Punchgate — P2P NAT-traversing mesh node.
///
/// Every node runs identical code. Roles (relay, provider, consumer)
/// emerge organically from network conditions.
#[derive(Parser, Debug)]
#[command(name = "punchgate", version, about)]
struct Cli {
    /// Path to the persistent identity file (Ed25519 keypair).
    #[arg(long, default_value = "identity.key", env = "PUNCHGATE_IDENTITY")]
    identity: PathBuf,

    /// Multiaddr(s) to listen on.
    #[arg(
        long,
        default_value = "/ip4/0.0.0.0/tcp/0",
        env = "PUNCHGATE_LISTEN",
        value_delimiter = ','
    )]
    listen: Vec<Multiaddr>,

    /// Bootstrap peer multiaddr(s) to dial on startup.
    #[arg(long, env = "PUNCHGATE_BOOTSTRAP", value_delimiter = ',')]
    bootstrap: Vec<Multiaddr>,

    /// Expose a local service: name=host:port
    #[arg(long, env = "PUNCHGATE_EXPOSE", value_delimiter = ',')]
    expose: Vec<ExposedService>,

    /// Tunnel to a remote service: peer_id:service_name@bind_addr
    #[arg(long, env = "PUNCHGATE_TUNNEL", value_delimiter = ',')]
    tunnel: Vec<TunnelSpec>,

    /// Tunnel to a service by name (discovers provider via DHT): service_name@bind_addr
    #[arg(long, env = "PUNCHGATE_TUNNEL_BY_NAME", value_delimiter = ',')]
    tunnel_by_name: Vec<TunnelByNameSpec>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    cli::node::run(cli::node::NodeConfig {
        identity_path: cli.identity,
        listen_addrs: cli.listen,
        bootstrap_addrs: cli.bootstrap,
        exposed: cli.expose,
        tunnel_specs: cli.tunnel,
        tunnel_by_name_specs: cli.tunnel_by_name,
    })
    .await
}
