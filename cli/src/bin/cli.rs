use std::path::PathBuf;

use clap::Parser;
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    cli::node::run(
        cli.identity,
        cli.listen,
        cli.bootstrap,
        cli.expose,
        cli.tunnel,
    )
    .await
}
