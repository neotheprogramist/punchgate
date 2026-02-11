use std::path::PathBuf;

use anyhow::{Result, bail};
use clap::Parser;
use cli::state::NatStatus;
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
    listen: Vec<String>,

    /// Bootstrap peer multiaddr(s) to dial on startup.
    #[arg(long, env = "PUNCHGATE_BOOTSTRAP", value_delimiter = ',')]
    bootstrap: Vec<String>,

    /// Expose a local service: name=host:port
    #[arg(long, env = "PUNCHGATE_EXPOSE", value_delimiter = ',')]
    expose: Vec<String>,

    /// Tunnel to a remote service: peer_id:service_name@bind_addr
    #[arg(long, env = "PUNCHGATE_TUNNEL", value_delimiter = ',')]
    tunnel: Vec<String>,

    /// Tunnel to a service by name (discovers provider via DHT): service_name@bind_addr
    #[arg(long, env = "PUNCHGATE_TUNNEL_BY_NAME", value_delimiter = ',')]
    tunnel_by_name: Vec<String>,

    /// Override NAT status detection (private or public).
    /// When set, bypasses AutoNAT wait and immediately enters participation.
    #[arg(long, env = "PUNCHGATE_NAT_STATUS")]
    nat_status: Option<String>,

    /// Declare external address(es) for this node.
    /// Required for relay servers so clients learn how to reach the relay.
    #[arg(long, env = "PUNCHGATE_EXTERNAL_ADDRESS", value_delimiter = ',')]
    external_address: Vec<String>,
}

fn parse_nat_status(s: &str) -> Result<NatStatus> {
    match s {
        "private" => Ok(NatStatus::Private),
        "public" => Ok(NatStatus::Public),
        other => bail!("invalid --nat-status value '{other}': must be 'private' or 'public'"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    let nat_status = cli
        .nat_status
        .as_deref()
        .map(parse_nat_status)
        .transpose()?;

    cli::node::run(cli::node::NodeConfig {
        identity_path: cli.identity,
        listen_addrs: cli.listen,
        bootstrap_addrs: cli.bootstrap,
        expose_specs: cli.expose,
        tunnel_specs: cli.tunnel,
        tunnel_by_name_specs: cli.tunnel_by_name,
        nat_status_override: nat_status,
        external_addrs: cli.external_address,
    })
    .await
}
