#![cfg_attr(windows, windows_subsystem = "windows")]

mod adapter;
mod admin;
mod gui;
mod proxy;
mod single_instance;
mod tray;
mod update;
mod vpn;

use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand, ValueEnum};
use tokio_util::sync::CancellationToken;

use crate::adapter::EgressTarget;
use crate::proxy::ProxyConfig;
use crate::vpn::{DnsStrategy, VpnConfig};

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Launch the desktop GUI.
    Gui,
    /// List usable network adapter addresses.
    List {
        /// Print JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Start only the weighted SOCKS5 proxy.
    Proxy {
        /// Address the SOCKS5 proxy listens on.
        #[arg(long, default_value = "127.0.0.1")]
        bind: IpAddr,
        /// Port the SOCKS5 proxy listens on.
        #[arg(long, default_value_t = 1080)]
        port: u16,
        /// Egress adapter IP with optional weight, for example 192.168.1.20/2.
        #[arg(long = "egress", required = true)]
        egress: Vec<String>,
        /// Disable SOCKS5 UDP ASSOCIATE support.
        #[arg(long)]
        no_udp: bool,
    },
    /// Start the proxy and launch the local TUN sidecar.
    Vpn {
        /// Address the SOCKS5 proxy listens on.
        #[arg(long, default_value = "127.0.0.1")]
        bind: IpAddr,
        /// Port the SOCKS5 proxy listens on.
        #[arg(long, default_value_t = 1080)]
        port: u16,
        /// Egress adapter IP with optional weight, for example 192.168.1.20/2.
        #[arg(long = "egress", required = true)]
        egress: Vec<String>,
        /// Path to tun2proxy-bin if it is not bundled beside net-combiner.
        #[arg(long)]
        tun2proxy: Option<PathBuf>,
        /// Do not ask tun2proxy to configure routes automatically.
        #[arg(long)]
        no_setup: bool,
        /// Add a CIDR/IP bypass passed to tun2proxy.
        #[arg(long = "bypass")]
        bypass: Vec<String>,
        /// DNS handling strategy passed to tun2proxy.
        #[arg(long, value_enum, default_value_t = CliDnsStrategy::Virtual)]
        dns: CliDnsStrategy,
        /// Enable IPv6 routing in tun2proxy.
        #[arg(long)]
        ipv6: bool,
        /// Disable SOCKS5 UDP ASSOCIATE support.
        #[arg(long)]
        no_udp: bool,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliDnsStrategy {
    Virtual,
    OverTcp,
    Direct,
}

impl From<CliDnsStrategy> for DnsStrategy {
    fn from(value: CliDnsStrategy) -> Self {
        match value {
            CliDnsStrategy::Virtual => DnsStrategy::Virtual,
            CliDnsStrategy::OverTcp => DnsStrategy::OverTcp,
            CliDnsStrategy::Direct => DnsStrategy::Direct,
        }
    }
}

fn main() -> Result<()> {
    admin::ensure_elevated()?;

    let cli = Cli::parse();
    let is_gui = matches!(&cli.command, None | Some(Command::Gui));
    let _single_instance: Option<single_instance::SingleInstanceGuard> = if is_gui {
        let guard = single_instance::acquire_or_notify()?;
        if guard.is_none() {
            return Ok(());
        }
        guard
    } else {
        None
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "net_combiner=info".into()),
        )
        .init();

    match cli.command.unwrap_or(Command::Gui) {
        Command::Gui => gui::run_gui(),
        Command::List { json } => list_adapters(json),
        Command::Proxy {
            bind,
            port,
            egress,
            no_udp,
        } => run_proxy_foreground(bind, port, egress, !no_udp),
        Command::Vpn {
            bind,
            port,
            egress,
            tun2proxy,
            no_setup,
            bypass,
            dns,
            ipv6,
            no_udp,
        } => run_vpn_foreground(
            bind,
            port,
            egress,
            tun2proxy,
            !no_setup,
            bypass,
            dns.into(),
            ipv6,
            !no_udp,
        ),
    }
}

fn list_adapters(json: bool) -> Result<()> {
    let adapters = adapter::list_adapters()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&adapters)?);
        return Ok(());
    }

    println!(
        "{:<32} {:<45} {:<8} {:<10}",
        "Name", "Address", "Family", "Scope"
    );
    for item in adapters {
        println!(
            "{:<32} {:<45} {:<8} {:<10}",
            trim_for_table(&item.name, 32),
            item.ip,
            item.family_label(),
            item.scope_label()
        );
    }
    Ok(())
}

