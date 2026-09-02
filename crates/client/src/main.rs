//! `lynxrdp` command line client.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use lynxrdp_client::app::{App, AppOptions};
use lynxrdp_client::connection::{Client, ConnectOptions};
use lynxrdp_client::tunnel::{parse_destination, RemoteTarget, Tunnel, TunnelConfig};

/// Connect to a LynxRDP session over SSH.
#[derive(Parser, Debug)]
#[command(name = "lynxrdp", version, about, long_about = None)]
struct Args {
    /// SSH destination: [user@]host[:port] or a ~/.ssh/config alias.
    destination: Option<String>,
    /// SSH port (overrides a port given in the destination).
    #[arg(short = 'p', long)]
    port: Option<u16>,
    /// SSH identity file.
    #[arg(short = 'i', long)]
    identity: Option<PathBuf>,
    /// Extra ssh -o option (repeatable), e.g. -o ProxyJump=bastion
    #[arg(short = 'o', long = "ssh-option")]
    ssh_options: Vec<String>,
    /// Path of the ssh executable.
    #[arg(long, default_value = "ssh")]
    ssh: String,
    /// LynxRDP port on the remote host's loopback interface.
    #[arg(long, default_value_t = lynxrdp_proto::DEFAULT_PORT)]
    remote_port: u16,
    /// Forward to a Unix socket on the remote host instead of a TCP port.
    #[arg(long)]
    remote_socket: Option<String>,
    /// Local port for the tunnel (default: a free port).
    #[arg(long, default_value_t = 0)]
    local_port: u16,
    /// Connect directly to an already tunnelled loopback address and skip
    /// starting ssh (e.g. 127.0.0.1:3390). Only loopback addresses are accepted.
    #[arg(long, conflicts_with = "destination")]
    connect: Option<SocketAddr>,
    /// Initial remote screen size, e.g. 1920x1080 (default: server default).
    #[arg(long, value_parser = parse_size)]
    size: Option<(u16, u16)>,
    /// Start in fullscreen (toggle with Ctrl+Alt+Enter).
    #[arg(short = 'f', long)]
    fullscreen: bool,
    /// Keep the remote screen size fixed instead of following the window.
    #[arg(long)]
    no_dynamic_resize: bool,
    /// Do not synchronise the clipboard.
    #[arg(long)]
    no_clipboard: bool,
    /// Seconds to wait for the tunnel (ssh may prompt for credentials).
    #[arg(long, default_value_t = 120)]
    tunnel_timeout: u64,
}

fn parse_size(s: &str) -> Result<(u16, u16), String> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| "expected WIDTHxHEIGHT".to_string())?;
    let w: u16 = w.parse().map_err(|_| "bad width".to_string())?;
    let h: u16 = h.parse().map_err(|_| "bad height".to_string())?;
    if w < 64 || h < 64 {
        return Err("size must be at least 64x64".into());
    }
    Ok((w, h))
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();
    if let Err(e) = run() {
        eprintln!("lynxrdp: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = Args::parse();
    let (addr, _tunnel) = match (&args.connect, &args.destination) {
        (Some(addr), _) => {
            if !addr.ip().is_loopback() {
                bail!(
                    "--connect only accepts loopback addresses; use an SSH tunnel for remote hosts"
                );
            }
            (*addr, None)
        }
        (None, Some(dest)) => {
            let (destination, port_in_dest) = parse_destination(dest)?;
            let cfg = TunnelConfig {
                destination,
                ssh_port: args.port.or(port_in_dest),
                identity: args.identity.clone(),
                options: args.ssh_options.clone(),
                remote: match &args.remote_socket {
                    Some(p) => RemoteTarget::Socket(p.clone()),
                    None => RemoteTarget::Port(args.remote_port),
                },
                local_port: args.local_port,
                ssh_program: args.ssh.clone(),
                extra_args: Vec::new(),
            };
            let tunnel = Tunnel::open(&cfg, Duration::from_secs(args.tunnel_timeout))?;
            (tunnel.local_addr(), Some(tunnel))
        }
        (None, None) => bail!("usage: lynxrdp [user@]host  (see --help)"),
    };

    let (waker, slot) = lynxrdp_client::app::make_waker();
    let opts = ConnectOptions {
        size: args.size,
        ..Default::default()
    };
    let client =
        Client::connect(addr, &opts, Some(waker)).context("connecting to the LynxRDP server")?;
    log::info!(
        "connected to {} as {} (session {}), screen {}x{}",
        client.info().server_name,
        client.info().username,
        client.info().session_id,
        client.size().0,
        client.size().1
    );
    let app_opts = AppOptions {
        fullscreen: args.fullscreen,
        title: "LynxRDP".to_string(),
        dynamic_resize: !args.no_dynamic_resize,
        clipboard: !args.no_clipboard,
    };
    let reason = App::run(client, app_opts, slot)?;
    if let Some(r) = reason {
        log::info!("session closed: {r}");
    }
    Ok(())
}
