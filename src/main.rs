use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use libp2p::Multiaddr;
use punchgate::specs::{ExposedService, TunnelByNameSpec, TunnelSpec};
use tracing_subscriber::EnvFilter;

const SERVICE_LABEL: &str = "com.punchgate.daemon";
const SYSTEMD_SERVICE_NAME: &str = "punchgate.service";

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
        default_value = "/ip4/0.0.0.0/udp/0/quic-v1",
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

    /// Service manager actions for running punchgate in the background.
    #[command(subcommand)]
    command: Option<ServiceCommand>,
}

#[derive(Subcommand, Debug)]
enum ServiceCommand {
    /// Install/update and start background service.
    Up,
    /// Stop and uninstall/disable background service.
    Down,
    /// Show background service status.
    Status,
    /// Show service logs.
    Logs {
        /// Follow logs continuously.
        #[arg(short, long)]
        follow: bool,
        /// Number of recent lines to show.
        #[arg(long, default_value_t = 200)]
        lines: usize,
    },
}

#[derive(Debug, Clone)]
struct RuntimeConfig {
    identity_path: PathBuf,
    listen_addrs: Vec<Multiaddr>,
    bootstrap_addrs: Vec<Multiaddr>,
    exposed: Vec<ExposedService>,
    tunnel_specs: Vec<TunnelSpec>,
    tunnel_by_name_specs: Vec<TunnelByNameSpec>,
}

impl From<&Cli> for RuntimeConfig {
    fn from(cli: &Cli) -> Self {
        Self {
            identity_path: cli.identity.clone(),
            listen_addrs: cli.listen.clone(),
            bootstrap_addrs: cli.bootstrap.clone(),
            exposed: cli.expose.clone(),
            tunnel_specs: cli.tunnel.clone(),
            tunnel_by_name_specs: cli.tunnel_by_name.clone(),
        }
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

    if let Some(command) = &cli.command {
        run_service_command(&cli, command)?;
        return Ok(());
    }

    let cfg = RuntimeConfig::from(&cli);
    punchgate::node::run(punchgate::node::NodeConfig {
        identity_path: cfg.identity_path,
        listen_addrs: cfg.listen_addrs,
        bootstrap_addrs: cfg.bootstrap_addrs,
        exposed: cfg.exposed,
        tunnel_specs: cfg.tunnel_specs,
        tunnel_by_name_specs: cfg.tunnel_by_name_specs,
    })
    .await
}

fn run_service_command(cli: &Cli, command: &ServiceCommand) -> Result<()> {
    match detect_os()? {
        OsKind::Macos => run_macos_service_command(cli, command),
        OsKind::Linux => run_linux_service_command(cli, command),
    }
}

#[derive(Debug, Clone, Copy)]
enum OsKind {
    Macos,
    Linux,
}

fn detect_os() -> Result<OsKind> {
    if cfg!(target_os = "macos") {
        Ok(OsKind::Macos)
    } else if cfg!(target_os = "linux") {
        Ok(OsKind::Linux)
    } else {
        bail!("service management is supported only on macOS and Linux");
    }
}

fn punchgate_home_dir() -> Result<PathBuf> {
    let home = env::var_os("HOME").ok_or_else(|| anyhow!("$HOME is not set"))?;
    Ok(PathBuf::from(home))
}

fn app_config_dir() -> Result<PathBuf> {
    Ok(punchgate_home_dir()?.join(".config").join("punchgate"))
}

fn app_env_file() -> Result<PathBuf> {
    Ok(app_config_dir()?.join("punchgate.env"))
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(env::current_dir()
            .context("failed to resolve current directory")?
            .join(path))
    }
}