fn trim_for_table(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        value.to_owned()
    } else {
        let mut text: String = value.chars().take(width.saturating_sub(3)).collect();
        text.push_str("...");
        text
    }
}

fn run_proxy_foreground(
    bind: IpAddr,
    port: u16,
    egress: Vec<String>,
    udp_enabled: bool,
) -> Result<()> {
    let config = ProxyConfig {
        listen_ip: bind,
        listen_port: port,
        egress: parse_egress_args(&egress)?,
        udp_enabled,
    };
    let (tx, rx) = mpsc::channel();
    spawn_log_printer(rx);

    let runtime = tokio::runtime::Runtime::new().context("failed to create Tokio runtime")?;
    runtime.block_on(async move {
        let cancel = CancellationToken::new();
        let server_cancel = cancel.clone();
        let server =
            tokio::spawn(async move { proxy::run_proxy(config, server_cancel, tx, None).await });

        tokio::signal::ctrl_c()
            .await
            .context("failed to listen for Ctrl+C")?;
        cancel.cancel();

        server.await.context("proxy task panicked")??;
        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
fn run_vpn_foreground(
    bind: IpAddr,
    port: u16,
    egress: Vec<String>,
    tun2proxy: Option<PathBuf>,
    setup_routes: bool,
    bypass: Vec<String>,
    dns: DnsStrategy,
    ipv6: bool,
    udp_enabled: bool,
) -> Result<()> {
    let proxy_config = ProxyConfig {
        listen_ip: bind,
        listen_port: port,
        egress: parse_egress_args(&egress)?,
        udp_enabled,
    };
    let vpn_config = VpnConfig {
        tun2proxy_path: tun2proxy,
        setup_routes,
        bypass,
        dns_strategy: dns,
        enable_ipv6: ipv6,
    };

    let (tx, rx) = mpsc::channel();
    spawn_log_printer(rx);

    let runtime = tokio::runtime::Runtime::new().context("failed to create Tokio runtime")?;
    runtime.block_on(async move {
        let cancel = CancellationToken::new();
        let server_cancel = cancel.clone();
        let proxy_log = tx.clone();
        let server = tokio::spawn(async move {
            proxy::run_proxy(proxy_config, server_cancel, proxy_log, None).await
        });

        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        let mut vpn = vpn::VpnProcess::start(port, vpn_config, tx.clone())
            .context("failed to start TUN sidecar")?;

        tokio::signal::ctrl_c()
            .await
            .context("failed to listen for Ctrl+C")?;
        let _ = vpn.stop();
        cancel.cancel();
        server.await.context("proxy task panicked")??;
        Ok(())
    })
}

fn parse_egress_args(values: &[String]) -> Result<Vec<EgressTarget>> {
    let mut targets = Vec::new();
    for value in values {
        let (ip_text, weight) = match value.rsplit_once('/') {
            Some((ip_text, weight_text)) => {
                let weight = weight_text
                    .parse::<u16>()
                    .with_context(|| format!("invalid egress weight in {value}"))?;
                (ip_text, weight.max(1))
            }
            None => (value.as_str(), 1),
        };
        let ip = ip_text
            .parse::<IpAddr>()
            .with_context(|| format!("invalid egress IP address in {value}"))?;
        targets.push(EgressTarget {
            name: ip.to_string(),
            ip,
            weight,
        });
    }

    if targets.is_empty() {
        return Err(anyhow!("at least one --egress adapter address is required"));
    }

    Ok(targets)
}

fn spawn_log_printer(rx: mpsc::Receiver<String>) {
    thread::spawn(move || {
        while let Ok(line) = rx.recv() {
            eprintln!("{line}");
        }
    });
}
