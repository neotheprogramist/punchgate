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
    #[arg(long, default_value = "/ip4/0.0.0.0/tcp/0")]
    listen: Vec<String>,

    /// Bootstrap peer multiaddr(s) to dial on startup.
    #[arg(long)]
    bootstrap: Vec<String>,

    /// Expose a local service: name=host:port
    #[arg(long)]
    expose: Vec<String>,

    /// Tunnel to a remote service: peer_id:service_name@bind_addr
    #[arg(long)]
    tunnel: Vec<String>,

    /// Override NAT status detection (private or public).
    /// When set, bypasses AutoNAT wait and immediately enters participation.
    #[arg(long)]
    nat_status: Option<String>,

    /// Declare external address(es) for this node.
    /// Required for relay servers so clients learn how to reach the relay.
    #[arg(long)]
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

    cli::node::run(
        cli.identity,
        cli.listen,
        cli.bootstrap,
        cli.expose,
        cli.tunnel,
        nat_status,
        cli.external_address,
    )
    .await
}