fn render_env_file(cli: &Cli) -> Result<String> {
    let mut lines = Vec::new();
    lines.push("# Auto-generated by `punchgate up`".to_string());
    lines.push("# Edit and re-run `punchgate up` to apply changes.".to_string());

    let identity = absolute_path(&cli.identity)?;
    lines.push(format!("PUNCHGATE_IDENTITY={}", identity.display()));
    lines.push(format!("PUNCHGATE_LISTEN={}", join_multiaddrs(&cli.listen)));

    if !cli.bootstrap.is_empty() {
        lines.push(format!(
            "PUNCHGATE_BOOTSTRAP={}",
            join_multiaddrs(&cli.bootstrap)
        ));
    }
    if !cli.expose.is_empty() {
        lines.push(format!(
            "PUNCHGATE_EXPOSE={}",
            format_exposed_services(&cli.expose)
        ));
    }
    if !cli.tunnel.is_empty() {
        lines.push(format!(
            "PUNCHGATE_TUNNEL={}",
            format_tunnel_specs(&cli.tunnel)
        ));
    }
    if !cli.tunnel_by_name.is_empty() {
        lines.push(format!(
            "PUNCHGATE_TUNNEL_BY_NAME={}",
            format_tunnel_by_name_specs(&cli.tunnel_by_name)
        ));
    }

    let rust_log = env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    lines.push(format!("RUST_LOG={rust_log}"));

    lines.push(String::new());
    Ok(lines.join("\n"))
}

fn join_multiaddrs(addrs: &[Multiaddr]) -> String {
    addrs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn format_exposed_services(items: &[ExposedService]) -> String {
    items
        .iter()
        .map(|spec| format!("{}={}", spec.name, spec.local_addr))
        .collect::<Vec<_>>()
        .join(",")
}

fn format_tunnel_specs(items: &[TunnelSpec]) -> String {
    items
        .iter()
        .map(|spec| {
            format!(
                "{}:{}@{}",
                spec.remote_peer, spec.service_name, spec.bind_addr
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn format_tunnel_by_name_specs(items: &[TunnelByNameSpec]) -> String {
    items
        .iter()
        .map(|spec| format!("{}@{}", spec.service_name, spec.bind_addr))
        .collect::<Vec<_>>()
        .join(",")
}

fn run_checked(cmd: &mut Command) -> Result<()> {
    let printable = format!("{:?}", cmd);
    let status = cmd
        .status()
        .with_context(|| format!("failed to execute {printable}"))?;
    if !status.success() {
        bail!("command failed ({status}): {printable}");
    }
    Ok(())
}

fn run_allow_failure(cmd: &mut Command) -> Result<()> {
    let printable = format!("{:?}", cmd);
    let _ = cmd
        .status()
        .with_context(|| format!("failed to execute {printable}"))?;
    Ok(())
}

fn run_linux_service_command(cli: &Cli, command: &ServiceCommand) -> Result<()> {
    let home = punchgate_home_dir()?;
    let systemd_user_dir = home.join(".config").join("systemd").join("user");
    let service_path = systemd_user_dir.join(SYSTEMD_SERVICE_NAME);

    match command {
        ServiceCommand::Up => {
            fs::create_dir_all(&systemd_user_dir)
                .with_context(|| format!("failed to create {}", systemd_user_dir.display()))?;
            fs::create_dir_all(app_config_dir()?)
                .with_context(|| "failed to create app config directory".to_string())?;

            let env_path = app_env_file()?;
            fs::write(&env_path, render_env_file(cli)?)
                .with_context(|| format!("failed to write {}", env_path.display()))?;

            let exe = env::current_exe().context("failed to resolve current executable")?;
            fs::write(&service_path, render_systemd_service(&exe))
                .with_context(|| format!("failed to write {}", service_path.display()))?;

            run_checked(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
            run_checked(Command::new("systemctl").args([
                "--user",
                "enable",
                "--now",
                SYSTEMD_SERVICE_NAME,
            ]))?;

            tracing::info!(path = %service_path.display(), "installed service unit");
            tracing::info!(path = %env_path.display(), "wrote runtime env file");
            tracing::info!(service = SYSTEMD_SERVICE_NAME, "service started");
        }
        ServiceCommand::Down => {
            run_allow_failure(Command::new("systemctl").args([
                "--user",
                "disable",
                "--now",
                SYSTEMD_SERVICE_NAME,
            ]))?;
            run_allow_failure(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
            tracing::info!(
                service = SYSTEMD_SERVICE_NAME,
                "service stopped and disabled"
            );
        }
        ServiceCommand::Status => {
            run_checked(Command::new("systemctl").args([
                "--user",
                "--no-pager",
                "status",
                SYSTEMD_SERVICE_NAME,
            ]))?;
        }
        ServiceCommand::Logs { follow, lines } => {
            let mut cmd = Command::new("journalctl");
            cmd.args([
                "--user",
                "-u",
                SYSTEMD_SERVICE_NAME,
                "--no-pager",
                "-n",
                &lines.to_string(),
            ]);
            if *follow {
                cmd.arg("-f");
            }
            run_checked(&mut cmd)?;
        }
    }

    Ok(())
}

fn render_systemd_service(exe_path: &Path) -> String {
    format!(
        "[Unit]\nDescription=Punchgate background daemon\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nEnvironmentFile=%h/.config/punchgate/punchgate.env\nExecStart={}\nRestart=always\nRestartSec=2\nKillSignal=SIGINT\nTimeoutStopSec=15\nStandardOutput=journal\nStandardError=journal\n\n[Install]\nWantedBy=default.target\n",
        exe_path.display()
    )
}

fn run_macos_service_command(cli: &Cli, command: &ServiceCommand) -> Result<()> {
    let home = punchgate_home_dir()?;
    let launch_agents_dir = home.join("Library").join("LaunchAgents");
    let logs_dir = home.join("Library").join("Logs");
    let plist_path = launch_agents_dir.join(format!("{SERVICE_LABEL}.plist"));
    let domain = format!("gui/{}", current_uid()?);
    let label_handle = format!("{domain}/{SERVICE_LABEL}");
    let log_out = logs_dir.join("punchgate.log");
    let log_err = logs_dir.join("punchgate.err.log");

    match command {
        ServiceCommand::Up => {
            fs::create_dir_all(&launch_agents_dir)
                .with_context(|| format!("failed to create {}", launch_agents_dir.display()))?;
            fs::create_dir_all(&logs_dir)
                .with_context(|| format!("failed to create {}", logs_dir.display()))?;
            fs::create_dir_all(app_config_dir()?)
                .with_context(|| "failed to create app config directory".to_string())?;

            let env_path = app_env_file()?;
            fs::write(&env_path, render_env_file(cli)?)
                .with_context(|| format!("failed to write {}", env_path.display()))?;

            let exe = env::current_exe().context("failed to resolve current executable")?;
            let plist = render_launchd_plist(
                &exe,
                &log_out,
                &log_err,
                &service_env_map(cli, env::var("RUST_LOG").ok()),
            )?;
            fs::write(&plist_path, plist)
                .with_context(|| format!("failed to write {}", plist_path.display()))?;

            run_allow_failure(Command::new("launchctl").args([
                "bootout",
                &domain,
                &plist_path_string(&plist_path),
            ]))?;
            run_checked(Command::new("launchctl").args([
                "bootstrap",
                &domain,
                &plist_path_string(&plist_path),
            ]))?;
            run_checked(Command::new("launchctl").args(["enable", &label_handle]))?;
            run_checked(Command::new("launchctl").args(["kickstart", "-k", &label_handle]))?;

            tracing::info!(path = %plist_path.display(), "installed launch agent");
            tracing::info!(path = %env_path.display(), "wrote runtime env file");
            tracing::info!(service = SERVICE_LABEL, "service started");
        }
        ServiceCommand::Down => {
            run_allow_failure(Command::new("launchctl").args([
                "bootout",
                &domain,
                &plist_path_string(&plist_path),
            ]))?;
            run_allow_failure(Command::new("launchctl").args(["disable", &label_handle]))?;
            tracing::info!(service = SERVICE_LABEL, "service stopped");
        }
        ServiceCommand::Status => {
            run_checked(Command::new("launchctl").args(["print", &label_handle]))?;
        }
        ServiceCommand::Logs { follow, lines } => {
            let mut cmd = Command::new("tail");
            cmd.arg("-n").arg(lines.to_string());
            if *follow {
                cmd.arg("-f");
            }
            cmd.arg(&log_out);
            run_checked(&mut cmd)?;
        }
    }

    Ok(())
}

fn plist_path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn current_uid() -> Result<u32> {
    let output = Command::new("id")
        .arg("-u")
        .output()
        .context("failed to run `id -u`")?;
    if !output.status.success() {
        bail!("`id -u` failed with status {}", output.status);
    }
    let uid = String::from_utf8(output.stdout)
        .context("`id -u` returned non-UTF8 output")?
        .trim()
        .parse::<u32>()
        .context("failed to parse uid from `id -u`")?;
    Ok(uid)
}

fn service_env_map(cli: &Cli, rust_log: Option<String>) -> BTreeMap<String, String> {
    let mut envs = BTreeMap::new();
    envs.insert(
        "PUNCHGATE_IDENTITY".to_string(),
        absolute_path(&cli.identity)
            .unwrap_or_else(|_| cli.identity.clone())
            .display()
            .to_string(),
    );
    envs.insert("PUNCHGATE_LISTEN".to_string(), join_multiaddrs(&cli.listen));
    if !cli.bootstrap.is_empty() {
        envs.insert(
            "PUNCHGATE_BOOTSTRAP".to_string(),
            join_multiaddrs(&cli.bootstrap),
        );
    }
    if !cli.expose.is_empty() {
        envs.insert(
            "PUNCHGATE_EXPOSE".to_string(),
            format_exposed_services(&cli.expose),
        );
    }
    if !cli.tunnel.is_empty() {
        envs.insert(
            "PUNCHGATE_TUNNEL".to_string(),
            format_tunnel_specs(&cli.tunnel),
        );
    }
    if !cli.tunnel_by_name.is_empty() {
        envs.insert(
            "PUNCHGATE_TUNNEL_BY_NAME".to_string(),
            format_tunnel_by_name_specs(&cli.tunnel_by_name),
        );
    }
    envs.insert(
        "RUST_LOG".to_string(),
        rust_log.unwrap_or_else(|| "info".to_string()),
    );
    envs
}

fn render_launchd_plist(
    exe_path: &Path,
    log_out: &Path,
    log_err: &Path,
    envs: &BTreeMap<String, String>,
) -> Result<String> {
    let exe = xml_escape(&exe_path.to_string_lossy());
    let out = xml_escape(&log_out.to_string_lossy());
    let err = xml_escape(&log_err.to_string_lossy());

    let mut env_entries = String::new();
    for (key, value) in envs {
        env_entries.push_str(&format!(
            "    <key>{}</key>\n    <string>{}</string>\n",
            xml_escape(key),
            xml_escape(value)
        ));
    }

    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key>\n  <string>{SERVICE_LABEL}</string>\n\n  <key>ProgramArguments</key>\n  <array>\n    <string>{exe}</string>\n  </array>\n\n  <key>RunAtLoad</key>\n  <true/>\n  <key>KeepAlive</key>\n  <true/>\n\n  <key>EnvironmentVariables</key>\n  <dict>\n{env_entries}  </dict>\n\n  <key>StandardOutPath</key>\n  <string>{out}</string>\n  <key>StandardErrorPath</key>\n  <string>{err}</string>\n</dict>\n</plist>\n"
    ))
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escape_escapes_special_chars() {
        let raw = "<&>'\"";
        assert_eq!(xml_escape(raw), "&lt;&amp;&gt;&apos;&quot;");
    }

    #[test]
    fn systemd_template_contains_execstart() {
        let unit = render_systemd_service(Path::new("/tmp/punchgate"));
        assert!(unit.contains("ExecStart=/tmp/punchgate"));
        assert!(unit.contains("EnvironmentFile=%h/.config/punchgate/punchgate.env"));
    }
}
